//! 处理链环节的具体实现（P3）。
//!
//! 每个环节都实现 [`Stage`]，因此能任意拼装、离线回归、并逐级记延迟。
//! 加新环节的规矩：先在本模块给出实现与单元测试，再在 [`crate::StageKind`] 里登记，
//! 最后才允许出现在 `ChainDesc` 的档位里——否则报告会写出一个跑不起来的链路。

pub mod agc2;
pub mod highpass;

pub use agc2::Agc2;
pub use highpass::Highpass;

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
        StageKind::Rnn | StageKind::Dfn3 | StageKind::Opus => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implemented_stages_report_their_kind_name() {
        for kind in [StageKind::Highpass, StageKind::Agc2] {
            let stage = build(kind).unwrap_or_else(|| panic!("{} 应该已实现", kind.name()));
            assert_eq!(stage.name(), kind.name());
            assert_eq!(stage.latency_samples(), kind.latency_samples());
        }
    }

    #[test]
    fn unimplemented_stages_are_not_built() {
        for kind in [StageKind::Rnn, StageKind::Dfn3, StageKind::Opus] {
            assert!(
                build(kind).is_none(),
                "{} 尚未实现，不该能构造",
                kind.name()
            );
        }
    }
}
