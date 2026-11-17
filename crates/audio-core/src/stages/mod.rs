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

pub use agc2::Agc2;
pub use crossfade::Crossfade;
#[cfg(feature = "denoise-dfn3")]
pub use dfn3::Dfn3;
pub use highpass::Highpass;
pub use rnn::Rnn;

use crate::Stage;
use crate::chain::StageKind;

/// 按档位构造环节。
///
/// 返回 `None` 表示该档位**还没有实现**。`opus` 永远返回 `None`：它是延迟预算里的一行，
/// 不是 [`Stage`]（编码器吃帧吐字节，不满足 `Stage` 的签名）。
#[must_use]
pub fn build(kind: StageKind) -> Option<Box<dyn Stage>> {
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
fn build_dfn3() -> Option<Box<dyn Stage>> {
    Some(Box::new(Dfn3::new()))
}

#[cfg(not(feature = "denoise-dfn3"))]
const fn build_dfn3() -> Option<Box<dyn Stage>> {
    None
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
}
