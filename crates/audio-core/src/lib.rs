//! HushSquad 音频处理核心。
//!
//! 本 crate 只吃 `[f32; 480]`、吐 `[f32; 480]`，**不依赖 I/O 与网络**，因此可以在 CI 里离线跑。
//!
//! # 实时纪律（任何 PR 违反都会被直接打回）
//!
//! 1. 热路径（音频回调与每帧处理）内**禁止**堆分配：不许 `Vec::push` 扩容、`format!`、
//!    `String`、`Box::new`、`collect`。所有缓冲在构造期预分配。
//! 2. 热路径内**禁止**加锁、禁止阻塞系统调用、禁止日志（`log::info!` 也会格式化）。
//! 3. 热路径内**禁止** panic：索引用 `get`/`chunks_exact`，除法前检查分母。
//! 4. 任何阶段都必须正确报告 `latency_samples()`，否则延迟预算表会静默失真。
//!
//! # 与其它模块的关系
//!
//! 依赖方向是单向的：`audio-core` 不认识 `transport`、`protocol`、`client-core`。
//! 这样才能把整套前处理平移到 LiveKit 或 QUIC 之上（见技术设计 §4.2）。
//!
//! # 接缝（P1-1）
//!
//! 全 crate 对外的唯一契约是 [`Stage`]：所有处理环节（高通、AGC2、RNNoise、DFN3、Opus 封装）
//! 都实现它，因此链路可以按档位自由拼装、离线跑回归、并把延迟逐级记账。

#![deny(unsafe_code)] // 确需 SIMD 等优化时，在具体项上 #[allow(unsafe_code)] 并写明理由

pub mod chain;
pub mod codec;
pub mod jitter;
pub mod receive;
pub mod stages;

pub use chain::{Chain, ChainBuildError, ChainDesc, ChainParseError, StageKind};
pub use codec::{AudioCodec, CodecError, MAX_PACKET_BYTES, OpusCodec, OpusConfig};
pub use stages::{Agc2, Crossfade, Highpass, Rnn, StageWorker, WorkerStage, WorkerStatsSnapshot};

/// 内部统一采样率：48 kHz（技术设计 §6.1）。
///
/// 设备不是 48 kHz 时由 I/O 边界用 `rubato` 重采样，**不**在处理链中段做。
pub const SAMPLE_RATE: u32 = 48_000;

/// 处理帧长：10 ms（技术设计 §6.1）。
///
/// DFN3 的 hop 也是 10 ms、`nnnoiseless` 是 480、`sonora-agc2` 按 10 ms 帧工作，
/// 链路粒度必须与之一致；2.5/5 ms 的 Opus 帧反而会破坏这个一致性（执行方案 §10 反模式）。
pub const FRAME_MS: u32 = 10;

/// 一帧的样本数：480（48 kHz × 10 ms）。
pub const FRAME_SAMPLES: usize = 480;

/// 一帧音频：单声道、f32、48 kHz。
pub type Frame = [f32; FRAME_SAMPLES];

/// 样本数 → 毫秒。
///
/// 延迟预算表（技术设计 附录 A）的唯一换算口径。所有 `latency_samples()` 都要经它换算，
/// 避免各处手写 `/ 48` 之后出现口径不一致。
#[must_use]
pub fn samples_to_ms(samples: usize) -> f32 {
    samples as f32 * 1000.0 / SAMPLE_RATE as f32
}

/// 所有音频阶段的统一接口。实时线程里调用 [`Stage::process`]。
///
/// 实现者必须同时满足：构造期预分配、`process` 内零分配零锁零日志、如实报告延迟。
pub trait Stage {
    /// 阶段名，与 `ChainDesc` 的档位名一致（`"highpass"` / `"agc2"` / `"rnn"` / `"dfn3"`）。
    ///
    /// 返回 `&'static str` 而非 `&str`：报告与日志要在不借用 `self` 的地方拼装链路描述，
    /// 绑到 `&self` 的生命周期会让调用方无谓地传递借用。
    fn name(&self) -> &'static str;

    /// 该阶段引入的算法延迟（样本数）。这是延迟预算表的**唯一事实来源**。
    ///
    /// 纯因果滤波器（如 80 Hz 高通）返回 0；带帧缓冲的阶段返回其缓冲长度
    /// （RNNoise 480 = 10 ms、DFN3 1440 = 30 ms、Opus 编码器前瞻 312 = 6.5 ms）。
    fn latency_samples(&self) -> usize;

    /// 原地处理一帧，返回该帧的 VAD 概率（0.0–1.0；本阶段不做 VAD 时返回 1.0）。
    ///
    /// VAD 概率会一路上传，供「谁在说话」的 UI 与 DTX 使用，不要在中间环节丢掉。
    fn process(&mut self, frame: &mut Frame) -> f32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_is_ten_milliseconds() {
        // 480 样本 @ 48 kHz = 10 ms ⇒ 每秒 100 帧
        assert_eq!(FRAME_SAMPLES * 100, SAMPLE_RATE as usize);
        assert_eq!(FRAME_MS, 10);
        assert_eq!(samples_to_ms(FRAME_SAMPLES), 10.0);
    }

    #[test]
    fn latency_constants_match_design_doc() {
        // 技术设计 §6.3：Opus 固定前瞻 6.5 ms = 312 样本（任何实现都要付）
        assert_eq!(samples_to_ms(312), 6.5);
        // 技术设计 §6.4：DFN3 算法延迟 30 ms = 1440 样本（P3-3 要在单元测试里断言）
        assert_eq!(samples_to_ms(1_440), 30.0);
        // 零延迟阶段
        assert_eq!(samples_to_ms(0), 0.0);
    }

    /// 最简阶段：证明接缝可用，也证明「不分配、不改延迟」的实现能写得多短。
    struct Passthrough {
        gain: f32,
    }

    impl Stage for Passthrough {
        fn name(&self) -> &'static str {
            "passthrough"
        }

        fn latency_samples(&self) -> usize {
            0
        }

        fn process(&mut self, frame: &mut Frame) -> f32 {
            for sample in frame.iter_mut() {
                *sample *= self.gain;
            }
            1.0
        }
    }

    #[test]
    fn stage_trait_is_implementable() {
        let mut stage = Passthrough { gain: 0.5 };
        let mut frame: Frame = [1.0; FRAME_SAMPLES];

        assert_eq!(stage.name(), "passthrough");
        assert_eq!(stage.latency_samples(), 0);
        assert_eq!(stage.process(&mut frame), 1.0);
        assert_eq!(frame[0], 0.5);
        assert_eq!(frame[FRAME_SAMPLES - 1], 0.5);
    }
}
