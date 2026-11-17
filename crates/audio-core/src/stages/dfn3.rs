//! DeepFilterNet3 降噪（P3-3，质量档）。
//!
//! 只有打开 `denoise-dfn3` feature 时才编译：模型（8.4 MB ONNX）与 tract 推理运行时都随包进来，
//! 桌面可以接受，APK 要单独评估（选型报告 §2.3）。关掉 feature 时 [`crate::stages::build`] 对
//! `Dfn3` 返回 `None`，链路会明确报「未实现」，不会静默降级。
//!
//! # 延迟
//!
//! `DfTract::delay()` = **1440 样本 = 30 ms**（10 ms 的 STFT 重叠 + 2 帧模型前瞻），与技术设计
//! 附录 A 第 3 行一致。这个数由库自己报，我们**不写死常数**：库升级时预算会自动跟着变。
//! 单元测试断言它等于 1440，一旦不符就让门禁失败（P3-3 的验收条件）。
//!
//! # VAD
//!
//! DFN3 **不提供 VAD 概率**：`process` 返回的是局部信噪比（dB），不是「这段是不是语音」。
//! 按 [`crate::Chain::process`] 的「第一个有效值胜出」规则，本环节返回 `1.0`（= 我没有 VAD），
//! 于是质量档链路 `highpass,dfn3,agc2` 的 VAD 由 **AGC2 的 RNN VAD** 提供。若链上没有 AGC2，
//! 整条链就没有 VAD，DTX 会退化成「一直发」。
//!
//! # 样本尺度
//!
//! 与 `sonora-agc2` / `nnnoiseless` 不同，DFN3 用**归一化 [-1,1]**（libDF 的 dBFS 以 1.0 为满量程，
//! 内部 `MEAN_NORM_INIT = -60 dB` 也按这个口径）。所以本环节**不做** s16 换算——这也是最容易
//! 抄错的一处，三个降噪/增益库的尺度约定各不相同。

use deep_filter_rt::tract::DfTract;
use ndarray::Array2;

use crate::{FRAME_SAMPLES, Frame, Stage};

/// DeepFilterNet3 降噪。
pub struct Dfn3 {
    df: Box<DfTract>,
    noisy: Array2<f32>,
    enhanced: Array2<f32>,
}

impl Dfn3 {
    /// 用内置模型与默认参数构造。
    ///
    /// 构造要加载 3 个 ONNX 模型（几十毫秒到几百毫秒），**必须在音频线程之外**完成。
    #[must_use]
    pub fn new() -> Self {
        let df = Box::new(DfTract::default());
        assert_eq!(
            df.hop_size, FRAME_SAMPLES,
            "DFN3 的 hop_size {} 与处理帧长 {} 不一致",
            df.hop_size, FRAME_SAMPLES
        );
        assert_eq!(df.ch, 1, "只支持单声道，模型给出 {} 通道", df.ch);
        Self {
            df,
            noisy: Array2::zeros((1, FRAME_SAMPLES)),
            enhanced: Array2::zeros((1, FRAME_SAMPLES)),
        }
    }
}

impl Default for Dfn3 {
    fn default() -> Self {
        Self::new()
    }
}

impl Stage for Dfn3 {
    fn name(&self) -> &'static str {
        "dfn3"
    }

    fn latency_samples(&self) -> usize {
        self.df.delay()
    }

    fn process(&mut self, frame: &mut Frame) -> f32 {
        // 拷进模型输入（`process` 要两个独立的视图，不能原地）。
        self.noisy
            .row_mut(0)
            .assign(&ndarray::ArrayView::from(&frame[..]));

        // 出错时**不改动这一帧**（原样透传）：热路径里不能 panic，也不能记日志。
        if self
            .df
            .process(self.noisy.view(), self.enhanced.view_mut())
            .is_ok()
        {
            frame.copy_from_slice(
                self.enhanced
                    .row(0)
                    .as_slice()
                    .unwrap_or(&[0.0; FRAME_SAMPLES]),
            );
        }

        // 不提供 VAD（返回的是局部 SNR，不是语音概率）。
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SAMPLE_RATE;
    use crate::samples_to_ms;

    fn run(stage: &mut Dfn3, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::with_capacity(input.len());
        for chunk in input.as_chunks::<FRAME_SAMPLES>().0 {
            let mut frame: Frame = [0.0; FRAME_SAMPLES];
            frame.copy_from_slice(chunk);
            assert_eq!(stage.process(&mut frame), 1.0, "DFN3 不该提供 VAD");
            out.extend_from_slice(&frame);
        }
        out
    }

    #[test]
    fn delay_is_thirty_milliseconds() {
        // P3-3 验收：delay = 1440 样本 = 30 ms（技术设计 附录 A 第 3 行）。
        let stage = Dfn3::new();
        assert_eq!(stage.name(), "dfn3");
        assert_eq!(stage.latency_samples(), 1_440);
        assert_eq!(samples_to_ms(stage.latency_samples()), 30.0);
    }

    #[test]
    fn silence_in_silence_out() {
        let mut stage = Dfn3::new();
        let input = vec![0.0_f32; 30 * FRAME_SAMPLES];
        let out = run(&mut stage, &input);
        assert!(out.iter().all(|x| x.is_finite()), "输出出现 NaN/Inf");
        // 前 30 ms 是 STFT 填充，之后应当接近静音。
        let tail = &out[10 * FRAME_SAMPLES..];
        assert!(tail.iter().all(|x| x.abs() < 1e-4), "静音输入产生了输出");
    }

    #[test]
    fn output_is_finite_and_bounded() {
        // 满量程附近的方波：只验证不炸、不超界（音质要靠真实素材判，合成素材不算数）。
        let mut stage = Dfn3::new();
        let input: Vec<f32> = (0..30 * FRAME_SAMPLES)
            .map(|i| if (i / 240) % 2 == 0 { 0.9 } else { -0.9 })
            .collect();
        let out = run(&mut stage, &input);
        assert!(out.iter().all(|x| x.is_finite()));
        let peak = out.iter().fold(0.0_f32, |acc, x| acc.max(x.abs()));
        assert!(peak <= 2.0, "输出峰值 {peak}，模型输出没有被约束");
    }

    #[test]
    fn worker_wrapper_adds_the_queue_delay_on_top_of_thirty_ms() {
        // P3-7 的预算口径：DFN3 的 30 ms 是**算法**延迟，队列的 depth 帧是**另算**的。
        // 两个数都要进附录 A，只记一个就会少记 20 ms。
        use crate::stages::WorkerStage;
        let (mut stage, mut worker) = WorkerStage::new(Box::new(Dfn3::new()), 2);
        assert_eq!(
            stage.latency_samples(),
            1_440 + 2 * FRAME_SAMPLES,
            "30 ms 算法延迟 + 20 ms 队列延迟 = 50 ms"
        );

        // 跑几帧，确认真的能穿过队列拿到非零输出（也顺带证明 Dfn3 是 Send）。
        let mut got_output = false;
        for index in 0..8 {
            let mut frame: Frame = [0.0; FRAME_SAMPLES];
            for (n, sample) in frame.iter_mut().enumerate() {
                let t = (index * FRAME_SAMPLES + n) as f32 / SAMPLE_RATE as f32;
                *sample = 0.4 * (2.0 * std::f32::consts::PI * 300.0 * t).sin();
            }
            stage.process(&mut frame);
            worker.run_pending();
            if frame.iter().any(|x| x.abs() > 1e-3) {
                got_output = true;
            }
        }
        assert!(got_output, "队列里没出来任何有声帧");
        assert_eq!(stage.stats().underruns, 0, "手工驱动不该有下溢");
    }
}
