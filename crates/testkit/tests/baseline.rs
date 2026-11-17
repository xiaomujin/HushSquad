//! P3-5 的自动闸门：离线回归基线。
//!
//! 基线存的是**确定性指标**（输出 RMS、包长、VAD 均值、算法延迟、热路径分配），不是 wav。
//! 任何一项超差都说明 DSP / 编码 / 链路被改动过——要么是 bug，要么是有意改动后忘了重新生成基线。
//!
//! 有意改动后重新生成：
//! `cargo run -p testkit -- baseline --out testdata/baseline.json`

use testkit::baseline::{Baseline, collect, compare, default_root};

#[test]
fn metrics_match_the_checked_in_baseline() {
    let root = default_root();
    let baseline_path = root
        .parent()
        .expect("素材目录应有父目录")
        .join("baseline.json");
    let text = std::fs::read_to_string(&baseline_path).unwrap_or_else(|e| {
        panic!(
            "读不到基线 {}：{e}\n先跑 cargo run -p testkit -- baseline --out testdata/baseline.json",
            baseline_path.display()
        )
    });
    let baseline: Baseline = serde_json::from_str(&text).expect("基线 JSON 解析失败");

    let current = collect(&root).expect("采集当前指标失败");
    let violations = compare(&baseline, &current);
    assert!(
        violations.is_empty(),
        "离线回归基线超差 {} 项（若是有意改动，请重新生成基线）：\n{}",
        violations.len(),
        violations
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    );
}
