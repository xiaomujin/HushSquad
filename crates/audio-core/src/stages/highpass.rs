//! 80 Hz 二阶高通（P3-1）。
//!
//! 目的：去掉桌面震动、空调低频与近讲气流造成的隆隆声，避免它们吃掉 Opus 的比特、
//! 并在后面 AGC 放大时把整个低频抬成嗡声。
//!
//! 实现是 RBJ 双二次（Butterworth Q）的高通型，转置直接 II 型状态，**零算法延迟**：
//! 纯因果 IIR，逐样本处理，不攒帧。系数在构造期算好（f64 计算、f32 存储）。
//!
//! 为什么不用 `sonora_agc2::biquad_filter`：它只是通用双二次，系数仍要我们算；
//! 少一层依赖语义，且这里的系数与状态要能被单元测试直接断言。

use std::f32::consts::PI;

use crate::{Frame, Stage};

/// 默认截止频率（Hz）。80 Hz 是技术设计 §6.2 定的：低于男声基频下限，高于隆隆声。
pub const DEFAULT_CUTOFF_HZ: f32 = 80.0;

/// 二阶 Butterworth 的 Q（= 1/√2），通带最平、无峰起。
const BUTTERWORTH_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// 80 Hz 二阶高通。
#[derive(Debug, Clone)]
pub struct Highpass {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    // 转置直接 II 型的两个状态：一个采样点的记忆就够，不需要缓冲。
    z1: f32,
    z2: f32,
}

impl Highpass {
    /// 用默认截止频率（80 Hz）与默认采样率（48 kHz）构造。
    #[must_use]
    pub fn new() -> Self {
        Self::with_cutoff(DEFAULT_CUTOFF_HZ, crate::SAMPLE_RATE)
    }

    /// 指定截止频率与采样率构造（只用于测试与离线实验）。
    #[must_use]
    pub fn with_cutoff(cutoff_hz: f32, sample_rate: u32) -> Self {
        let (b0, b1, b2, a1, a2) = highpass_coefficients(cutoff_hz, sample_rate);
        Self {
            b0,
            b1,
            b2,
            a1,
            a2,
            z1: 0.0,
            z2: 0.0,
        }
    }

    /// 清空滤波器状态（换设备或重建链路时用）。
    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    /// 当前状态的幅频响应（线性幅度），只用于测试与诊断。
    #[must_use]
    pub fn magnitude_at(&self, freq_hz: f32, sample_rate: u32) -> f32 {
        // |H(e^{jw})| 用复数直接算，避免测试里重复一份滤波器实现。
        let w = 2.0 * PI * freq_hz / sample_rate as f32;
        let (sin1, cos1) = w.sin_cos();
        let (sin2, cos2) = (2.0 * w).sin_cos();
        let num_re = self.b0 + self.b1 * cos1 + self.b2 * cos2;
        let num_im = -(self.b1 * sin1 + self.b2 * sin2);
        let den_re = 1.0 + self.a1 * cos1 + self.a2 * cos2;
        let den_im = -(self.a1 * sin1 + self.a2 * sin2);
        let num = (num_re * num_re + num_im * num_im).sqrt();
        let den = (den_re * den_re + den_im * den_im).sqrt();
        if den == 0.0 { 0.0 } else { num / den }
    }
}

impl Default for Highpass {
    fn default() -> Self {
        Self::new()
    }
}

/// RBJ cookbook 的高通型双二次系数（已按 a0 归一化）。
fn highpass_coefficients(cutoff_hz: f32, sample_rate: u32) -> (f32, f32, f32, f32, f32) {
    let w0 = 2.0 * std::f64::consts::PI * f64::from(cutoff_hz) / f64::from(sample_rate);
    let (sin_w0, cos_w0) = w0.sin_cos();
    let alpha = sin_w0 / (2.0 * f64::from(BUTTERWORTH_Q));
    let a0 = 1.0 + alpha;
    let b0 = (1.0 + cos_w0) / 2.0;
    let b1 = -(1.0 + cos_w0);
    let b2 = (1.0 + cos_w0) / 2.0;
    let a1 = -2.0 * cos_w0;
    let a2 = 1.0 - alpha;
    (
        (b0 / a0) as f32,
        (b1 / a0) as f32,
        (b2 / a0) as f32,
        (a1 / a0) as f32,
        (a2 / a0) as f32,
    )
}

impl Stage for Highpass {
    fn name(&self) -> &'static str {
        "highpass"
    }

    fn latency_samples(&self) -> usize {
        0
    }

    fn process(&mut self, frame: &mut Frame) -> f32 {
        self.process_slice(frame);
        // 高通不提供 VAD。
        1.0
    }
}

impl Highpass {
    /// 原地处理一段**连续**样本。
    ///
    /// `process` 与离线整段处理共用这一份实现，因此「分帧跑」与「整段跑」在数值上必须
    /// 逐位相同（`state_carries_across_frame_boundaries` 断言这件事）。
    pub fn process_slice(&mut self, samples: &mut [f32]) {
        for sample in samples.iter_mut() {
            let x = *sample;
            let y = self.b0 * x + self.z1;
            self.z1 = self.b1 * x - self.a1 * y + self.z2;
            self.z2 = self.b2 * x - self.a2 * y;
            *sample = y;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FRAME_SAMPLES, SAMPLE_RATE, samples_to_ms};

    fn run(stage: &mut Highpass, samples: &[f32]) -> Vec<f32> {
        let mut out = Vec::with_capacity(samples.len());
        for chunk in samples.chunks(FRAME_SAMPLES) {
            let mut frame: Frame = [0.0; FRAME_SAMPLES];
            frame[..chunk.len()].copy_from_slice(chunk);
            stage.process(&mut frame);
            out.extend_from_slice(&frame[..chunk.len()]);
        }
        out
    }

    fn sine(freq_hz: f32, amplitude: f32, seconds: f32) -> Vec<f32> {
        let n = (SAMPLE_RATE as f32 * seconds) as usize;
        (0..n)
            .map(|i| amplitude * (2.0 * PI * freq_hz * i as f32 / SAMPLE_RATE as f32).sin())
            .collect()
    }

    fn rms(xs: &[f32]) -> f32 {
        if xs.is_empty() {
            return 0.0;
        }
        (xs.iter().map(|x| x * x).sum::<f32>() / xs.len() as f32).sqrt()
    }

    #[test]
    fn latency_is_zero_and_name_matches_stage_kind() {
        let stage = Highpass::new();
        assert_eq!(stage.name(), "highpass");
        assert_eq!(stage.latency_samples(), 0);
        assert_eq!(samples_to_ms(stage.latency_samples()), 0.0);
    }

    #[test]
    fn dc_is_removed() {
        // 1 s 的常数输入：一阶以上的高通最终把直流完全压掉（残留只是数值尾）。
        let mut stage = Highpass::new();
        let input = vec![1.0_f32; SAMPLE_RATE as usize];
        let out = run(&mut stage, &input);
        let tail = &out[out.len() - FRAME_SAMPLES..];
        let peak = tail.iter().fold(0.0_f32, |acc, x| acc.max(x.abs()));
        assert!(peak < 1e-3, "直流残留 {peak}，高通没有生效");
    }

    #[test]
    fn speech_band_passes_almost_unchanged() {
        let stage = Highpass::new();
        // 1 kHz 在通带内，幅度应几乎不变（<0.1 dB 偏差）。
        let magnitude = stage.magnitude_at(1_000.0, SAMPLE_RATE);
        let db = 20.0 * magnitude.log10();
        assert!(db.abs() < 0.1, "1 kHz 增益 {db:.3} dB，应为 0 dB");
        // 实测验证一次：1 kHz 正弦过完滤波器，RMS 之比应与理论一致。
        let mut stage = Highpass::new();
        let input = sine(1_000.0, 0.5, 1.0);
        let out = run(&mut stage, &input);
        // 丢掉前 100 ms 的瞬态再比。
        let skip = SAMPLE_RATE as usize / 10;
        let ratio = rms(&out[skip..]) / rms(&input[skip..]);
        assert!(
            (ratio - 1.0).abs() < 0.01,
            "1 kHz 实测幅度比 {ratio:.4}，应约为 1"
        );
    }

    #[test]
    fn rumble_is_attenuated() {
        let stage = Highpass::new();
        // 二阶高通在截止频率以下按 -12 dB/oct 衰减：40 Hz 约 -12 dB，20 Hz 约 -24 dB。
        let at_40 = 20.0 * stage.magnitude_at(40.0, SAMPLE_RATE).log10();
        let at_20 = 20.0 * stage.magnitude_at(20.0, SAMPLE_RATE).log10();
        assert!(at_40 < -10.0, "40 Hz 只衰减了 {at_40:.1} dB");
        assert!(at_20 < -20.0, "20 Hz 只衰减了 {at_20:.1} dB");
    }

    #[test]
    fn state_carries_across_frame_boundaries() {
        // 分帧处理与整段处理必须给出**逐位相同**的结果，否则每 10 ms 都有一次接缝。
        let input = sine(1_000.0, 0.5, 0.5);
        let mut staged = Highpass::new();
        let by_frame = run(&mut staged, &input);
        // 参照实现：同一份 process_slice，但一次喂完整段。
        let mut whole = Highpass::new();
        let mut by_slice = input.clone();
        whole.process_slice(&mut by_slice);
        let max_diff = by_frame
            .iter()
            .zip(&by_slice)
            .fold(0.0_f32, |acc, (a, b)| acc.max((a - b).abs()));
        assert!(
            max_diff == 0.0,
            "分帧与整段结果差 {max_diff}，每 10 ms 会有一次接缝"
        );
    }

    #[test]
    fn silence_stays_silent() {
        let mut stage = Highpass::new();
        let mut frame: Frame = [0.0; FRAME_SAMPLES];
        for _ in 0..10 {
            stage.process(&mut frame);
        }
        assert!(frame.iter().all(|x| *x == 0.0), "静音输入产生了输出");
    }

    #[test]
    fn reset_clears_history() {
        let mut stage = Highpass::new();
        let mut frame: Frame = [1.0; FRAME_SAMPLES];
        stage.process(&mut frame);
        stage.reset();
        let mut silent: Frame = [0.0; FRAME_SAMPLES];
        stage.process(&mut silent);
        assert!(silent.iter().all(|x| *x == 0.0), "reset 后仍有残留状态");
    }
}
