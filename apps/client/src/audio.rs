//! 音频 I/O 的两半：上行（采集 → 处理链 → Opus）与下行（混音器 → 播放）。
//!
//! # 线程与缓冲的分工
//!
//! ```text
//! cpal 采集回调 ──rtrb──▶ 主循环 on_tick：480 样本/帧 → 处理链 → Opus → UDP
//!                              │
//!                              └─ 每个说话人一条 ReceivePath（抖动缓冲+漂移补偿）
//!                                        │
//!                                        └──rtrb──▶ 混音器 ──▶ cpal 播放回调
//! ```
//!
//! 采集侧与播放侧不对称，这是有意的：
//!
//! - **上行**在环上缓冲**整帧**：环里有几帧就多几帧的延迟，所以环浅（16 帧）而每 tick 最多取 2 帧，
//!   设备时钟与定时器差出来的那点漂移靠「偶尔一次取两帧」吸收；
//! - **下行**不额外缓冲：混音器**就在播放回调里**（[`audio_io::SampleSource`] 这条接缝），
//!   所以 PCM 从抖动缓冲出来就直接进设备，中间没有第二个环。若把混音放到别的线程再经环递给
//!   回调，就凭空多 10–20 ms —— 这正是 C-12 里那种「莫名多出来的一段」。
//!
//! # 48 kHz 是硬前提
//!
//! 链路固定 48 kHz / 10 ms / 480 样本（`audio-core::FRAME_SAMPLES`），Opus 也是按这个配的。
//! 设备不给 48 kHz 时**明确报错**并列出设备支持的区间，而不是偷偷重采样：
//! 重采样要额外一环、要自己算延迟，v1 不做（`rubato` 已在依赖里，留给后续）。

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Sender;

use audio_core::mix::TrackSource;
use audio_core::receive::PcmReceiver;
use audio_core::{
    AudioCodec, Chain, ChainDesc, FRAME_SAMPLES, Frame, MixControl, MixStats, Mixer, OpusCodec,
    SAMPLE_RATE,
};
use audio_io::{
    CaptureHandle, Devices, PlayoutHandle, RateRange, SampleSource, StreamEvent, StreamFormat,
};

/// 采集环深度（帧）。占用不等于延迟，但环太深会把「跟不上」藏起来。
const CAPTURE_RING_FRAMES: usize = 16;

/// 混音器的实时计数（播放回调里只写原子量，不打日志、不分配）。
#[derive(Debug, Default)]
pub struct MixCounters {
    /// 混过的帧数。
    pub frames: AtomicU64,
    /// 有轨取不到数据的帧数（抖动缓冲饿了）。
    pub underruns: AtomicU64,
    /// 求和后超过满量程的样本数（会被软限幅压住，不会硬削）。
    pub clipped: AtomicU64,
    /// 软限幅之前的峰值（f32 的位模式）。
    peak_bits: AtomicU32,
}

impl MixCounters {
    fn record(&self, stats: MixStats) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        if stats.underruns > 0 {
            self.underruns.fetch_add(1, Ordering::Relaxed);
        }
        if stats.clipped_samples > 0 {
            self.clipped
                .fetch_add(u64::from(stats.clipped_samples), Ordering::Relaxed);
        }
        let bits = stats.peak.to_bits();
        let mut current = self.peak_bits.load(Ordering::Relaxed);
        while bits > current {
            match self.peak_bits.compare_exchange_weak(
                current,
                bits,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    /// 峰值（线性）。
    #[must_use]
    pub fn peak(&self) -> f32 {
        f32::from_bits(self.peak_bits.load(Ordering::Relaxed))
    }
}

/// 在混音器取数据的那条路上量电平。
///
/// 为什么要包一层：混音器的统计只有**全局**峰值，而界面要的是「每个人多大声」。
/// 唯一能拿到逐轨数据的地方就是 [`TrackSource::pull_frame`] —— 它跑在播放回调
/// （音频线程）里，所以这里只做一次 fold 和一个原子写，不分配、不打日志、不锁。
pub struct MeteredSource {
    inner: PcmReceiver,
    peak_bits: Arc<AtomicU32>,
}

impl MeteredSource {
    /// 包一层，返回音源本身和读电平的把手。
    #[must_use]
    pub fn new(inner: PcmReceiver) -> (Self, LevelMeter) {
        let peak_bits = Arc::new(AtomicU32::new(0));
        (
            Self {
                inner,
                peak_bits: Arc::clone(&peak_bits),
            },
            LevelMeter { peak_bits },
        )
    }
}

impl TrackSource for MeteredSource {
    fn pull_frame(&mut self, out: &mut Frame) -> bool {
        let has_data = self.inner.pull_frame(out);
        // 非负 f32 的位模式与数值同序，所以「按位取最大」等价于「按值取最大」。
        let peak = out
            .iter()
            .fold(0.0_f32, |acc, sample| acc.max(sample.abs()));
        self.peak_bits.fetch_max(peak.to_bits(), Ordering::Relaxed);
        has_data
    }
}

/// 读电平的把手：`take()` 返回「上次读以来」的峰值，并清零。
///
/// 这个语义正好对上界面的 20 Hz 推送：每次推送读一次，读到的是这 50 ms 内最响的那一帧，
/// 不需要自己算衰减，也不会漏掉一帧。
#[derive(Debug, Clone)]
pub struct LevelMeter {
    peak_bits: Arc<AtomicU32>,
}

impl LevelMeter {
    /// 取走上次读以来的峰值（0–1）。
    #[must_use]
    pub fn take(&self) -> f32 {
        f32::from_bits(self.peak_bits.swap(0, Ordering::Relaxed))
    }
}

/// 把混音器包成播放回调的音源。
struct MixerSource {
    mixer: Mixer,
    frame: Frame,
    cursor: usize,
    counters: Arc<MixCounters>,
}

impl SampleSource for MixerSource {
    fn next_sample(&mut self) -> Option<f32> {
        if self.cursor >= FRAME_SAMPLES {
            let stats = self.mixer.mix(&mut self.frame);
            self.counters.record(stats);
            self.cursor = 0;
        }
        let sample = self.frame[self.cursor];
        self.cursor += 1;
        Some(sample)
    }
}

/// 上行：采集环 + 处理链 + 编码器。
pub struct Capture {
    /// 必须排在最前：drop 时先停流，环再消失，避免回调往已释放的环里写。
    handle: CaptureHandle,
    ring: rtrb::Consumer<f32>,
    chain: Chain,
    codec: OpusCodec,
    frames: u64,
    starved: u64,
}

impl Capture {
    /// 开麦克风。
    pub fn start(
        device: Option<&str>,
        chain_spec: &str,
        events: Sender<StreamEvent>,
    ) -> Result<Self, String> {
        let desc = ChainDesc::parse(chain_spec).map_err(|e| e.to_string())?;
        let chain = Chain::from_desc(&desc).map_err(|e| e.to_string())?;
        let codec = OpusCodec::with_defaults().map_err(|e| e.to_string())?;
        let (producer, ring) = rtrb::RingBuffer::new(CAPTURE_RING_FRAMES * FRAME_SAMPLES);
        let handle = audio_io::start_capture(device, producer, events, None, None)
            .map_err(|e| e.to_string())?;
        require_48k(&handle.format, device, true, "麦克风")?;
        Ok(Self {
            handle,
            ring,
            chain,
            codec,
            frames: 0,
            starved: 0,
        })
    }

    /// 取一整帧并跑处理链；环里不足一帧时返回 `None`（记一次饿死）。
    ///
    /// 返回值里带上处理链给的 VAD 概率 —— 上行的门控（`audio_core::TxGate`）靠它
    /// 区分「说话」和「风扇」。`1.0` 表示这条链不提供 VAD（见 `Stage::process`），
    /// 那时门控退化成只看能量，不是「一直发」。
    pub fn pull(&mut self) -> Option<(Frame, f32)> {
        if self.ring.slots() < FRAME_SAMPLES {
            self.starved += 1;
            return None;
        }
        let mut frame: Frame = [0.0; FRAME_SAMPLES];
        for slot in &mut frame {
            match self.ring.pop() {
                Ok(sample) => *slot = sample,
                Err(_) => {
                    self.starved += 1;
                    return None;
                }
            }
        }
        // 处理链的返回值是这一帧的 VAD 概率。以前这里把它丢掉（DTX 走峰值判定），
        // 现在它是语音激活的门控判据：丢在这里就等于没有门控。
        let vad = self.chain.process(&mut frame);
        self.frames += 1;
        Some((frame, vad))
    }

    /// 编码一帧，返回写入 `out` 的字节数（0 = 编码失败）。
    pub fn encode(&mut self, frame: &Frame, out: &mut [u8]) -> usize {
        self.codec.encode(frame, out)
    }

    /// 处理链引入的算法延迟（毫秒），进汇总报告。
    #[must_use]
    pub fn chain_latency_ms(&self) -> f32 {
        self.chain.total_latency_ms()
    }

    /// 采集侧计数：`(已取帧, 饿死次数, 环满丢弃样本数, 峰值)`。
    #[must_use]
    pub fn stats(&self) -> (u64, u64, u64, f32) {
        (
            self.frames,
            self.starved,
            self.handle.stats.dropped_samples.load(Ordering::Relaxed),
            self.handle.stats.peak(),
        )
    }

    /// 实际打开的格式。
    #[must_use]
    pub fn format(&self) -> StreamFormat {
        self.handle.format
    }
}

/// 下行：混音器（在播放回调里）+ 各路音源的控制端。
pub struct Playout {
    handle: PlayoutHandle,
    mix: MixControl,
    counters: Arc<MixCounters>,
}

impl Playout {
    /// 开扬声器。混音器直接搬进回调，所以这里返回的控制端是给主循环挂轨用的。
    pub fn start(device: Option<&str>, events: Sender<StreamEvent>) -> Result<Self, String> {
        let (mixer, mix) = Mixer::new();
        let counters = Arc::new(MixCounters::default());
        let source = MixerSource {
            mixer,
            frame: [0.0; FRAME_SAMPLES],
            cursor: FRAME_SAMPLES,
            counters: Arc::clone(&counters),
        };
        let handle = audio_io::start_playout_source(
            device,
            Box::new(source),
            events,
            None,
            None,
            SAMPLE_RATE,
        )
        .map_err(|e| e.to_string())?;
        require_48k(&handle.format, device, false, "扬声器")?;
        Ok(Self {
            handle,
            mix,
            counters,
        })
    }

    /// 混音器控制端（挂轨 / 调音量 / 静音）。
    #[must_use]
    pub const fn mix(&self) -> &MixControl {
        &self.mix
    }

    /// 实时计数。
    #[must_use]
    pub const fn counters(&self) -> &Arc<MixCounters> {
        &self.counters
    }

    /// 实际打开的格式。
    #[must_use]
    pub fn format(&self) -> StreamFormat {
        self.handle.format
    }
}

/// 合成正弦音源：没有麦克风（或 CI 里根本没有声卡）时，用它验证整条链路。
///
/// 它的存在不是为了好听，而是为了让「加密 → 网络 → 解密 → 抖动缓冲 → 混音」这一段**可断言**：
/// 两端都跑这个，服务端的 fanout 与接收端的 frames_in 都必须大于 0。
pub struct Tone {
    phase: f32,
    step: f32,
}

impl Tone {
    /// `hz` 是频率。幅度固定 −12 dBFS，避免一上来就触发软限幅。
    #[must_use]
    pub fn new(hz: f32) -> Self {
        Self {
            phase: 0.0,
            step: 2.0 * std::f32::consts::PI * hz / SAMPLE_RATE as f32,
        }
    }

    /// 生成一帧。永不返回 `None`：合成音源不会饿。
    pub fn pull(&mut self) -> Frame {
        let mut frame: Frame = [0.0; FRAME_SAMPLES];
        for sample in &mut frame {
            *sample = 0.25 * self.phase.sin();
            self.phase += self.step;
            if self.phase > std::f32::consts::TAU {
                self.phase -= std::f32::consts::TAU;
            }
        }
        frame
    }
}

/// 设备清单（含每个设备接受的采样率区间）。
pub fn devices() -> Result<String, String> {
    let Devices { inputs, outputs } = audio_io::list_devices().map_err(|e| e.to_string())?;
    let mut text = String::new();
    for (title, list, input) in [
        ("输入（麦克风）", &inputs, true),
        ("输出（扬声器）", &outputs, false),
    ] {
        text.push_str(title);
        text.push('\n');
        if list.is_empty() {
            text.push_str("  （没有设备）\n");
            continue;
        }
        for device in list {
            let ranges = audio_io::rate_ranges(Some(&device.name), input).unwrap_or_default();
            text.push_str(&format!(
                "  {}{}\n    {}\n",
                if device.is_default { "* " } else { "  " },
                device.name,
                describe_rates(&ranges),
            ));
        }
    }
    Ok(text)
}

/// 把采样率区间压成人能读的一句话（设备清单与设置页共用）。
#[must_use]
pub fn describe_rates(ranges: &[RateRange]) -> String {
    if ranges.is_empty() {
        return "采样率：读不出来".to_owned();
    }
    // 同一个采样率会因为「声道数 × 采样格式」的每种组合各出现一次（实测一个 DAC 能报出上百条），
    // 直接打印既读不了也没信息量。先按区间去重、排序，再压缩成人能读的一句话。
    let mut unique: Vec<RateRange> = Vec::new();
    for range in ranges {
        if !unique
            .iter()
            .any(|seen| seen.min == range.min && seen.max == range.max)
        {
            unique.push(*range);
        }
    }
    unique.sort_unstable_by_key(|range| (range.min, range.max));

    let text = if unique.iter().all(|range| range.min == range.max) {
        // 全是单点：列出来最有信息量（能直接看出「有没有 48 kHz」）。
        unique
            .iter()
            .map(|range| range.min.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        // 有真正的区间：只给总范围，逐条列会很长且没有额外信息。
        let min = unique.iter().map(|range| range.min).min().unwrap_or(0);
        let max = unique.iter().map(|range| range.max).max().unwrap_or(0);
        format!("{min}–{max}")
    };
    let mark = if unique.iter().any(|range| range.covers(SAMPLE_RATE)) {
        "（支持 48 kHz）"
    } else {
        "（不支持 48 kHz —— 语音链路用不了）"
    };
    format!("采样率：{text} {mark}")
}
/// 设备必须给 48 kHz，否则整条链路都不成立（帧长、Opus、抖动缓冲都按 48 kHz 配的）。
fn require_48k(
    format: &StreamFormat,
    device: Option<&str>,
    input: bool,
    what: &str,
) -> Result<(), String> {
    if format.sample_rate == SAMPLE_RATE {
        return Ok(());
    }
    let ranges = audio_io::rate_ranges(device, input).unwrap_or_default();
    Err(format!(
        "{what}打开的是 {} Hz，而语音链路固定 {SAMPLE_RATE} Hz。这个设备支持的区间：{}。\n\
         用 --list-devices 挑一个支持 48 kHz 的设备，再用 --mic / --speaker 指定。",
        format.sample_rate,
        describe_rates(&ranges),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 常数音源：验证「混音器 → 播放回调」这条新接缝真的在出声。
    struct Constant(f32);

    impl audio_core::TrackSource for Constant {
        fn pull_frame(&mut self, out: &mut Frame) -> bool {
            out.fill(self.0);
            true
        }
    }

    #[test]
    fn the_mixer_source_emits_audio_and_runs_the_mixer_once_per_frame() {
        // 这条接缝是 P5-7 新加的（`audio_io::SampleSource`），也是唯一没有硬件也能测的部分：
        // 如果它接错了，客户端会「一切正常但没声音」，而那是最难查的故障。
        let (mixer, control) = Mixer::new();
        control.add_track(0, Box::new(Constant(0.5))).expect("挂轨");
        let counters = Arc::new(MixCounters::default());
        let mut source = MixerSource {
            mixer,
            frame: [0.0; FRAME_SAMPLES],
            cursor: FRAME_SAMPLES,
            counters: Arc::clone(&counters),
        };

        let mut peak = 0.0_f32;
        for _ in 0..FRAME_SAMPLES {
            let sample = source.next_sample().expect("混音器永远有样本");
            peak = peak.max(sample.abs());
        }
        assert!(
            (peak - 0.5).abs() < 1e-6,
            "混音器应当输出 0.5 的常数，实际峰值 {peak}"
        );
        assert_eq!(
            counters.frames.load(Ordering::Relaxed),
            1,
            "480 个样本只该触发一次 mix()"
        );
    }

    #[test]
    fn repeated_ranges_collapse_into_one_readable_line() {
        // 真实设备会把同一个采样率按「声道数 × 采样格式」重复报很多次。
        let ranges = [RateRange {
            min: 48_000,
            max: 48_000,
        }; 64];
        let text = describe_rates(&ranges);
        assert_eq!(text, "采样率：48000 （支持 48 kHz）");
        let wide = describe_rates(&[
            RateRange {
                min: 44_100,
                max: 192_000,
            },
            RateRange {
                min: 48_000,
                max: 48_000,
            },
        ]);
        assert!(
            wide.contains("44100–192000"),
            "有真区间时只给总范围：{wide}"
        );
    }

    #[test]
    fn rate_description_calls_out_missing_48k() {
        let good = describe_rates(&[RateRange {
            min: 44_100,
            max: 192_000,
        }]);
        assert!(good.contains("支持 48 kHz"));
        let bad = describe_rates(&[RateRange {
            min: 44_100,
            max: 44_100,
        }]);
        assert!(bad.contains("不支持"));
        assert!(describe_rates(&[]).contains("读不出来"));
    }
}
