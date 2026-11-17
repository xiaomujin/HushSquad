//! AGC2：自适应数字增益 + 限幅（P3-1）。
//!
//! 用的是 `sonora-agc2`（WebRTC `agc2` 的纯 Rust 移植），**不是**自己拍一个「按 RMS 调音量」。
//! 理由：AGC 的难点不在放大，而在于「知道什么时候不该放大」——说话间隙、纯噪声、
//! 突然的爆音、以及增益变化本身造成的泵动。WebRTC 那套把这几件事拆成互相制约的部件：
//!
//! - RNN VAD 给每帧的语音概率（`vad_wrapper`）；
//! - 语音电平估计（`speech_level_estimator`）：只在语音帧上更新，噪声帧不污染估计；
//! - 噪声底估计（`noise_level_estimator`）：增益受「噪声不得被抬到 -50 dBFS 以上」约束，
//!   所以安静时不会把底噪放大成嘶声；
//! - 饱和保护（`saturation_protector`）：根据近期峰值动态给出 headroom，避免一直贴顶；
//! - 限幅器（`limiter`）：子帧插值的增益曲线，保证放大后不爆。
//!
//! 这套部件**没有任何算法延迟**（都是逐帧的因果处理），因此延迟预算里 AGC2 记 0 ms；
//! 代价是 CPU：RNN VAD 是这套里最贵的一块，实测数字见 `docs/measurements.md` §2。
//!
//! # 与平台 AGC 互斥
//!
//! 安卓的 `VOICE_COMMUNICATION` 音源通常自带 AGC/NS。两套 AGC 串联会互相追着调，
//! 听感是周期性泵动与呼吸声。因此启用本阶段时必须在 I/O 侧关掉平台 AGC（P2B 落地）。
//!
//! # 样本尺度（踩过一次的坑）
//!
//! WebRTC 的内部浮点样本不是归一化的 [-1, 1]，而是**按 s16 尺度**（±32768）：它的 dBFS
//! 换算（`float_s16_to_dbfs`）、限幅器的输出钳位（±32767）与增益曲线的输入范围都以此为准。
//! 我们全链路用归一化 f32，所以本环节在入口乘 32768、出口除以 32768（2^15，精确无损失）。
//! 少了这步的实测症状：限幅器把 5.5 的峰值读成 -75 dBFS，完全不动作，输出直接爆掉。

use sonora_agc2::adaptive_digital_gain_controller::{AdaptiveDigitalGainController, FrameInfo};
use sonora_agc2::common::{
    ADJACENT_SPEECH_FRAMES_THRESHOLD, MIN_LEVEL_DBFS, SATURATION_PROTECTOR_INITIAL_HEADROOM_DB,
    VAD_RESET_PERIOD_MS, float_s16_to_dbfs,
};
use sonora_agc2::limiter::Limiter;
use sonora_agc2::noise_level_estimator::NoiseLevelEstimator;
use sonora_agc2::saturation_protector::SaturationProtector;
use sonora_agc2::speech_level_estimator::{AdaptiveDigitalConfig, SpeechLevelEstimator};
use sonora_agc2::vad_wrapper::{MonoVad, VoiceActivityDetectorWrapper};

use crate::{FRAME_SAMPLES, Frame, SAMPLE_RATE, Stage};

/// WebRTC 内部样本尺度：s16 满量程。进出本环节各换算一次。
const S16_SCALE: f32 = 32_768.0;

/// 自适应增益 + 限幅。
pub struct Agc2 {
    vad: VoiceActivityDetectorWrapper,
    speech_level: SpeechLevelEstimator,
    noise: NoiseLevelEstimator,
    saturation: SaturationProtector,
    gain: AdaptiveDigitalGainController,
    limiter: Limiter,
}

impl Agc2 {
    /// 用默认配置（WebRTC 默认值）与 RNN VAD 构造。
    #[must_use]
    pub fn new() -> Self {
        let backend = sonora_simd::detect_backend();
        Self::with_vad(
            AdaptiveDigitalConfig::default(),
            VoiceActivityDetectorWrapper::new(backend, SAMPLE_RATE as i32),
        )
    }

    /// 换一个 VAD 后端（离线实验与单元测试用：RNN VAD 对合成信号不一定判成语音）。
    #[must_use]
    pub fn with_vad_backend(vad: Box<dyn MonoVad>) -> Self {
        Self::with_vad(
            AdaptiveDigitalConfig::default(),
            VoiceActivityDetectorWrapper::with_vad(VAD_RESET_PERIOD_MS, vad, SAMPLE_RATE as i32),
        )
    }

    /// 换配置（离线实验用），VAD 仍是 RNN。
    #[must_use]
    pub fn with_config(config: AdaptiveDigitalConfig) -> Self {
        let backend = sonora_simd::detect_backend();
        Self::with_vad(
            config,
            VoiceActivityDetectorWrapper::new(backend, SAMPLE_RATE as i32),
        )
    }

    /// 同时换配置与 VAD（离线实验与单元测试用）。
    #[must_use]
    pub fn with_config_and_vad(config: AdaptiveDigitalConfig, vad: Box<dyn MonoVad>) -> Self {
        Self::with_vad(
            config,
            VoiceActivityDetectorWrapper::with_vad(VAD_RESET_PERIOD_MS, vad, SAMPLE_RATE as i32),
        )
    }

    fn with_vad(config: AdaptiveDigitalConfig, vad: VoiceActivityDetectorWrapper) -> Self {
        Self {
            vad,
            speech_level: SpeechLevelEstimator::new(&config, ADJACENT_SPEECH_FRAMES_THRESHOLD),
            noise: NoiseLevelEstimator::default(),
            saturation: SaturationProtector::new(
                SATURATION_PROTECTOR_INITIAL_HEADROOM_DB,
                ADJACENT_SPEECH_FRAMES_THRESHOLD,
            ),
            gain: AdaptiveDigitalGainController::new(config, ADJACENT_SPEECH_FRAMES_THRESHOLD),
            limiter: Limiter::new(FRAME_SAMPLES),
        }
    }
}

impl Default for Agc2 {
    fn default() -> Self {
        Self::new()
    }
}

/// 一帧的 RMS 电平（dBFS）。
///
/// 输入必须已经按 s16 尺度换算过：`float_s16_to_dbfs` 内部以 32768 为 0 dBFS，
/// 与 AGC2 各部件用的是同一个口径。
fn rms_dbfs(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return MIN_LEVEL_DBFS;
    }
    let energy: f32 = frame.iter().map(|x| x * x).sum();
    float_s16_to_dbfs((energy / frame.len() as f32).sqrt())
}

/// 一帧的峰值电平（dBFS），同样按 s16 尺度。
fn peak_dbfs(frame: &[f32]) -> f32 {
    let peak = frame.iter().fold(0.0_f32, |acc, x| acc.max(x.abs()));
    float_s16_to_dbfs(peak)
}

impl Stage for Agc2 {
    fn name(&self) -> &'static str {
        "agc2"
    }

    fn latency_samples(&self) -> usize {
        // 全部部件都是逐帧因果处理，不攒帧、不前视。
        0
    }

    fn process(&mut self, frame: &mut Frame) -> f32 {
        // `sonora-agc2` 沿用 WebRTC 的内部样本约定：浮点但按 s16 尺度（±32768），
        // 所有 dBFS 换算与限幅阈值都以 32768 为 0 dBFS。我们全链路是归一化 [-1,1]，
        // 所以在这里换算一次、出去再换回来（除以 2^15 是精确的，没有精度损失）。
        // 漏掉这一步的后果实测过：限幅器把 5.5 当成 -75 dBFS，完全不动手。
        for sample in frame.iter_mut() {
            *sample *= S16_SCALE;
        }

        let speech_probability = self.vad.analyze(&frame[..]);

        let level_dbfs = rms_dbfs(&frame[..]);
        self.speech_level.update(level_dbfs, speech_probability);
        let speech_level_dbfs = self.speech_level.level_dbfs();
        let speech_level_reliable = self.speech_level.is_confident();

        let noise_rms_dbfs = self.noise.analyze(&[&frame[..]]);
        let peak = peak_dbfs(&frame[..]);
        self.saturation
            .analyze(speech_probability, peak, speech_level_dbfs);

        let info = FrameInfo {
            speech_probability,
            speech_level_dbfs,
            speech_level_reliable,
            noise_rms_dbfs,
            headroom_db: self.saturation.headroom_db(),
            limiter_envelope_dbfs: self.limiter.last_audio_level(),
        };

        // 单声道：一个通道的借用数组，栈上构造，不分配。
        let mut channels = [frame.as_mut_slice()];
        self.gain.process(&info, &mut channels);
        self.limiter.process(&mut channels);

        for sample in frame.iter_mut() {
            *sample /= S16_SCALE;
        }

        speech_probability
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples_to_ms;

    /// 固定概率的假 VAD：把「是不是语音」这件事从测试里摘出去，
    /// 只验证增益与限幅的行为。
    struct FixedVad(f32);

    impl MonoVad for FixedVad {
        fn sample_rate_hz(&self) -> i32 {
            24_000
        }

        fn reset(&mut self) {}

        fn analyze(&mut self, _frame: &[f32]) -> f32 {
            self.0
        }
    }

    fn sine(amplitude: f32, frames: usize) -> Vec<f32> {
        let n = frames * FRAME_SAMPLES;
        (0..n)
            .map(|i| amplitude * (2.0 * std::f32::consts::PI * 1_000.0 * i as f32 / 48_000.0).sin())
            .collect()
    }

    fn run(stage: &mut Agc2, input: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let mut out = Vec::with_capacity(input.len());
        let mut probs = Vec::new();
        for chunk in input.as_chunks::<FRAME_SAMPLES>().0 {
            let mut frame: Frame = [0.0; FRAME_SAMPLES];
            frame.copy_from_slice(chunk);
            probs.push(stage.process(&mut frame));
            out.extend_from_slice(&frame);
        }
        (out, probs)
    }

    fn rms(xs: &[f32]) -> f32 {
        if xs.is_empty() {
            return 0.0;
        }
        (xs.iter().map(|x| x * x).sum::<f32>() / xs.len() as f32).sqrt()
    }

    fn peak(xs: &[f32]) -> f32 {
        xs.iter().fold(0.0_f32, |acc, x| acc.max(x.abs()))
    }

    #[test]
    fn latency_is_zero() {
        let stage = Agc2::new();
        assert_eq!(stage.name(), "agc2");
        assert_eq!(stage.latency_samples(), 0);
        assert_eq!(samples_to_ms(stage.latency_samples()), 0.0);
    }

    #[test]
    fn quiet_speech_is_lifted() {
        // -40 dBFS 的「语音」：AGC2 的初始增益是 15 dB，应当立刻把它抬上来。
        let input = sine(0.01, 100);
        let mut stage = Agc2::with_vad_backend(Box::new(FixedVad(1.0)));
        let (out, _) = run(&mut stage, &input);
        let ratio = rms(&out) / rms(&input);
        assert!(
            ratio > 3.0,
            "只放大了 {ratio:.2} 倍（{:.1} dB），初始增益没生效",
            20.0 * ratio.log10()
        );
        assert!(peak(&out) <= 1.05, "放大后峰值 {}，限幅没兜住", peak(&out));
    }

    #[test]
    fn loud_input_is_limited_not_clipped_hard() {
        // 满量程附近的正弦：初始 15 dB 增益会把它推到限幅器，输出不得超出满量程太多。
        let input = sine(0.99, 500);
        let mut stage = Agc2::with_vad_backend(Box::new(FixedVad(1.0)));
        let (out, _) = run(&mut stage, &input);
        let peak_out = peak(&out);
        assert!(
            peak_out <= 1.05,
            "输出峰值 {peak_out}，限幅器失效（未限幅时会是 5.6）"
        );
        assert!(out.iter().all(|x| x.is_finite()), "输出出现 NaN/Inf");
    }

    #[test]
    fn gain_comes_down_on_sustained_loud_input() {
        // 自适应增益下降速度上限 6 dB/s：15 dB 的初始增益约需 2.5 s 退到 0。
        let input = sine(0.5, 600);
        let mut stage = Agc2::with_vad_backend(Box::new(FixedVad(1.0)));
        let (out, _) = run(&mut stage, &input);
        let tail = &out[out.len() - 100 * FRAME_SAMPLES..];
        let tail_input = &input[input.len() - 100 * FRAME_SAMPLES..];
        let ratio_db = 20.0 * (rms(tail) / rms(tail_input)).log10();
        assert!(
            ratio_db.abs() < 3.0,
            "5 s 后增益仍偏离 {ratio_db:.1} dB，自适应没有收敛"
        );
    }

    #[test]
    fn silence_stays_silent_and_finite() {
        let mut stage = Agc2::with_vad_backend(Box::new(FixedVad(0.0)));
        let input = vec![0.0_f32; 100 * FRAME_SAMPLES];
        let (out, _) = run(&mut stage, &input);
        assert!(out.iter().all(|x| *x == 0.0), "静音输入产生了输出");
    }

    #[test]
    fn vad_probability_is_passed_through() {
        // Stage 契约要求把 VAD 概率一路上传：链路的 DTX 与「谁在说话」靠它。
        let mut stage = Agc2::with_vad_backend(Box::new(FixedVad(0.25)));
        let input = sine(0.1, 3);
        let (_, probs) = run(&mut stage, &input);
        assert!(
            probs.iter().all(|p| (*p - 0.25).abs() < 1e-6),
            "VAD 概率没透传：{probs:?}"
        );
    }

    /// 首 100 ms 与稳态（最后 1 s）的 RMS 差，用来量化「开场爆音」。
    fn startup_burst_db(out: &[f32]) -> f32 {
        let head = rms(&out[..10 * FRAME_SAMPLES]);
        let steady = rms(&out[out.len() - 100 * FRAME_SAMPLES..]);
        20.0 * (head / steady).log10()
    }

    #[test]
    fn default_initial_gain_causes_a_bounded_startup_burst() {
        // 已知行为（不是期望行为）：WebRTC 默认 initial_gain_db = 15，而增益下降速率上限是
        // 6 dB/s，所以流的**头 1–2 s** 会比稳态响一截，峰值被限幅器顶到满量程。
        // 离线实测（`clean_48k_mono.wav`，20 s）：首秒 RMS 比稳态高 8.0 dB，
        // 全片只有 57 个样本被钉在 |x| > 0.99，3 s 后回落到峰值 -5.5 dBFS。
        // 这条测试把行为钉住：改默认初始增益时它会失败，提醒同步文档与 measurements。
        let input = sine(0.5, 400);
        let mut stage = Agc2::with_vad_backend(Box::new(FixedVad(1.0)));
        let (out, _) = run(&mut stage, &input);
        let burst = startup_burst_db(&out);
        assert!(
            (3.0..15.0).contains(&burst),
            "启动瞬态 {burst:.1} dB，与文档记录的 8 dB 量级不符"
        );
        assert!(peak(&out) <= 1.05, "启动瞬态把峰值推到 {}", peak(&out));
    }

    #[test]
    fn zero_initial_gain_removes_the_startup_burst() {
        // 备选方案：initial_gain_db = 0（quiet 麦克风靠自适应爬升，6 dB/s）。
        // 它消掉开场爆音，代价是安静麦克风开头几秒偏轻——取舍需要真实语音素材做盲听。
        let input = sine(0.5, 400);
        let mut stage = Agc2::with_config_and_vad(
            AdaptiveDigitalConfig {
                initial_gain_db: 0.0,
                ..AdaptiveDigitalConfig::default()
            },
            Box::new(FixedVad(1.0)),
        );
        let (out, _) = run(&mut stage, &input);
        let burst = startup_burst_db(&out);
        assert!(burst < 2.0, "初始增益归零后仍有 {burst:.1} dB 的启动瞬态");
    }

    #[test]
    fn rnn_vad_path_runs_without_panicking() {
        // 默认构造走真 RNN VAD：这里只保证装配正确、能跑、输出有限。
        let mut stage = Agc2::new();
        let input = sine(0.2, 50);
        let (out, probs) = run(&mut stage, &input);
        assert!(out.iter().all(|x| x.is_finite()));
        assert!(probs.iter().all(|p| (0.0..=1.0).contains(p)));
    }
}
