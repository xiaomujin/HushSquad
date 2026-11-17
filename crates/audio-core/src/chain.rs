//! 链路拼装与链路描述（P1-2）。
//!
//! 两个东西：
//!
//! - [`ChainDesc`]：**可复现的实验描述**。`"none"` / `"rnn"` / `"rnn,agc2"` / `"dfn3,agc2,opus"`，
//!   解析成档位列表并能算出总延迟。它让「同一个实验」可以写进报告、粘进 `measurements.md`、
//!   在 `testkit` 里复跑；
//! - [`Chain`]：真正跑帧的容器。构造期把各档 [`Stage`] 装进去，`process` 里按顺序原地处理。
//!
//! 延迟口径：`ChainDesc::total_latency_ms()` 与 `Chain::total_latency_ms()` 必须一致，
//! 否则「文档里写的链路」与「跑起来的链路」会对不上账。

use std::fmt;

use crate::{Frame, Stage, samples_to_ms};

/// 一个档位（可组合成链路的最小单位）。
///
/// `Opus` 不是 [`Stage`]（它吃帧吐字节），但它的 6.5 ms 前瞻同样是延迟预算的一行，
/// 所以也列在档位里——否则报告里 `algorithm_ms` 会少算 6.5 ms。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StageKind {
    /// 80 Hz 二阶高通（去隆隆声与桌面震动），0 延迟。
    Highpass,
    /// `sonora-agc2` 自适应增益 + 限幅，0 延迟。
    Agc2,
    /// RNNoise（`nnnoiseless`），10 ms 算法延迟。
    Rnn,
    /// DeepFilterNet3（`deep-filter-rt`），30 ms 算法延迟。
    Dfn3,
    /// Opus 编解码前瞻，6.5 ms（312 样本）。
    Opus,
}

impl StageKind {
    /// 全部合法档位名，用于报错时列出可选值。
    pub const ALL: [Self; 5] = [
        Self::Highpass,
        Self::Agc2,
        Self::Rnn,
        Self::Dfn3,
        Self::Opus,
    ];

    /// 档位名（与 CLI 的 `--chain`、报告 JSON、`Stage::name()` 保持一致）。
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Highpass => "highpass",
            Self::Agc2 => "agc2",
            Self::Rnn => "rnn",
            Self::Dfn3 => "dfn3",
            Self::Opus => "opus",
        }
    }

    /// 该档位引入的算法延迟（样本数）。
    #[must_use]
    pub fn latency_samples(self) -> usize {
        match self {
            Self::Highpass | Self::Agc2 => 0,
            Self::Rnn => 480,
            Self::Dfn3 => 1_440,
            Self::Opus => 312,
        }
    }

    /// 按名字解析档位。
    pub fn parse(name: &str) -> Result<Self, ChainParseError> {
        match name {
            "highpass" => Ok(Self::Highpass),
            "agc2" => Ok(Self::Agc2),
            "rnn" => Ok(Self::Rnn),
            "dfn3" => Ok(Self::Dfn3),
            "opus" => Ok(Self::Opus),
            other => Err(ChainParseError::UnknownStage {
                name: other.to_owned(),
            }),
        }
    }
}

/// 链路描述解析失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainParseError {
    /// 出现了未知档位名。
    UnknownStage {
        /// 原始输入里的那一段。
        name: String,
    },
}

impl fmt::Display for ChainParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownStage { name } => {
                let valid: Vec<&str> = StageKind::ALL.iter().map(|k| k.name()).collect();
                write!(f, "未知档位 {name:?}，可选值：{}", valid.join(", "))
            }
        }
    }
}

impl std::error::Error for ChainParseError {}

/// 按描述构造链路时的失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainBuildError {
    /// 该档位在 `ChainDesc` 里合法，但还没有实现（P3 逐步补齐）。
    Unimplemented {
        /// 档位名。
        name: &'static str,
    },
}

impl fmt::Display for ChainBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unimplemented { name } => write!(f, "档位 {name} 还没有实现"),
        }
    }
}

impl std::error::Error for ChainBuildError {}

/// 链路描述：一串档位。空链路用 `"none"` 表示。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChainDesc {
    stages: Vec<StageKind>,
}

impl ChainDesc {
    /// 解析 `"none"` / `"rnn"` / `"rnn,agc2"` 这类描述。
    ///
    /// 容忍首尾空白与多余逗号（`"rnn,,agc2,"` 等价于 `"rnn,agc2"`）；
    /// 空串按 `"none"` 处理，方便 CLI 直接传空参数。
    pub fn parse(spec: &str) -> Result<Self, ChainParseError> {
        let spec = spec.trim();
        if spec.is_empty() || spec == "none" {
            return Ok(Self::default());
        }

        let mut stages = Vec::new();
        for part in spec.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            stages.push(StageKind::parse(part)?);
        }
        Ok(Self { stages })
    }

    /// 档位列表。
    #[must_use]
    pub fn stages(&self) -> &[StageKind] {
        &self.stages
    }

    /// 链路总算法延迟（样本数）。
    #[must_use]
    pub fn total_latency_samples(&self) -> usize {
        self.stages.iter().map(|k| k.latency_samples()).sum()
    }

    /// 链路总算法延迟（毫秒）。
    #[must_use]
    pub fn total_latency_ms(&self) -> f32 {
        samples_to_ms(self.total_latency_samples())
    }

    /// 报告用的 JSON 数组（如 `["rnn","agc2"]`）。
    ///
    /// 手写而不引 `serde`：只有一层字符串数组，没必要为它拉一整套序列化框架进实时 crate。
    #[must_use]
    pub fn to_json(&self) -> String {
        let items: Vec<String> = self
            .stages
            .iter()
            .map(|k| format!("\"{}\"", k.name()))
            .collect();
        format!("[{}]", items.join(","))
    }
}

impl fmt::Display for ChainDesc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.stages.is_empty() {
            return f.write_str("none");
        }
        let names: Vec<&str> = self.stages.iter().map(|k| k.name()).collect();
        f.write_str(&names.join(","))
    }
}

/// 真正跑帧的链路。
///
/// `Vec<Box<dyn Stage>>` 只在构造期可变，`process` 里只做遍历（v1 允许虚调用，先测出来再谈去虚化）。
pub struct Chain {
    stages: Vec<Box<dyn Stage>>,
}

impl Chain {
    /// 用已经构造好的阶段拼一条链。
    #[must_use]
    pub fn new(stages: Vec<Box<dyn Stage>>) -> Self {
        Self { stages }
    }

    /// 空链：只做直通，用于取「无处理」基线。
    #[must_use]
    pub fn empty() -> Self {
        Self { stages: Vec::new() }
    }

    /// 按 [`ChainDesc`] 构造链路。
    ///
    /// `opus` 档位被跳过：它是延迟预算的一行，不是 [`Stage`]（编码器在 `codec` 里）。
    /// 其它档位若还没实现，返回 [`ChainBuildError::Unimplemented`]——**不要**静默跳过，
    /// 否则报告里的链路与真正跑的链路不是一回事。
    pub fn from_desc(desc: &ChainDesc) -> Result<Self, ChainBuildError> {
        let mut chain = Self::empty();
        for kind in desc.stages() {
            if *kind == StageKind::Opus {
                continue;
            }
            let stage = crate::stages::build(*kind)
                .ok_or(ChainBuildError::Unimplemented { name: kind.name() })?;
            chain.push(stage);
        }
        Ok(chain)
    }

    /// 逐阶段计时版本，**只用于离线测量**（`testkit` 的逐阶段耗时）。
    ///
    /// `per_stage_ms` 与链路的阶段一一对应，每次调用往里累加该阶段的耗时（毫秒）。
    /// 生产路径请用 [`Chain::process`]：它不读时钟。
    pub fn process_profiled(&mut self, frame: &mut Frame, per_stage_ms: &mut [f64]) -> f32 {
        self.process_inner(frame, Some(per_stage_ms))
    }

    /// `process` 与 `process_profiled` 的唯一实现：VAD 传播规则只写一次。
    fn process_inner(&mut self, frame: &mut Frame, per_stage_ms: Option<&mut [f64]>) -> f32 {
        let mut vad = 1.0;
        let mut times = per_stage_ms;
        for (index, stage) in self.stages.iter_mut().enumerate() {
            let stage_vad = match times.as_deref_mut() {
                Some(slots) => {
                    let start = std::time::Instant::now();
                    let value = stage.process(frame);
                    if let Some(slot) = slots.get_mut(index) {
                        *slot += start.elapsed().as_secs_f64() * 1000.0;
                    }
                    value
                }
                None => stage.process(frame),
            };
            // **第一个**给出有效值的阶段胜出（见 `process` 的文档）。
            if vad == 1.0 && stage_vad != 1.0 {
                vad = stage_vad;
            }
        }
        vad
    }

    /// 在链尾追加一个阶段（仅构造期调用）。
    pub fn push(&mut self, stage: Box<dyn Stage>) {
        self.stages.push(stage);
    }

    /// 链路总算法延迟（样本数），由各阶段自报，**不是**配置里抄来的常数。
    #[must_use]
    pub fn total_latency_samples(&self) -> usize {
        self.stages.iter().map(|s| s.latency_samples()).sum()
    }

    /// 链路总算法延迟（毫秒）。
    #[must_use]
    pub fn total_latency_ms(&self) -> f32 {
        samples_to_ms(self.total_latency_samples())
    }

    /// 原地处理一帧，返回 VAD 概率。
    ///
    /// 约定 `1.0` 表示「本阶段没有提供 VAD」；返回值取**第一个给出有效值的阶段**，
    /// 全链都没有 VAD 时为 `1.0`。
    ///
    /// 为什么是第一个而不是最后一个：设计顺序是 高通 → 降噪 → AGC2，降噪排在前，
    /// 它的 VAD 是**为「这段是不是语音」训练的**，下游 DTX 与「谁在说话」要的正是这个判据；
    /// AGC2 内部的 RNN VAD 只服务于它自己的增益决策（而且它看的是降噪后的信号，判据被
    /// 降噪器改写过）。所以链路对外暴露降噪档的 VAD，而不是最后一级的。
    pub fn process(&mut self, frame: &mut Frame) -> f32 {
        self.process_inner(frame, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FRAME_SAMPLES, Frame};

    #[test]
    fn none_has_zero_latency() {
        let desc = ChainDesc::parse("none").expect("解析 none 失败");
        assert!(desc.stages().is_empty());
        assert_eq!(desc.total_latency_ms(), 0.0);
        assert_eq!(desc.to_string(), "none");
    }

    #[test]
    fn rnn_costs_ten_milliseconds() {
        // P1-2 验收：parse("rnn").total_latency_ms() == 10.0
        assert_eq!(ChainDesc::parse("rnn").unwrap().total_latency_ms(), 10.0);
    }

    #[test]
    fn full_chain_matches_design_budget() {
        // 技术设计 §6.3 的示例：rnn + agc2 + opus = 10 + 0 + 6.5 = 16.5 ms
        let desc = ChainDesc::parse("highpass,agc2,rnn,opus").unwrap();
        assert_eq!(desc.total_latency_ms(), 16.5);
        // DFN3 档：30 + 0 + 6.5 = 36.5 ms
        let quality = ChainDesc::parse("dfn3,agc2,opus").unwrap();
        assert_eq!(quality.total_latency_ms(), 36.5);
    }

    #[test]
    fn parser_tolerates_whitespace_and_empty_segments() {
        let desc = ChainDesc::parse("  rnn , , agc2 ,").unwrap();
        assert_eq!(desc.stages(), &[StageKind::Rnn, StageKind::Agc2]);
        assert_eq!(desc.to_string(), "rnn,agc2");
        assert_eq!(ChainDesc::parse("").unwrap(), ChainDesc::default());
    }

    #[test]
    fn unknown_stage_is_rejected_with_hint() {
        let err = ChainDesc::parse("rnn,sew-noise").unwrap_err();
        assert_eq!(
            err,
            ChainParseError::UnknownStage {
                name: "sew-noise".to_owned()
            }
        );
        let msg = err.to_string();
        assert!(msg.contains("rnn"), "报错应列出可选值：{msg}");
    }

    #[test]
    fn json_is_a_plain_string_array() {
        assert_eq!(ChainDesc::parse("none").unwrap().to_json(), "[]");
        assert_eq!(
            ChainDesc::parse("rnn,agc2").unwrap().to_json(),
            "[\"rnn\",\"agc2\"]"
        );
    }

    /// 测试用阶段：只加一个固定延迟并记录是否被调用，用来验证链的顺序与延迟求和。
    struct FakeStage {
        name: &'static str,
        latency: usize,
        vad: f32,
        calls: usize,
    }

    impl Stage for FakeStage {
        fn name(&self) -> &'static str {
            self.name
        }

        fn latency_samples(&self) -> usize {
            self.latency
        }

        fn process(&mut self, frame: &mut Frame) -> f32 {
            self.calls += 1;
            frame[0] += 1.0;
            self.vad
        }
    }

    #[test]
    fn from_desc_builds_implemented_stages_and_skips_opus() {
        let desc = ChainDesc::parse("highpass,agc2,opus").unwrap();
        let chain = Chain::from_desc(&desc).expect("highpass/agc2 已实现，应能构造");
        // opus 不是 Stage（编码器在 codec 里），所以链路自身延迟是 0；
        // 那 6.5 ms 记在 ChainDesc 的预算里，报告取的是 ChainDesc。
        assert_eq!(chain.total_latency_ms(), 0.0);
        assert_eq!(desc.total_latency_ms(), 6.5);
    }

    #[test]
    fn from_desc_refuses_unimplemented_stages() {
        let desc = ChainDesc::parse("dfn3").unwrap();
        let err = Chain::from_desc(&desc)
            .err()
            .expect("dfn3 尚未实现，构造应失败");
        assert_eq!(err, ChainBuildError::Unimplemented { name: "dfn3" });
    }

    #[test]
    fn profiled_processing_matches_plain_processing() {
        // 计时版本必须与生产路径逐位一致，否则 testkit 的数字说的不是同一件事。
        let desc = ChainDesc::parse("highpass").unwrap();
        let mut plain = Chain::from_desc(&desc).unwrap();
        let mut profiled = Chain::from_desc(&desc).unwrap();
        let mut times = [0.0_f64; 1];
        for i in 0..20 {
            let sample = (i as f32 * 0.1).sin();
            let mut a: Frame = [0.0; FRAME_SAMPLES];
            let mut b: Frame = [0.0; FRAME_SAMPLES];
            a[0] = sample;
            b[0] = sample;
            let vad_plain = plain.process(&mut a);
            let vad_profiled = profiled.process_profiled(&mut b, &mut times);
            assert_eq!(vad_plain, vad_profiled);
            assert_eq!(a, b);
        }
        assert!(times[0] >= 0.0, "计时为负");
    }

    #[test]
    fn first_vad_stage_wins() {
        // 两个都提供 VAD 的阶段：按设计顺序降噪在前，对外暴露的应是它的判据。
        let mut chain = Chain::new(vec![
            Box::new(FakeStage {
                name: "rnn",
                latency: 480,
                vad: 0.8,
                calls: 0,
            }),
            Box::new(FakeStage {
                name: "agc2",
                latency: 0,
                vad: 0.1,
                calls: 0,
            }),
        ]);
        let mut frame: Frame = [0.0; FRAME_SAMPLES];
        assert_eq!(chain.process(&mut frame), 0.8);
    }

    #[test]
    fn chain_sums_latency_and_propagates_vad() {
        let mut chain = Chain::new(vec![
            Box::new(FakeStage {
                name: "rnn",
                latency: 480,
                vad: 1.0,
                calls: 0,
            }),
            Box::new(FakeStage {
                name: "vad-source",
                latency: 0,
                vad: 0.25,
                calls: 0,
            }),
        ]);

        let mut frame: Frame = [0.0; FRAME_SAMPLES];
        assert_eq!(chain.process(&mut frame), 0.25);
        assert_eq!(frame[0], 2.0); // 两个阶段都跑过，顺序正确
        assert_eq!(chain.total_latency_ms(), 10.0);

        let mut empty = Chain::empty();
        let mut untouched: Frame = [0.0; FRAME_SAMPLES];
        assert_eq!(empty.process(&mut untouched), 1.0);
        assert_eq!(untouched[0], 0.0);
    }
}
