//! 桌面环回工具（P2-2）：采集 → `rtrb` → 播放，可选串 Opus；`--impulse` 测物理环回延迟。
//!
//! 为什么必须有 `--impulse`：这是唯一不靠假设就能拿到 D1 真值的手段（技术设计 §11.5 方法 3）。
//! 没有它，就只能相信「cpal 说 480 帧就是 10 ms」。
//!
//! 本工具是 P2 期间唯一允许打印的地方。

#![allow(clippy::print_stdout)]

mod exclusive;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use audio_core::{AudioCodec, FRAME_SAMPLES, MAX_PACKET_BYTES, OpusCodec, OpusConfig};
use audio_io::{StreamEvent, StreamFormat};
use serde::Serialize;

const USAGE: &str = "\
桌面环回工具（P2）\n\
\n\
用法：\n\
  loopback --list-devices\n\
  loopback [--input <名字>] [--output <名字>] [--seconds 10] [--ring-ms 10]\n\
           [--codec none|opus:10ms:32k] [--report out/loopback.json]\n\
  loopback --impulse [--period-ms 500] [--seconds 10] [--report out/impulse.json]\n\
  loopback --impulse --exclusive [--buffer 480] [--report out/exclusive.json]\n\
\n\
说明：\n\
  --ring-ms   播放/采集环形缓冲长度，直接进环回延迟，默认 50 ms\n\
  --buffer    设备缓冲（帧，如 128/256/480）；不给就用设备默认。调小可降延迟，代价是更容易 xrun\n\
  --amplitude 脉冲振幅（0–0.95），默认 0.7；怕吵就先给 0.2 探一下\n\
  --single    只发一个脉冲（在 0.5 s 处）：没有周期歧义，结果唯一，推荐先用它\n\
  --repeat    连测 N 次并给出中位数与极差（默认 1）；重复性比单次读数更能说明问题\n\
  --dump      把录音写成 WAV（排查「两个峰」这类问题时唯一能定案的手段）\n\
  --exclusive 播放改走 WASAPI 独占模式：绕过音频引擎与 APO（本机实测 5 个），只支持默认播放端点\n\
  --impulse   播放已知脉冲串并用麦克风录回，匹配滤波求环回延迟（外放时才有声学路径）\n";

/// 一次会话的统计。
#[derive(Debug, Clone, Default, Serialize)]
struct SessionReport {
    mode: String,
    input: Option<String>,
    output: Option<String>,
    capture_format: Option<FormatReport>,
    playout_format: Option<FormatReport>,
    ring_samples: usize,
    seconds: f64,
    capture_dropped_samples: u64,
    playout_underrun_samples: u64,
    stream_errors: u64,
    xrun_events: u64,
    restarts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    delay_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delay_lag_samples: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    correlation: Option<f32>,
    /// 抛物线插值后的亚样本位移（毫秒换算用它，`delay_ms` 已含）。
    #[serde(skip_serializing_if = "Option::is_none")]
    delay_fractional_lag: Option<f64>,
    /// 逐脉冲各自最佳位移的极差：单次测量内部的重复性。
    #[serde(skip_serializing_if = "Option::is_none")]
    click_spread_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uncertainty_ms: Option<f64>,
    /// `--repeat` 的连测次数；只有 1 次时不写。
    #[serde(skip_serializing_if = "Option::is_none")]
    repeats: Option<usize>,
    /// 连测读数的极差（毫秒）：这才是能拿去签字的重复性。
    #[serde(skip_serializing_if = "Option::is_none")]
    delay_spread_ms: Option<f64>,
    /// 每次的读数（按运行顺序），便于看出有没有单次离群。
    #[serde(skip_serializing_if = "Option::is_none")]
    run_delays_ms: Option<Vec<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capture_peak: Option<f32>,
    /// 实际推给播放设备的样本数（impulse 模式）。为 0 就说明根本没喂进去。
    #[serde(skip_serializing_if = "Option::is_none")]
    emitted_samples: Option<usize>,
    /// 检测信噪比（峰值 ÷ 曲线中位数）：判断「到底有没有测到脉冲」的判据。
    #[serde(skip_serializing_if = "Option::is_none")]
    detection_snr: Option<f32>,
    /// 相关法给出的位移。含流启动错位（实测 0.4–1.4 s），**不能当延迟读**。
    #[serde(skip_serializing_if = "Option::is_none")]
    delay_ms_correlation: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    opus_frames: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
struct FormatReport {
    sample_rate: u32,
    channels: u16,
    /// 请求的设备缓冲（帧）；`null` 表示用设备默认。
    requested_buffer_frames: Option<u32>,
    /// 设备实际缓冲（帧）。
    ///
    /// 共享模式下 WASAPI 只把请求值当建议（实测给 128 帧，回调周期照样是设备周期），
    /// 独占模式下它就是真值，所以只有独占模式才写这个字段。
    #[serde(skip_serializing_if = "Option::is_none")]
    actual_buffer_frames: Option<u32>,
    /// 设备实际采样格式。
    sample_format: String,
}

impl From<StreamFormat> for FormatReport {
    fn from(value: StreamFormat) -> Self {
        Self {
            sample_rate: value.sample_rate,
            channels: value.channels,
            requested_buffer_frames: value.requested_buffer_frames,
            actual_buffer_frames: None,
            sample_format: format!("{:?}", value.sample_format),
        }
    }
}

/// 播放侧的两种实现：cpal 共享模式（默认）与 WASAPI 独占模式（`--exclusive`）。
///
/// 用枚举而不是 `trait`：只有两处调用点，动态分派在这里没有任何好处，
/// 而枚举强迫每个分支都被编译器检查一遍（新加一个后端不会漏改）。
enum Playout {
    Shared(audio_io::PlayoutHandle),
    Exclusive(exclusive::ExclusiveRender),
}

impl Playout {
    fn sample_rate(&self) -> u32 {
        match self {
            Self::Shared(handle) => handle.format.sample_rate,
            Self::Exclusive(render) => render.format.sample_rate,
        }
    }

    fn format_report(&self) -> FormatReport {
        match self {
            Self::Shared(handle) => FormatReport::from(handle.format),
            Self::Exclusive(render) => FormatReport {
                sample_rate: render.format.sample_rate,
                channels: render.format.channels,
                requested_buffer_frames: None,
                actual_buffer_frames: Some(render.format.buffer_frames),
                sample_format: format!("{} (独占)", render.format.sample_format),
            },
        }
    }

    fn stats(&self) -> &audio_io::PlayoutStats {
        match self {
            Self::Shared(handle) => &handle.stats,
            Self::Exclusive(render) => &render.stats,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Codec {
    None,
    Opus,
}

struct Args {
    list_devices: bool,
    input: Option<String>,
    output: Option<String>,
    seconds: f64,
    ring_ms: f64,
    buffer: Option<u32>,
    impulse: bool,
    exclusive: bool,
    /// 采集改成在**播放设备**上开回环（WASAPI loopback）：摘掉扬声器、空气和麦克风，
    /// 只量「播放侧 + 数字回环」。
    loopback: bool,
    /// 仅诊断用：把独占渲染强行开到指定采样率（与采集不一致时延迟读数作废）。
    exclusive_rate: Option<u32>,
    /// 独占模式的取用方式：auto（先事件后轮询）/ event / poll。
    exclusive_mode: exclusive::ExclusiveMode,
    /// 共享模式下的渲染采样率（默认跟随采集）。
    ///
    /// 用途：回环采集被锁在端点混音格式（本机 192 kHz），把它设成 48 kHz 就能单独量出
    /// 「引擎 48→192 重采样」到底值多少毫秒——这是那 ~141 ms 唯一还没排除的嫌疑。
    render_rate: Option<u32>,
    single: bool,
    repeat: usize,
    period_ms: f64,
    amplitude: f32,
    codec: Codec,
    report: Option<PathBuf>,
    dump: Option<PathBuf>,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut parsed = Args {
        list_devices: false,
        input: None,
        output: None,
        seconds: 10.0,
        ring_ms: 50.0,
        buffer: None,
        impulse: false,
        exclusive: false,
        loopback: false,
        exclusive_rate: None,
        exclusive_mode: exclusive::ExclusiveMode::Auto,
        render_rate: None,
        single: false,
        repeat: 1,
        period_ms: 500.0,
        amplitude: 0.7,
        codec: Codec::None,
        report: None,
        dump: None,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--list-devices" => parsed.list_devices = true,
            "--input" => parsed.input = Some(take(args, &mut i, "--input")?),
            "--output" => parsed.output = Some(take(args, &mut i, "--output")?),
            "--seconds" => {
                parsed.seconds = take(args, &mut i, "--seconds")?
                    .parse()
                    .map_err(|e| format!("--seconds 不是数字：{e}"))?;
            }
            "--ring-ms" => {
                parsed.ring_ms = take(args, &mut i, "--ring-ms")?
                    .parse()
                    .map_err(|e| format!("--ring-ms 不是数字：{e}"))?;
            }
            "--period-ms" => {
                parsed.period_ms = take(args, &mut i, "--period-ms")?
                    .parse()
                    .map_err(|e| format!("--period-ms 不是数字：{e}"))?;
            }
            "--buffer" => {
                let frames: u32 = take(args, &mut i, "--buffer")?
                    .parse()
                    .map_err(|e| format!("--buffer 不是整数：{e}"))?;
                parsed.buffer = Some(frames);
            }
            "--amplitude" => {
                parsed.amplitude = take(args, &mut i, "--amplitude")?
                    .parse()
                    .map_err(|e| format!("--amplitude 不是数字：{e}"))?;
            }
            "--impulse" => parsed.impulse = true,
            "--exclusive" => parsed.exclusive = true,
            "--loopback" => parsed.loopback = true,
            "--render-rate" => {
                parsed.render_rate = Some(
                    take(args, &mut i, "--render-rate")?
                        .parse()
                        .map_err(|e| format!("--render-rate 不是整数：{e}"))?,
                );
            }
            "--exclusive-mode" => {
                let value = take(args, &mut i, "--exclusive-mode")?;
                parsed.exclusive_mode = match value.as_str() {
                    "auto" => exclusive::ExclusiveMode::Auto,
                    "event" => exclusive::ExclusiveMode::Event,
                    "poll" => exclusive::ExclusiveMode::Poll,
                    other => {
                        return Err(format!(
                            "未知 --exclusive-mode {other:?}（auto / event / poll）"
                        ));
                    }
                };
            }
            "--exclusive-rate" => {
                parsed.exclusive_rate = Some(
                    take(args, &mut i, "--exclusive-rate")?
                        .parse()
                        .map_err(|e| format!("--exclusive-rate 不是整数：{e}"))?,
                );
            }
            "--single" => parsed.single = true,
            "--repeat" => {
                parsed.repeat = take(args, &mut i, "--repeat")?
                    .parse::<usize>()
                    .map_err(|e| format!("--repeat 不是正整数：{e}"))?
                    .max(1);
            }
            "--codec" => {
                let spec = take(args, &mut i, "--codec")?;
                parsed.codec = if spec == "none" {
                    Codec::None
                } else if spec.starts_with("opus") {
                    Codec::Opus
                } else {
                    return Err(format!("未知 codec {spec:?}（none / opus:10ms:32k）"));
                };
            }
            "--report" => parsed.report = Some(PathBuf::from(take(args, &mut i, "--report")?)),
            "--dump" => parsed.dump = Some(PathBuf::from(take(args, &mut i, "--dump")?)),
            other => return Err(format!("不认识参数 {other:?}")),
        }
        i += 1;
    }
    Ok(parsed)
}

fn take(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} 缺少取值"))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // 只有「参数不对」才打印用法；设备/格式这类运行期错误只报错，别刷一屏帮助。
    let parsed = match parse_args(&args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("错误：{message}");
            eprintln!();
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    match execute(&parsed) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("错误：{message}");
            ExitCode::FAILURE
        }
    }
}

fn execute(parsed: &Args) -> Result<(), String> {
    if parsed.list_devices {
        let devices = audio_io::list_devices().map_err(|e| e.to_string())?;
        println!("输入设备：");
        for device in &devices.inputs {
            println!(
                "  {}{}",
                device.name,
                if device.is_default { "  (默认)" } else { "" }
            );
        }
        println!("输出设备：");
        for device in &devices.outputs {
            println!(
                "  {}{}",
                device.name,
                if device.is_default { "  (默认)" } else { "" }
            );
        }
        return Ok(());
    }

    let report = if parsed.impulse {
        if parsed.repeat > 1 {
            run_impulse_repeated(parsed)?
        } else {
            run_impulse(parsed)?
        }
    } else {
        run_passthrough(parsed)?
    };

    let json = serde_json::to_string_pretty(&report).map_err(|e| format!("序列化报告失败：{e}"))?;
    match &parsed.report {
        Some(path) => {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("建目录 {} 失败：{e}", parent.display()))?;
            }
            std::fs::write(path, &json).map_err(|e| format!("写报告失败：{e}"))?;
            println!("报告已写入 {}", path.display());
        }
        None => println!("{json}"),
    }
    Ok(())
}

/// 采集直通播放（可选串 Opus）。
fn run_passthrough(args: &Args) -> Result<SessionReport, String> {
    let mut attempt = 0_u32;
    loop {
        let report = session_passthrough(args)?;
        // 只有「设备真的没了」才重建；xrun 只是抖了一下，重建反而丢更多音频。
        if report.stream_errors == 0 || attempt >= 2 {
            return Ok(SessionReport {
                restarts: attempt,
                ..report
            });
        }
        // P2-4：设备报错后重建整条流（rtrb 端点在回调里，重建流只能连缓冲一起重建）。
        attempt += 1;
        eprintln!("流报错，重建第 {attempt} 次…");
    }
}

fn session_passthrough(args: &Args) -> Result<SessionReport, String> {
    let ring_samples = ring_capacity(args);
    let (tx, rx) = mpsc::channel();

    let (capture, playout, stop, codec_frames) = match args.codec {
        Codec::None => {
            let (producer, consumer) = rtrb::RingBuffer::<f32>::new(ring_samples);
            let capture = audio_io::start_capture(
                args.input.as_deref(),
                producer,
                tx.clone(),
                args.buffer,
                None,
            )
            .map_err(|e| e.to_string())?;
            let playout = audio_io::start_playout(
                args.output.as_deref(),
                consumer,
                tx,
                args.buffer,
                None,
                audio_io::PREFERRED_SAMPLE_RATE,
            )
            .map_err(|e| e.to_string())?;
            (capture, playout, None, None)
        }
        Codec::Opus => {
            let (producer_a, consumer_a) = rtrb::RingBuffer::<f32>::new(ring_samples);
            let (producer_b, consumer_b) = rtrb::RingBuffer::<f32>::new(ring_samples);
            let capture = audio_io::start_capture(
                args.input.as_deref(),
                producer_a,
                tx.clone(),
                args.buffer,
                None,
            )
            .map_err(|e| e.to_string())?;
            let playout = audio_io::start_playout(
                args.output.as_deref(),
                consumer_b,
                tx,
                args.buffer,
                None,
                audio_io::PREFERRED_SAMPLE_RATE,
            )
            .map_err(|e| e.to_string())?;
            if !capture.is_48k() || !playout.is_48k() {
                return Err(format!(
                    "Opus 路径要求 48 kHz（采集 {} Hz / 播放 {} Hz）；先换 48 kHz 设备或去掉 --codec",
                    capture.format.sample_rate, playout.format.sample_rate
                ));
            }
            let stop = Arc::new(AtomicBool::new(false));
            let frames = Arc::new(AtomicU64::new(0));
            spawn_opus_worker(
                consumer_a,
                producer_b,
                Arc::clone(&stop),
                Arc::clone(&frames),
            );
            (capture, playout, Some(stop), Some(frames))
        }
    };

    wait(args.seconds, &rx);

    if let Some(stop) = &stop {
        stop.store(true, Ordering::Relaxed);
    }
    let report = SessionReport {
        mode: match args.codec {
            Codec::None => "passthrough".to_owned(),
            Codec::Opus => "opus".to_owned(),
        },
        input: args.input.clone(),
        output: args.output.clone(),
        capture_format: Some(capture.format.into()),
        playout_format: Some(playout.format.into()),
        ring_samples,
        seconds: args.seconds,
        capture_dropped_samples: capture.stats.dropped_samples.load(Ordering::Relaxed),
        playout_underrun_samples: playout.stats.underrun_samples.load(Ordering::Relaxed),
        stream_errors: capture.stats.fatal_errors.load(Ordering::Relaxed)
            + playout.stats.fatal_errors.load(Ordering::Relaxed),
        xrun_events: capture.stats.xrun_events.load(Ordering::Relaxed)
            + playout.stats.xrun_events.load(Ordering::Relaxed),
        restarts: 0,
        delay_ms: None,
        delay_lag_samples: None,
        correlation: None,
        delay_fractional_lag: None,
        click_spread_ms: None,
        uncertainty_ms: None,
        repeats: None,
        delay_spread_ms: None,
        run_delays_ms: None,
        capture_peak: Some(capture.stats.peak()),
        emitted_samples: None,
        detection_snr: None,
        delay_ms_correlation: None,
        opus_frames: codec_frames.map(|frames| frames.load(Ordering::Relaxed)),
    };

    // 显式丢弃句柄，确保流在统计之后才停。
    drop(capture);
    drop(playout);
    Ok(report)
}

fn ring_capacity(args: &Args) -> usize {
    let samples = (args.ring_ms / 1000.0 * f64::from(audio_io::PREFERRED_SAMPLE_RATE)) as usize;
    samples.max(FRAME_SAMPLES)
}

fn wait(seconds: f64, events: &mpsc::Receiver<StreamEvent>) {
    let deadline = Instant::now() + Duration::from_secs_f64(seconds.max(0.0));
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(100)) {
            Ok(StreamEvent::Fatal { kind, message }) => eprintln!("[致命] {kind}: {message}"),
            Ok(StreamEvent::Warning { kind, message }) => eprintln!("[xrun] {kind}: {message}"),
            Err(_) => {}
        }
    }
}

fn spawn_opus_worker(
    mut from_capture: rtrb::Consumer<f32>,
    mut to_playout: rtrb::Producer<f32>,
    stop: Arc<AtomicBool>,
    frames: Arc<AtomicU64>,
) {
    std::thread::spawn(move || {
        let mut codec = match OpusCodec::new(OpusConfig::default()) {
            Ok(codec) => codec,
            Err(err) => {
                eprintln!("Opus 初始化失败：{err}");
                return;
            }
        };
        let mut frame = [0.0_f32; FRAME_SAMPLES];
        let mut decoded = [0.0_f32; FRAME_SAMPLES];
        let mut packet = [0_u8; MAX_PACKET_BYTES];

        while !stop.load(Ordering::Relaxed) {
            // 攒满一帧；不够就先睡 1 ms（这里不是实时线程，允许睡眠）。
            let mut filled = 0;
            while filled < FRAME_SAMPLES {
                match from_capture.pop() {
                    Ok(sample) => {
                        frame[filled] = sample;
                        filled += 1;
                    }
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(1));
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                    }
                }
            }

            let len = codec.encode(&frame, &mut packet);
            if len == 0 || !codec.decode(&packet[..len], false, &mut decoded) {
                continue;
            }
            frames.fetch_add(1, Ordering::Relaxed);

            let mut pushed = 0;
            while pushed < FRAME_SAMPLES {
                if to_playout.push(decoded[pushed]).is_ok() {
                    pushed += 1;
                } else {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    });
}

/// `--impulse`：推已知脉冲串，同时录音，互相关求环回延迟。
fn run_impulse(args: &Args) -> Result<SessionReport, String> {
    // Windows 默认定时器精度 15.6 ms：`sleep(1ms)` 会睡成 15 ms。轮询模式按 15 ms 的节奏
    // 补数据喂不动 6–10 ms 的设备缓冲，pump_impulse 也会供料不足（实测 3 s 欠载上千样本）。
    // 整段测量期间把精度提到 1 ms，离开作用域自动还原。
    let _timer = exclusive::TimerResolution::raise();
    let ring_samples = ring_capacity(args);
    let (tx, rx) = mpsc::channel();
    // 录音侧单独放大到 20 倍（默认 200 ms）：抽干是「每轮循环一次」，Windows 上两轮之间可能有 15 ms，
    // 10 ms 的环必然溢出——那不是设备问题，是我们自己把缓冲开小了。
    let capture_ring = ring_samples.saturating_mul(20);
    let (capture_producer, capture_consumer) = rtrb::RingBuffer::<f32>::new(capture_ring);
    let (playout_producer, playout_consumer) = rtrb::RingBuffer::<f32>::new(ring_samples);
    // 时间戳通道：输入/输出各一条，回调里无锁写入，跑完再抽出来算延迟。
    let (stamp_out_tx, mut stamp_out_rx) = rtrb::RingBuffer::<audio_io::CallbackStamp>::new(8_192);
    let (stamp_in_tx, mut stamp_in_rx) = rtrb::RingBuffer::<audio_io::CallbackStamp>::new(8_192);

    // --loopback：采集改成在播放设备上开回环，把声学与麦克风整段摘掉。
    let capture = if args.loopback {
        audio_io::start_loopback_capture(
            args.input.as_deref().or(args.output.as_deref()),
            capture_producer,
            tx.clone(),
            args.buffer,
            Some(stamp_in_tx),
        )
    } else {
        audio_io::start_capture(
            args.input.as_deref(),
            capture_producer,
            tx.clone(),
            args.buffer,
            Some(stamp_in_tx),
        )
    }
    .map_err(|e| e.to_string())?;
    let sample_rate = capture.format.sample_rate;
    // 渲染采样率：独占模式用 --exclusive-rate 强行指定（诊断「设备在不在消费」）；
    // 共享模式用 --render-rate（诊断「引擎重采样值多少毫秒」）；都不给就跟随采集。
    let render_rate = args
        .exclusive_rate
        .or(args.render_rate)
        .unwrap_or(sample_rate);
    // 两种不一致要分开处理：
    // - 独占 + --exclusive-rate：只为看设备在不在消费，延迟读数作废；
    // - --render-rate：是「引擎重采样值多少毫秒」的正经实验，读数有效（录音会抽取后分析）。
    let exclusive_rate_mismatch = args.exclusive && render_rate != sample_rate;
    let rate_mismatch = render_rate != sample_rate;
    let playout = if args.exclusive {
        Playout::Exclusive(exclusive::start(
            playout_consumer,
            stamp_out_tx,
            tx,
            render_rate,
            args.buffer,
            args.output.clone(),
            args.exclusive_mode,
        )?)
    } else {
        Playout::Shared(
            audio_io::start_playout(
                args.output.as_deref(),
                playout_consumer,
                tx,
                args.buffer,
                Some(stamp_out_tx),
                // 回环模式下采集被锁在端点混音格式（本机 192 kHz），而 --render-rate 可以
                // 让它故意不同——那正是「引擎重采样值多少毫秒」的实验。
                render_rate,
            )
            .map_err(|e| e.to_string())?,
        )
    };

    if playout.sample_rate() != sample_rate {
        if !rate_mismatch {
            return Err(format!(
                "采集与播放采样率不一致（{} / {} Hz），先换同一设备族再测",
                sample_rate,
                playout.sample_rate()
            ));
        }
        if exclusive_rate_mismatch {
            eprintln!(
                "⚠ 诊断模式：渲染 {} Hz 与采集 {} Hz 不一致，延迟读数作废，只看「已填缓冲」有没有增长",
                playout.sample_rate(),
                sample_rate
            );
        } else {
            eprintln!(
                "渲染 {} Hz / 采集 {} Hz：录音会抽取到渲染采样率再分析（用来量引擎重采样的代价）",
                playout.sample_rate(),
                sample_rate
            );
        }
    }

    if let Playout::Exclusive(render) = &playout {
        println!(
            "独占模式：{} 声道 {} Hz {}，缓冲 {} 帧（{:.2} ms），设备周期 {} 帧，GetStreamLatency {:.2} ms",
            render.format.channels,
            render.format.sample_rate,
            render.format.sample_format,
            render.format.buffer_frames,
            f64::from(render.format.buffer_frames) * 1000.0 / f64::from(sample_rate),
            render.format.period_frames,
            render.format.stream_latency_ms,
        );
        println!("  格式协商：{}", render.format.negotiation);
    }

    // 发射序列必须按**渲染**采样率生成：它要按设备的节奏播出去。
    let total = (args.seconds.max(0.1) * f64::from(render_rate)) as usize;
    let amplitude = args.amplitude.clamp(0.0, 0.95);
    // 单次脉冲：周期取整段长度 ⇒ 只有一个脉冲；再旋到 0.5 s 处，前后都留余量。
    // 周期脉冲串做互相关有「延迟 mod 周期」的歧义，实测会在 4 / 23 / 443 ms 之间乱跳。
    let (period, train) = if args.single {
        let mut single = audio_io::impulse::click_train(total, total, render_rate, amplitude);
        let shift = ((0.5 * f64::from(sample_rate)) as usize).min(total.saturating_sub(1));
        single.rotate_right(shift);
        (total, single)
    } else {
        let period = ((args.period_ms / 1000.0 * f64::from(render_rate)) as usize).max(1);
        (
            period,
            audio_io::impulse::click_train(total, period, render_rate, amplitude),
        )
    };

    let stop = Arc::new(AtomicBool::new(false));
    let (emitted_len, recorded, min_occupancy, max_occupancy) = pump_impulse(
        capture_consumer,
        playout_producer,
        &train,
        sample_rate,
        Arc::clone(&stop),
        args.seconds,
        ring_samples,
    );
    stop.store(true, Ordering::Relaxed);

    // 填充次数放在这里看才有意义：开流瞬间还是 0，跑完才知道渲染线程到底有没有被喂动。
    if let Playout::Exclusive(render) = &playout {
        let fills = render.fills.load(Ordering::Relaxed);
        // 只报次数与上限：轮询模式下每次能拿到的可写帧数并不等于整块，
        // 「次数 × 缓冲帧数」会算出一个比实际大好几倍的假总量。
        println!(
            "  已填缓冲 {fills} 次（每次最多 {} 帧）",
            render.format.buffer_frames
        );
        // 一次都没填 = 设备根本没消费。此时再往下算延迟只会得到一个假的「没测到脉冲」，
        // 必须把真正的原因喊出来，否则下一次还会有人以为是自己麦克风没开。
        if fills == 0 {
            return Err(format!(
                "独占流建起来了但设备一次都没消费：{} 声道 {} Hz {}、{} 帧缓冲，Start 返回成功，但设备时钟 position 恒为 0、padding 恒等于缓冲大小。\n同机对照：无 APO 的端点（TOPPING USB DAC）独占正常（6 ms 缓冲、2 秒内填充 313 次）；挂 APO 的端点（扬声器 与 Realtek Digital Output，各 5 个 APO：RltkAPOU64 + dtsapo3core64）全部如此。\n结论：这条端点上的 APO 链让 WASAPI 独占模式静默失效。换端点，或关掉该端点的音效增强后重试",
                render.format.channels,
                render.format.sample_rate,
                render.format.sample_format,
                render.format.buffer_frames
            ));
        }
    }
    if exclusive_rate_mismatch {
        return Err("诊断模式结束：渲染与采集采样率不一致，不产出延迟读数".to_owned());
    }

    // 抽出两边的时间戳（回调里只写，不读，避免任何阻塞）。
    let mut out_stamps: Vec<audio_io::CallbackStamp> = Vec::new();
    while let Ok(stamp) = stamp_out_rx.pop() {
        out_stamps.push(stamp);
    }
    let mut in_stamps: Vec<audio_io::CallbackStamp> = Vec::new();
    while let Ok(stamp) = stamp_in_rx.pop() {
        in_stamps.push(stamp);
    }

    if let Some(path) = &args.dump {
        write_wav(path, &recorded, sample_rate)?;
        println!("录音已写入 {}", path.display());
    }

    // 把流事件打印出来（设备被拔、格式失效都会走这里），别让它们静默消失。
    while let Ok(event) = rx.try_recv() {
        match event {
            StreamEvent::Fatal { kind, message } => eprintln!("[致命] {kind}: {message}"),
            StreamEvent::Warning { kind, message } => eprintln!("[xrun] {kind}: {message}"),
        }
    }

    // 渲染与采集采样率不同时（回环采集被锁在端点混音格式），把录音按整数倍抽取回渲染采样率，
    // 这样发射序列与录音落在同一个采样率上，匹配滤波才有意义。
    let ratio = if sample_rate == render_rate {
        1
    } else if sample_rate % render_rate == 0 {
        (sample_rate / render_rate) as usize
    } else {
        return Err(format!(
            "采集 {sample_rate} Hz 与渲染 {render_rate} Hz 不是整数倍关系，没法对齐分析"
        ));
    };
    let analysis = if ratio == 1 {
        recorded.clone()
    } else {
        decimate(&recorded, ratio)
    };

    let estimate = audio_io::impulse::estimate_delay(
        &analysis,
        &train[..emitted_len.min(train.len())],
        period,
        render_rate,
    );

    // 时间戳通道：把「发出去的那个样本」与「录到的那个样本」各自换算成回调墙钟再相减。
    // 采集侧丢过样本时下标就对不上了，直接放弃这个值。
    let dropped = capture.stats.dropped_samples.load(Ordering::Relaxed);
    let stamp_result = estimate.filter(|_| dropped == 0).and_then(|estimate| {
        stamp_delay_of(
            &estimate,
            &out_stamps,
            &in_stamps,
            render_rate,
            sample_rate,
            ratio,
        )
    });
    let stamp_delay = stamp_result.map(|(delay, _, _)| delay);
    if dropped > 0 {
        eprintln!("采集侧丢了 {dropped} 个样本，录音下标与流序号不再对应，时间戳通道作废");
    }

    // 不确定度 = 播放环占用的**波动范围** + 一个回调缓冲。
    // 恒定占用只是把延迟整体平移（已知量），波动才会让单个 lag 对不齐。
    let jitter = max_occupancy.saturating_sub(min_occupancy.min(max_occupancy));
    let uncertainty_samples = jitter + FRAME_SAMPLES;
    let report = SessionReport {
        mode: "impulse".to_owned(),
        input: args.input.clone(),
        output: args.output.clone(),
        capture_format: Some(capture.format.into()),
        playout_format: Some(playout.format_report()),
        ring_samples,
        seconds: args.seconds,
        capture_dropped_samples: capture.stats.dropped_samples.load(Ordering::Relaxed),
        playout_underrun_samples: playout.stats().underrun_samples.load(Ordering::Relaxed),
        stream_errors: capture.stats.fatal_errors.load(Ordering::Relaxed)
            + playout.stats().fatal_errors.load(Ordering::Relaxed),
        xrun_events: capture.stats.xrun_events.load(Ordering::Relaxed)
            + playout.stats().xrun_events.load(Ordering::Relaxed),
        restarts: 0,
        // 主值走时间戳：不含我们自己的环形缓冲占用，也不含流启动错位。
        delay_ms: stamp_delay,
        delay_lag_samples: estimate.map(|e| e.lag_samples),
        correlation: estimate.map(|e| e.correlation),
        detection_snr: estimate.map(|e| e.detection_snr),
        delay_fractional_lag: estimate.map(|e| e.fractional_lag),
        click_spread_ms: estimate.map(|e| e.click_spread_ms),
        // 相关法的绝对值含流启动错位（实测 0.4–1.4 s），只能当诊断。
        delay_ms_correlation: estimate.map(|e| e.delay_ms),
        repeats: None,
        delay_spread_ms: None,
        run_delays_ms: None,
        uncertainty_ms: Some(uncertainty_samples as f64 * 1000.0 / f64::from(render_rate)),
        capture_peak: Some(capture.stats.peak()),
        emitted_samples: Some(emitted_len),
        opus_frames: None,
    };

    let underruns = playout.stats().underrun_samples.load(Ordering::Relaxed);
    drop(capture);
    drop(playout);

    println!(
        "已推送 {:.2} s 脉冲到播放设备（欠载 {} 样本），录音 {:.2} s",
        emitted_len as f64 / f64::from(render_rate),
        underruns,
        recorded.len() as f64 / f64::from(sample_rate)
    );

    // 时间戳自检：`stream_nanos` 是设备承诺的时刻，`wall_nanos` 是回调真正跑起来的时刻。
    // 两者之差就是「这一批样本离真正播放/采集还有多久」，也就是设备缓冲深度。
    // QPC 与 crate 内 Instant 纪元不同源，不先对齐就会印出 +3900356 ms 这种读不出的数。
    let epoch_ns = exclusive::qpc_epoch_offset_ns().unwrap_or(0);
    let out_offset = median_offset(&out_stamps, epoch_ns);
    let in_offset = median_offset(&in_stamps, epoch_ns);
    println!(
        "时间戳：输出 {} 条（playback−回调 {:+.1} ms），输入 {} 条（capture−回调 {:+.1} ms）",
        out_stamps.len(),
        out_offset,
        in_stamps.len(),
        in_offset
    );

    if let Some(estimate) = estimate {
        print_stamp_pair(&estimate, &out_stamps, &in_stamps, epoch_ns, ratio);
    }
    if let Some((_, t_play, t_capture)) = stamp_result {
        let base = out_stamps
            .first()
            .map(|stamp| stamp.wall_nanos)
            .unwrap_or(0)
            .min(
                in_stamps
                    .first()
                    .map(|stamp| stamp.wall_nanos)
                    .unwrap_or(u64::MAX),
            );
        println!(
            "  时间轴（相对最早一条回调）：播放点击 +{:.1} ms，采集点击 +{:.1} ms，差 {:.1} ms",
            (t_play - base) as f64 / 1_000_000.0,
            (t_capture - base) as f64 / 1_000_000.0,
            (t_capture - t_play) as f64 / 1_000_000.0
        );
    }

    match (report.delay_ms, estimate) {
        (Some(delay), Some(estimate)) => {
            println!("环回延迟 {delay:.3} ms（时间戳法：播放回调 → 采集回调，含两端设备缓冲）");
            println!(
                "  匹配滤波：相关度 {:.3}，检测信噪比 {:.0}×，{} 个脉冲折叠，逐脉冲极差 {:.3} ms",
                estimate.correlation,
                estimate.detection_snr,
                estimate.clicks,
                estimate.click_spread_ms
            );
            println!(
                "  参考：相关位移 {:.1} ms（含流启动错位，不能当延迟读）",
                estimate.delay_ms
            );
        }
        (Some(delay), None) => {
            println!("环回延迟 {delay:.3} ms（时间戳法；匹配滤波没找到脉冲，结果不可信）");
        }
        (None, Some(estimate)) => {
            eprintln!(
                "时间戳通道没给出结果（录音或时间戳不完整）；相关位移 {:.1} ms 含流启动错位，不能当延迟读",
                estimate.delay_ms
            );
        }
        (None, None) => {
            eprintln!(
                "没测到脉冲：录音峰值 {:.4}（−60 dBFS 以上才算有声）。检查麦克风是否静音、输出是否真的接到了这台设备的输入",
                report.capture_peak.unwrap_or(0.0)
            );
            let curve = audio_io::impulse::correlation_curve(
                &recorded,
                &train[..emitted_len],
                period,
                sample_rate,
            );
            let mut peaks: Vec<(usize, f32)> = curve.iter().copied().enumerate().collect();
            peaks.sort_by(|a, b| b.1.total_cmp(&a.1));
            eprintln!("分数最高的 5 个位移（判断是没收到还是算法没对上）：");
            for (lag, score) in peaks.iter().take(5) {
                eprintln!(
                    "  {:.1} ms  score {:.3e}",
                    *lag as f64 * 1000.0 / f64::from(sample_rate),
                    score
                );
            }
        }
    }
    Ok(report)
}

/// --repeat：连测 N 次，报中位数与极差。
///
/// 单次读数证明不了任何事——旧实现三次给出 11.8 / 4.0 / 9.5 ms，任何一次拿出来看都像「合格」。
/// 能不能签字取决于**极差**，所以把重复性做成工具的一部分，而不是靠人记得多跑几遍。
fn run_impulse_repeated(args: &Args) -> Result<SessionReport, String> {
    let mut runs: Vec<SessionReport> = Vec::with_capacity(args.repeat);
    let mut readings: Vec<f64> = Vec::with_capacity(args.repeat);
    for index in 0..args.repeat {
        println!("—— 第 {}/{} 次 ——", index + 1, args.repeat);
        let report = run_impulse(args)?;
        if let Some(delay) = report.delay_ms {
            readings.push(delay);
        }
        runs.push(report);
    }
    let mut merged = runs
        .last()
        .cloned()
        .ok_or_else(|| "没有跑任何一次".to_owned())?;
    if readings.is_empty() {
        eprintln!("{0} 次都没测出延迟，无法给出中位数", args.repeat);
        return Ok(merged);
    }

    let mut sorted = readings.clone();
    sorted.sort_by(f64::total_cmp);
    let median = sorted[sorted.len() / 2];
    let spread = sorted[sorted.len() - 1] - sorted[0];

    // 代表值取「离中位数最近的那一次」，这样 lag / 相关度 / 逐脉冲极差与中位数自洽，
    // 而不是「最后一次碰巧是什么就写什么」。
    if let Some(representative) = runs
        .iter()
        .filter(|run| run.delay_ms.is_some())
        .min_by(|a, b| {
            let left = (a.delay_ms.unwrap_or(0.0) - median).abs();
            let right = (b.delay_ms.unwrap_or(0.0) - median).abs();
            left.total_cmp(&right)
        })
        .cloned()
    {
        merged = representative;
    }
    merged.delay_ms = Some(median);
    merged.repeats = Some(args.repeat);
    merged.delay_spread_ms = Some(spread);
    // 不确定度取「连测极差」与「单次逐脉冲极差」的较大者：前者管可重复性，后者管单次内部一致。
    let click_spread = merged.click_spread_ms.unwrap_or(0.0);
    merged.uncertainty_ms = Some(spread.max(click_spread));
    merged.run_delays_ms = Some(readings.clone());

    println!();
    println!(
        "{0} 次读数：{1:?}",
        args.repeat,
        readings
            .iter()
            .map(|value| format!("{value:.3}"))
            .collect::<Vec<_>>()
    );
    println!(
        "中位数 {:.3} ms，极差 {:.3} ms（最小 {:.3} / 最大 {:.3}）",
        median,
        spread,
        sorted[0],
        sorted[sorted.len() - 1]
    );
    Ok(merged)
}

/// 按整数倍抽取（box 平均）。
///
/// 为什么可以这么粗暴：脉冲是 1 ms 的 1 kHz 正弦，抽取后形状仍然可用；
/// 这里只关心时移，不关心高频细节。
fn decimate(samples: &[f32], ratio: usize) -> Vec<f32> {
    let divisor = ratio as f32;
    samples
        .chunks_exact(ratio)
        .map(|chunk| chunk.iter().sum::<f32>() / divisor)
        .collect()
}

/// 把录音写成 32-bit float 单声道 WAV。
///
/// 为什么需要它：相关法给出「两个几乎等高的峰」时（0.3 ms 与 766 ms），
/// 只看数字无法判断哪个是直达声、哪个是伪峰，必须看波形本身。
fn write_wav(path: &PathBuf, samples: &[f32], sample_rate: u32) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("建目录 {} 失败：{e}", parent.display()))?;
    }
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer =
        hound::WavWriter::create(path, spec).map_err(|e| format!("建 WAV 失败：{e}"))?;
    for sample in samples {
        writer
            .write_sample(*sample)
            .map_err(|e| format!("写 WAV 失败：{e}"))?;
    }
    writer.finalize().map_err(|e| format!("收尾 WAV 失败：{e}"))
}

/// 把「推入环形缓冲的样本序号」换算成「回调墙钟」时间（纳秒）。
///
/// 关键：推入序号 ≠ 流序号。流启动时会先播若干静音、欠载时又会插静音，
/// 所以必须用 `real_frames` 累积出「推入序号 ↔ 流序号」的映射，否则会算出一个
/// 随欠载量漂移的假延迟（实测偏了 150–200 ms）。
fn play_time_of(
    stamps: &[audio_io::CallbackStamp],
    train_index: u64,
    sample_rate: u32,
) -> Option<u64> {
    let mut consumed = 0_u64;
    for stamp in stamps {
        let next = consumed + stamp.real_frames;
        if train_index >= consumed && train_index < next {
            let stream_index = stamp.first_index + (train_index - consumed);
            return Some(
                stamp.wall_nanos
                    + (stream_index - stamp.first_index) * 1_000_000_000 / u64::from(sample_rate),
            );
        }
        consumed = next;
    }
    None
}

/// 把流内样本序号换算成「回调墙钟」时间（纳秒）。
///
/// 不用 cpal 的 `stream_nanos`：实测输入/输出两路原点差出上百毫秒，直接相减会得到一个假延迟。
/// 回调墙钟来自同一个 `Instant` 纪元，两路天然可比。
fn wall_time_of(stamps: &[audio_io::CallbackStamp], index: u64, sample_rate: u32) -> Option<u64> {
    stamps.iter().find_map(|stamp| {
        if index < stamp.first_index || index >= stamp.first_index + stamp.frames {
            return None;
        }
        Some(
            stamp.wall_nanos + (index - stamp.first_index) * 1_000_000_000 / u64::from(sample_rate),
        )
    })
}

/// 找出包含 `index` 的那条时间戳。
fn find_stamp(stamps: &[audio_io::CallbackStamp], index: u64) -> Option<&audio_io::CallbackStamp> {
    stamps
        .iter()
        .find(|stamp| index >= stamp.first_index && index < stamp.first_index + stamp.frames)
}

/// 把两端「设备承诺时刻」与「回调墙钟」的差打印出来：这就是设备缓冲深度。
///
/// 播放侧 `playback − callback` = 还要等多久才播到（含 WASAPI 引擎延迟）；
/// 采集侧 `capture − callback` = 这批样本是多久以前采到的（负值）。
fn print_stamp_pair(
    estimate: &audio_io::impulse::DelayEstimate,
    out_stamps: &[audio_io::CallbackStamp],
    in_stamps: &[audio_io::CallbackStamp],
    epoch_ns: i128,
    in_scale: usize,
) {
    let describe = |label: &str, stamp: Option<&audio_io::CallbackStamp>| match stamp {
        Some(stamp) => println!(
            "  {label} stamp：first_index {}，frames {}，real_frames {}，设备时刻−回调 {:+.2} ms",
            stamp.first_index,
            stamp.frames,
            stamp.real_frames,
            (i128::from(stamp.stream_nanos) - epoch_ns - i128::from(stamp.wall_nanos)) as f64
                / 1_000_000.0
        ),
        None => println!(
            "  {label} stamp：没找到（索引 {}）",
            if label == "播放" {
                estimate.emitted_click_index
            } else {
                estimate.recorded_click_index
            }
        ),
    };
    describe(
        "播放",
        find_stamp(out_stamps, estimate.emitted_click_index as u64),
    );
    // 录音下标是按渲染采样率算出来的，采集时间戳用的是采集采样率的下标。
    describe(
        "采集",
        find_stamp(
            in_stamps,
            estimate.recorded_click_index as u64 * in_scale as u64,
        ),
    );
}

/// 一批时间戳里 `stream_nanos − wall_nanos` 的中位数（毫秒）。
///
/// 输出侧是「还要等多久才真正播放」，输入侧是「这批样本是多久以前采到的」（负值）。
/// 两者相加就是设备缓冲总量；用它把「端到端延迟」拆成「设备缓冲」与「声学/驱动」两部分。
fn median_offset(stamps: &[audio_io::CallbackStamp], epoch_ns: i128) -> f64 {
    if stamps.is_empty() {
        return f64::NAN;
    }
    let mut offsets: Vec<f64> = stamps
        .iter()
        .map(|stamp| {
            (i128::from(stamp.stream_nanos) - epoch_ns - i128::from(stamp.wall_nanos)) as f64
                / 1_000_000.0
        })
        .collect();
    offsets.sort_by(f64::total_cmp);
    offsets[offsets.len() / 2]
}

/// 用两路回调时间戳算环回延迟：`采集回调墙钟 − 播放回调墙钟`。
///
/// 为什么这条路才是主值：
///
/// 1. 相关法的 lag 是「录音数组下标 − 发射数组下标」，而两个流**不是同时开始的**——
///    实测播放流的首个回调比采集流晚 0.4–1.4 s，这个错位直接进 lag，让绝对值完全不可用。
///    时间戳法把两个样本各自换算成同一根墙钟轴上的时刻，错位自动抵消。
/// 2. 播放侧用的是「这个样本被交给设备的时刻」，采集侧是「这个样本被交给我们的时刻」，
///    两者之差 = 两端设备缓冲 + 声学传播，正是端到端延迟要量的东西；
///    我们自己的环形缓冲占用（`pump_impulse` 里维持的半满）不在其中。
///
/// 前提：采集侧没有丢样本，这样「录音数组下标」才等于「采集流样本序号」。
/// 丢过就必须放弃这个值——宁可没有数，也不要一个偏移过的数。
fn stamp_delay_of(
    estimate: &audio_io::impulse::DelayEstimate,
    out_stamps: &[audio_io::CallbackStamp],
    in_stamps: &[audio_io::CallbackStamp],
    out_rate: u32,
    in_rate: u32,
    in_scale: usize,
) -> Option<(f64, u64, u64)> {
    let t_play = play_time_of(out_stamps, estimate.emitted_click_index as u64, out_rate)?;
    // 录音下标是按渲染采样率算的，换算回采集采样率的下标才能对上时间戳。
    let captured_index = estimate.recorded_click_index as u64 * in_scale as u64;
    let t_capture = wall_time_of(in_stamps, captured_index, in_rate)?;
    if t_capture <= t_play {
        return None;
    }
    Some(((t_capture - t_play) as f64 / 1_000_000.0, t_play, t_capture))
}

/// 推送脉冲串并录音，返回（已推送样本数, 录音）。
/// 按**墙钟时间**推进发射序列，同时把录音抽干。
///
/// 为什么不是「每次 sleep 1 ms 就推 1 ms 的量」：Windows 的 `sleep(1ms)` 实际睡 ~15 ms，
/// 那样推得比设备慢，播放侧会大面积欠载（实测 3 秒欠载 16 704 样本），测出来的延迟也没意义。
/// 改成「按已流逝时间算出应该推到哪」之后，粗粒度睡眠自动被补偿，环缓冲占用也保持很低。
/// 维持「半满」的播放环：既不欠载，也不把延迟堆到环满。
///
/// 为什么不能按墙钟硬推：环满时只能停推，于是「发射序号 ↔ 真实时间」不再是线性的，
/// 8 个脉冲就无法用同一个 lag 对齐，相关度会塌到 0.3 左右（实测就是这样）。
/// 改成「盯住占用量、缺多少补多少」之后，推送节奏自动跟随设备消费，映射重新变成线性。
///
/// 返回（已推送样本数, 录音, 占用最小值, 占用最大值）。
fn pump_impulse(
    mut from_capture: rtrb::Consumer<f32>,
    mut to_playout: rtrb::Producer<f32>,
    train: &[f32],
    sample_rate: u32,
    stop: Arc<AtomicBool>,
    seconds: f64,
    playout_capacity: usize,
) -> (usize, Vec<f32>, usize, usize) {
    let mut recorded: Vec<f32> = Vec::with_capacity((seconds * f64::from(sample_rate)) as usize);
    let mut emitted = 0_usize;
    let capacity = playout_capacity;
    let target_occupancy = capacity / 2;
    let mut min_occupancy = usize::MAX;
    let mut max_occupancy = 0_usize;
    let start = Instant::now();

    loop {
        if start.elapsed().as_secs_f64() >= seconds.max(0.1) || stop.load(Ordering::Relaxed) {
            break;
        }

        let occupancy = capacity.saturating_sub(to_playout.slots());
        let mut to_push = target_occupancy.saturating_sub(occupancy);
        while to_push > 0 && emitted < train.len() {
            if to_playout.push(train[emitted]).is_err() {
                break;
            }
            emitted += 1;
            to_push -= 1;
        }

        while let Ok(sample) = from_capture.pop() {
            recorded.push(sample);
        }

        min_occupancy = min_occupancy.min(occupancy);
        max_occupancy = max_occupancy.max(occupancy);
        std::thread::sleep(Duration::from_millis(1));
    }

    while let Ok(sample) = from_capture.pop() {
        recorded.push(sample);
    }
    (emitted, recorded, min_occupancy, max_occupancy)
}
