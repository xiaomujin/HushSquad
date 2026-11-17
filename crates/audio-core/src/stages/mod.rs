//! 处理链环节的具体实现（P3）。
//!
//! 每个环节都实现 [`Stage`]，因此能任意拼装、离线回归、并逐级记延迟。
//! 加新环节的规矩：先在本模块给出实现与单元测试，再在 [`crate::StageKind`] 里登记，
//! 最后才允许出现在 `ChainDesc` 的档位里——否则报告会写出一个跑不起来的链路。

pub mod agc2;
pub mod crossfade;
#[cfg(feature = "denoise-dfn3")]
pub mod dfn3;
pub mod highpass;
pub mod rnn;
pub mod worker;

pub use agc2::Agc2;
pub use crossfade::Crossfade;
#[cfg(feature = "denoise-dfn3")]
pub use dfn3::Dfn3;
pub use highpass::Highpass;
pub use rnn::Rnn;
pub use worker::{DEFAULT_DEPTH, StageWorker, WorkerStage, WorkerStatsSnapshot};

use crate::Stage;
use crate::chain::StageKind;

/// 按档位构造环节。
///
/// 返回 `None` 表示该档位**还没有实现**。`opus` 永远返回 `None`：它是延迟预算里的一行，
/// 不是 [`Stage`]（编码器吃帧吐字节，不满足 `Stage` 的签名）。
#[must_use]
pub fn build(kind: StageKind) -> Option<Box<dyn Stage + Send>> {
    match kind {
        StageKind::Highpass => Some(Box::new(Highpass::new())),
        StageKind::Agc2 => Some(Box::new(Agc2::new())),
        StageKind::Rnn => Some(Box::new(Rnn::new())),
        // 模型 8.4 MB + tract 运行时：只有打开 feature 才进二进制。
        StageKind::Dfn3 => build_dfn3(),
        StageKind::Opus => None,
    }
}

#[cfg(feature = "denoise-dfn3")]
fn build_dfn3() -> Option<Box<dyn Stage + Send>> {
    Some(Box::new(Dfn3::new()))
}

#[cfg(not(feature = "denoise-dfn3"))]
const fn build_dfn3() -> Option<Box<dyn Stage + Send>> {
    None
}

/// 哪些档位**必须**跑在工作线程上。
///
/// 判断依据只有一条：热路径会不会分配。DFN3 每帧约 185 次（C-20），其余环节都是 0 次。
/// 这个判断集中在一处，免得每个调用方各写一份 `if kind == Dfn3`。
#[must_use]
pub const fn needs_worker(kind: StageKind) -> bool {
    matches!(kind, StageKind::Dfn3)
}

/// 构造「音频侧 + 工作线程侧」这一对。
///
/// 不需要工作线程的档位返回 `None`——它们直接走 [`build`] 就行，多一层队列只是白白加延迟。
/// `depth` 同时是额外延迟帧数与线程容错帧数，见 [`worker`] 模块文档。
pub fn build_worker(kind: StageKind, depth: usize) -> Option<(WorkerStage, StageWorker)> {
    if !needs_worker(kind) {
        return None;
    }
    let stage = build(kind)?;
    Some(WorkerStage::new(stage, depth))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implemented_stages_report_their_kind_name() {
        #[cfg(feature = "denoise-dfn3")]
        let kinds = [
            StageKind::Highpass,
            StageKind::Agc2,
            StageKind::Rnn,
            StageKind::Dfn3,
        ];
        #[cfg(not(feature = "denoise-dfn3"))]
        let kinds = [StageKind::Highpass, StageKind::Agc2, StageKind::Rnn];
        for kind in kinds {
            let stage = build(kind).unwrap_or_else(|| panic!("{} 应该已实现", kind.name()));
            assert_eq!(stage.name(), kind.name());
            // 延迟由环节自己报，必须与档位登记的预算值一致（Dfn3 由库报 1440）。
            assert_eq!(stage.latency_samples(), kind.latency_samples());
        }
    }

    #[test]
    fn unimplemented_stages_are_not_built() {
        // `opus` 永远不是 Stage（编码器吃帧吐字节，不满足签名）。
        assert!(build(StageKind::Opus).is_none(), "opus 不该能构造");
    }

    #[test]
    fn dfn3_follows_the_feature_flag() {
        // feature 关掉时必须「明确不可用」，而不是静默给个直通。
        #[cfg(feature = "denoise-dfn3")]
        assert!(
            build(StageKind::Dfn3).is_some(),
            "feature 已开，dfn3 应可构造"
        );
        #[cfg(not(feature = "denoise-dfn3"))]
        assert!(
            build(StageKind::Dfn3).is_none(),
            "feature 未开，dfn3 不该能构造"
        );
    }

    #[test]
    fn only_the_allocating_stage_needs_a_worker() {
        // 判断依据只有一条：热路径会不会分配。DFN3 会（C-20），其余都是 0。
        assert!(needs_worker(StageKind::Dfn3));
        assert!(!needs_worker(StageKind::Highpass));
        assert!(!needs_worker(StageKind::Rnn));
        assert!(!needs_worker(StageKind::Agc2));
        assert!(!needs_worker(StageKind::Opus));

        // 不需要工作线程的档位不提供 worker 构造：多一层队列只是白白加 20 ms 延迟。
        assert!(build_worker(StageKind::Highpass, 2).is_none());
        assert!(build_worker(StageKind::Opus, 2).is_none());
    }

    #[test]
    fn worker_construction_follows_the_feature_flag() {
        #[cfg(feature = "denoise-dfn3")]
        {
            let (stage, _worker) = build_worker(StageKind::Dfn3, 2).expect("dfn3 应当能上工作线程");
            assert_eq!(stage.depth(), 2);
            assert_eq!(
                stage.latency_samples(),
                1_440 + 2 * crate::FRAME_SAMPLES,
                "30 ms 算法延迟 + 20 ms 队列延迟"
            );
        }
        #[cfg(not(feature = "denoise-dfn3"))]
        assert!(
            build_worker(StageKind::Dfn3, 2).is_none(),
            "feature 未开，不该能构造工作线程版 dfn3"
        );
    }
}
