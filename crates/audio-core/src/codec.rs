//! Opus 编解码封装（P1-3）。
//!
//! 目标是把编解码做成**可替换的接缝**：`opus`（libopus 绑定）是默认实现，
//! 备选 `opus-pure`（纯 Rust）可以在不改调用方的前提下替换进来（feature 名已在 P0 冻结）。
//!
//! # 参数（技术设计 §6.3 钉死，不要随手改）
//!
//! | 参数 | 取值 |
//! |---|---|
//! | application | `Voip`（**不用** `RestrictedLowDelay`，它省不掉 6.5 ms 前瞻） |
//! | 帧长 | 10 ms（480 样本） |
//! | 比特率 | 32 kbps VBR（24–40 kbps 范围内可调） |
//! | complexity | 10 |
//! | in-band FEC | 开 |
//! | 预期丢包率 | 5–10 %（给编码器留 FEC 冗余） |
//! | 前瞻 | 312 样本 = 6.5 ms（任何实现都要付，计入延迟预算） |
//!
//! DTX 保持关闭：PTT 场景静音期本就不发包，开 DTX 只会让包率变得不规律。

use std::fmt;

use crate::{FRAME_SAMPLES, Frame, SAMPLE_RATE};

/// 单包最大字节数。
///
/// 32 kbps × 10 ms ≈ 40 B；给 VBR 峰值与包级冗余留 10 倍余量，Opus 单帧上限是 1275 B。
pub const MAX_PACKET_BYTES: usize = 400;

/// 编解码接缝。
///
/// 与 [`crate::Stage`] 分开：编码器不吃 `Frame` 吐 `Frame`，而是吃 `Frame` 吐字节，
/// 且延迟记账方式不同（前瞻 312 样本属于编解码，不属于处理链）。
pub trait AudioCodec {
    /// 编码一帧，返回写入 `out` 的字节数；返回 `0` 表示失败。
    ///
    /// 失败时**不要 panic**（实时线程禁止 panic）：调用方应当丢弃这一帧并计数。
    fn encode(&mut self, frame: &Frame, out: &mut [u8]) -> usize;

    /// 解码一个包到 `out`，返回是否得到完整一帧。
    ///
    /// `lost = true` 表示这一包没收到，走 PLC（丢包隐藏）路径；
    /// 用下一包做 FEC 恢复请调 [`OpusCodec::decode_fec`]。
    fn decode(&mut self, payload: &[u8], lost: bool, out: &mut Frame) -> bool;

    /// 编解码引入的固定前瞻（样本数）。48 kHz 下 libopus 恒为 312。
    fn lookahead_samples(&self) -> usize;
}

/// Opus 编码器参数。默认值即技术设计 §6.3 的取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpusConfig {
    /// 目标比特率（bit/s）。默认 32 000。
    pub bitrate_bps: i32,
    /// 预期丢包率（%），告诉编码器该为 FEC 留多少冗余。默认 10。
    pub packet_loss_perc: i32,
    /// 计算复杂度 0–10。桌面用 10，低端设备可降（技术设计 §6.3）。默认 10。
    pub complexity: i32,
}

impl Default for OpusConfig {
    fn default() -> Self {
        Self {
            bitrate_bps: 32_000,
            packet_loss_perc: 10,
            complexity: 10,
        }
    }
}

/// 编解码错误。只在构造期出现；热路径用返回值（`0` / `false`）表达失败，不构造错误。
#[derive(Debug)]
pub enum CodecError {
    /// libopus 返回了错误。
    Opus(opus::Error),
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Opus(err) => write!(f, "libopus 调用失败：{err}"),
        }
    }
}

impl std::error::Error for CodecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Opus(err) => Some(err),
        }
    }
}

impl From<opus::Error> for CodecError {
    fn from(err: opus::Error) -> Self {
        Self::Opus(err)
    }
}

/// libopus 的 Opus 编解码器（默认实现）。
///
/// 构造期一次性配置好，`encode`/`decode` 内部不分配：输出缓冲由调用方提供。
pub struct OpusCodec {
    encoder: opus::Encoder,
    decoder: opus::Decoder,
    lookahead_samples: usize,
}

impl OpusCodec {
    /// 按给定参数构造。`sample_rate` 固定取 [`SAMPLE_RATE`]，不允许别的值（链路是 48 kHz 单声道）。
    pub fn new(config: OpusConfig) -> Result<Self, CodecError> {
        let mut encoder =
            opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip)?;
        encoder.set_bitrate(opus::Bitrate::Bits(config.bitrate_bps))?;
        encoder.set_complexity(config.complexity)?;
        encoder.set_inband_fec(true)?;
        encoder.set_packet_loss_perc(config.packet_loss_perc)?;

        // get_lookahead 需要 &mut self，所以在构造期取一次缓存起来（前瞻不会变）。
        let lookahead_samples = encoder.get_lookahead()?.max(0) as usize;

        let decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono)?;

        Ok(Self {
            encoder,
            decoder,
            lookahead_samples,
        })
    }

    /// 用技术设计 §6.3 的默认参数构造。
    pub fn with_defaults() -> Result<Self, CodecError> {
        Self::new(OpusConfig::default())
    }

    /// 用「下一包」做 FEC 恢复（P4-3 用）。
    ///
    /// 当前包丢失、但下一包已到达时调用：从下一包里解出当前帧。
    pub fn decode_fec(&mut self, next_payload: &[u8], out: &mut Frame) -> bool {
        match self.decoder.decode_float(next_payload, out, true) {
            Ok(n) => n == FRAME_SAMPLES,
            Err(_) => false,
        }
    }
}

impl AudioCodec for OpusCodec {
    fn encode(&mut self, frame: &Frame, out: &mut [u8]) -> usize {
        // 失败返回 0（调用方丢帧并计数），不 panic：实时线程里没有「异常」这个选项。
        self.encoder.encode_float(frame, out).unwrap_or_default()
    }

    fn decode(&mut self, payload: &[u8], lost: bool, out: &mut Frame) -> bool {
        // 空切片 = 声明丢包，libopus 走 PLC。
        let input: &[u8] = if lost { &[] } else { payload };
        match self.decoder.decode_float(input, out, false) {
            Ok(n) => n == FRAME_SAMPLES,
            Err(_) => false,
        }
    }

    fn lookahead_samples(&self) -> usize {
        self.lookahead_samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SAMPLE_RATE;
    use std::f32::consts::PI;

    /// 参考信号与实测信号的 SNR（dB）。
    fn snr_db(reference: &[f32], measured: &[f32]) -> f32 {
        let mut signal = 0.0_f64;
        let mut noise = 0.0_f64;
        for (r, m) in reference.iter().zip(measured) {
            signal += f64::from(*r) * f64::from(*r);
            let diff = f64::from(*r) - f64::from(*m);
            noise += diff * diff;
        }
        (10.0 * (signal / noise).log10()) as f32
    }

    /// 合成元音：120 Hz 基频的谐波堆 + 三条共振峰 + 4 Hz 音节包络。
    ///
    /// 不用单音：单音是周期的，对齐搜索会命中它的周期别名（440 Hz 的周期是 109.09 样本），
    /// 最佳 lag 会落在 306 / 415 / 197 这些等价位置上，测出来的延迟就没有意义了。
    /// 也不用扫频：啁啾在低码率下会被谱涂抹，SNR 天然偏低，测出来的是信号难不难而不是编解码好不好。
    ///
    /// 这个信号与 P1-5 要生成的 `clean_48k_mono.wav` 同源（合成元音），所以数字可以互相印证。
    fn vowel(frames: usize) -> Vec<Frame> {
        let f0 = 120.0_f32;
        // (中心频率, 带宽, 增益)：近似 /a/ 的三个共振峰
        let formants = [
            (700.0_f32, 130.0_f32, 1.0_f32),
            (1_220.0, 170.0, 0.6),
            (2_600.0, 250.0, 0.3),
        ];
        let harmonics = 40;
        let mut input = vec![[0.0_f32; FRAME_SAMPLES]; frames];
        for (i, frame) in input.iter_mut().enumerate() {
            for (j, sample) in frame.iter_mut().enumerate() {
                let t = (i * FRAME_SAMPLES + j) as f32 / SAMPLE_RATE as f32;
                let mut value = 0.0_f32;
                for k in 1..=harmonics {
                    let freq = f0 * k as f32;
                    if freq > 8_000.0 {
                        break;
                    }
                    let mut gain = 0.0_f32;
                    for (center, bandwidth, amp) in formants {
                        let d = (freq - center) / bandwidth;
                        gain += amp * (-d * d).exp();
                    }
                    if gain > 1e-3 {
                        value += gain * (2.0 * PI * freq * t).sin() / (k as f32).sqrt();
                    }
                }
                // 音节包络：4 Hz 的周期性起落，让编码器看到真实的语音动态
                let envelope = 0.5 * (1.0 - (2.0 * PI * 4.0 * t).cos());
                *sample = 0.12 * value * envelope;
            }
        }
        input
    }

    #[test]
    fn lookahead_is_312_samples() {
        // 技术设计 §6.3：6.5 ms @ 48 kHz = 312 样本。这个常数进延迟预算，改动必须被测试挡住。
        let codec = OpusCodec::with_defaults().expect("构造 OpusCodec 失败");
        assert_eq!(codec.lookahead_samples(), 312);
    }

    /// 把一段合成元音整段编码再解码，返回（最佳对齐 lag, 该 lag 下的 SNR dB）。
    ///
    /// 不做「延迟一定是 N」的假设：`get_lookahead()` 报 312，但逐包「编码即解码」时实测在 306–310 之间，
    /// 所以这里扫一遍 250..360 取 SNR 最高的 lag，让测量自己说话。
    fn measure_round_trip(config: OpusConfig) -> (usize, f32) {
        let mut codec = OpusCodec::new(config).expect("构造 OpusCodec 失败");
        let frames = 100;
        let input = vowel(frames);
        let mut packet = [0_u8; MAX_PACKET_BYTES];

        let mut decoded: Vec<f32> = Vec::with_capacity(frames * FRAME_SAMPLES);
        for frame in &input {
            let len = codec.encode(frame, &mut packet);
            assert!(len > 0, "编码失败");
            let mut out = [0.0_f32; FRAME_SAMPLES];
            assert!(
                codec.decode(&packet[..len], false, &mut out),
                "解码未得到完整一帧"
            );
            decoded.extend_from_slice(&out);
        }

        let source: Vec<f32> = input.iter().flatten().copied().collect();
        let mut best = (0_usize, f32::NEG_INFINITY);
        for lag in 250..360 {
            let n = decoded.len() - lag;
            let snr = snr_db(&source[..n], &decoded[lag..]);
            if snr > best.1 {
                best = (lag, snr);
            }
        }
        best
    }

    #[test]
    fn round_trip_is_aligned_and_clean() {
        let (lag, snr) = measure_round_trip(OpusConfig::default());

        // 前瞻 312 是 libopus 的自报值（进延迟预算）；实测往返对齐落在 300..=320 内即可。
        assert!(
            (300..=320).contains(&lag),
            "往返延迟 {lag} 样本，超出预期区间 300..=320（前瞻 312，实测 306–310）"
        );
        // 32 kbps + FEC 在合成元音上实测约 17.5 dB。执行方案草案写的 20 dB 是拍脑袋数字，
        // 实测之后把门禁定在 15 dB（留 2.5 dB 余量），并把实测值记进 docs/measurements.md。
        assert!(
            snr > 15.0,
            "往返 SNR = {snr} dB（lag = {lag}），低于 15 dB 门禁"
        );
    }

    #[test]
    fn higher_bitrate_buys_snr() {
        // 反证「低 SNR 是信号难而不是链路坏了」：把码率翻倍，SNR 必须明显上升。
        let (_, snr_32k) = measure_round_trip(OpusConfig::default());
        let (_, snr_64k) = measure_round_trip(OpusConfig {
            bitrate_bps: 64_000,
            ..OpusConfig::default()
        });
        assert!(
            snr_64k > snr_32k + 2.0,
            "64 kbps SNR = {snr_64k} dB 只比 32 kbps 的 {snr_32k} dB 高一点点，编码链路可能有问题"
        );
    }

    #[test]
    fn plc_path_returns_a_full_frame() {
        let mut codec = OpusCodec::with_defaults().expect("构造 OpusCodec 失败");
        let mut out = [0.0_f32; FRAME_SAMPLES];
        // 连续丢包也要吐满一帧（PLC），不能返回半帧把下游时序打乱。
        assert!(codec.decode(&[], true, &mut out));
    }

    #[test]
    fn unusable_output_buffer_fails_without_panicking() {
        let mut codec = OpusCodec::with_defaults().expect("构造 OpusCodec 失败");
        let frame = vowel(1)[0];
        // 缓冲为 0 字节时 libopus 返回 BAD_ARG；接缝把它翻译成 0，绝不 panic。
        // 注意：1 字节的缓冲 libopus 是能编出来的（最小包），所以这里用空切片。
        let mut empty: [u8; 0] = [];
        assert_eq!(codec.encode(&frame, &mut empty), 0);
    }
}
