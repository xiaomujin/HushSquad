//! RNNoise 降噪（P3-2，低延迟档）。
//!
//! 用 `nnnoiseless`（Xiph RNNoise 的纯 Rust 移植，BSD-3）。它是**帧式**降噪器：一次吃 480 样本、
//! 吐 480 样本，内部靠上一帧的窗口与基音缓冲做分析，所以有 **1 帧（10 ms）的算法延迟**——
//! 这是延迟预算表附录 A 第 3 行的「低延迟档 10 ms」。
//!
//! # 样本尺度
//!
//! 与 `sonora-agc2` 同一个坑：`nnnoiseless` 的输入输出虽然也是 `f32`，但要求是 **i16 尺度**
//! （`[-32768, 32767]`），文档写得很直白。我们全链路是归一化 [-1,1]，因此进出各换算一次。
//!
//! # VAD 归属（与 AGC2 的关系）
//!
//! 链路里可能同时有两个 VAD：AGC2 内部的 RNN VAD（只服务于它自己的增益决策，无法外借——
//! 它需要「处理前」这一帧的概率）和本环节输出的概率（RNNoise 的 RNN 同时给出语音概率）。
//! 对外暴露哪一个由 [`crate::Chain::process`] 的规则决定：**第一个给出有效值的环节胜出**。
//! 设计顺序是 高通 → 降噪 → AGC2，本环节排在 AGC2 前面，所以对外暴露的是 RNNoise 的 VAD——
//! 与「用降噪器自己的判据做 DTX / 「谁在说话」」一致。
//!
//! # 性能注意
//!
//! `nnnoiseless` 对**纯静音**有快速返回：`compute_frame_features` 判定静音时整个 RNN 不跑，
//! 所以安静时它比说话时便宜得多。这条在 `measurements.md` §2 的 p50/p95 差距里能看出来。

use nnnoiseless::DenoiseState;

use crate::{FRAME_SAMPLES, Frame, Stage};

/// i16 尺度换算（与 `sonora-agc2` 相同：2^15，乘除精确）。
const S16_SCALE: f32 = 32_768.0;

/// RNNoise 降噪。
pub struct Rnn {
    state: Box<DenoiseState<'static>>,
    // `process_frame` 要求输入输出是两个不重叠的切片，所以留一块输出缓冲（构造期分配一次）。
    scratch: [f32; FRAME_SAMPLES],
}

impl Rnn {
    /// 用默认模型构造，并在构造期**预热一帧**。
    ///
    /// 预热不是为了音质，是为了实时纪律：`easyfft` 的 FFT 计划是**惰性分配**的，第一次
    /// `process_frame` 会做 48 次堆分配（实测，与素材无关）。留到音频回调里就是实时线程上
    /// 的 48 次分配——`testkit` 的「热路径零分配」断言会直接拦下来。构造期喂一帧静音把它
    /// 烧热，输出丢弃（RNNoise 的首帧输出本来也有淡入瑕疵）。
    #[must_use]
    pub fn new() -> Self {
        let mut state = DenoiseState::new();
        let mut scratch = [0.0; FRAME_SAMPLES];
        let silence = [0.0; FRAME_SAMPLES];
        let _ = state.process_frame(&mut scratch, &silence);
        Self { state, scratch }
    }
}

impl Default for Rnn {
    fn default() -> Self {
        Self::new()
    }
}

impl Stage for Rnn {
    fn name(&self) -> &'static str {
        "rnn"
    }

    fn latency_samples(&self) -> usize {
        // 帧式降噪器：输出对应当前帧，但分析用到了上一帧的窗口与基音缓冲，
        // 按 RNNoise 的设计记 1 帧 = 480 样本（附录 A 第 3 行）。
        FRAME_SAMPLES
    }

    fn process(&mut self, frame: &mut Frame) -> f32 {
        for sample in frame.iter_mut() {
            *sample *= S16_SCALE;
        }

        let vad = self.state.process_frame(&mut self.scratch, &frame[..]);

        // 钳到满量程：RNNoise 是「按频带乘增益 + 重叠相加」，输出可以超过 1.0（实测白噪声
        // 峰值 1.27）。设计顺序里降噪在 AGC2 之前、限幅器在最后，所以正常不会漏出去；
        // 但把钳位放在这里，链路的「输出在 [-1,1]」契约就不依赖调用方的顺序了。
        for (sample, denoised) in frame.iter_mut().zip(&self.scratch) {
            *sample = (denoised / S16_SCALE).clamp(-1.0, 1.0);
        }

        vad
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FRAME_SAMPLES, samples_to_ms};

    fn run(stage: &mut Rnn, input: &[f32]) -> (Vec<f32>, Vec<f32>) {
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

    fn db(ratio: f32) -> f32 {
        20.0 * ratio.log10()
    }

    /// 确定性白噪声（LCG），避免引 rand 进实时 crate 的测试依赖。
    fn white_noise(amplitude: f32, frames: usize) -> Vec<f32> {
        let mut state: u32 = 0x1234_5678;
        (0..frames * FRAME_SAMPLES)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let unit = (state >> 8) as f32 / 16_777_216.0 * 2.0 - 1.0;
                unit * amplitude
            })
            .collect()
    }

    #[test]
    fn latency_is_one_frame() {
        let stage = Rnn::new();
        assert_eq!(stage.name(), "rnn");
        assert_eq!(stage.latency_samples(), FRAME_SAMPLES);
        assert_eq!(samples_to_ms(stage.latency_samples()), 10.0);
    }

    #[test]
    fn stationary_noise_is_attenuated() {
        // 稳态白噪声：RNNoise 判成非语音后应大幅衰减。
        let input = white_noise(0.1, 100);
        let mut stage = Rnn::new();
        let (out, probs) = run(&mut stage, &input);
        // 丢掉前 200 ms 的收敛期。
        let skip = 20 * FRAME_SAMPLES;
        let attenuation = db(rms(&out[skip..]) / rms(&input[skip..]));
        assert!(
            attenuation < -1.0,
            "白噪声只被压了 {attenuation:.1} dB，降噪没起作用"
        );
        assert!(
            probs.iter().all(|p| (0.0..=1.0).contains(p)),
            "VAD 概率越界"
        );
    }

    #[test]
    fn silence_stays_silent_and_finite() {
        let mut stage = Rnn::new();
        let input = vec![0.0_f32; 20 * FRAME_SAMPLES];
        let (out, probs) = run(&mut stage, &input);
        assert!(out.iter().all(|x| x.is_finite()));
        assert!(out.iter().all(|x| x.abs() < 1e-6), "静音输入产生了输出");
        assert!(probs.iter().all(|p| *p < 0.5), "静音被判成语音");
    }

    #[test]
    fn output_never_exceeds_full_scale() {
        let input = white_noise(0.9, 50);
        let mut stage = Rnn::new();
        let (out, _) = run(&mut stage, &input);
        let peak = out.iter().fold(0.0_f32, |acc, x| acc.max(x.abs()));
        assert!(peak <= 1.0, "输出峰值 {peak} 超过满量程");
    }

    #[test]
    fn processing_is_deterministic() {
        // 同一输入、两个独立状态：输出必须逐位一致（降噪器内部无随机性）。
        let input = white_noise(0.05, 30);
        let (a, pa) = run(&mut Rnn::new(), &input);
        let (b, pb) = run(&mut Rnn::new(), &input);
        assert_eq!(a, b, "两次运行输出不一致");
        assert_eq!(pa, pb, "两次运行 VAD 不一致");
    }
}
