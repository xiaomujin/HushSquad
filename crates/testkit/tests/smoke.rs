//! 工作区冒烟测试：证明测试链路可用。真正的测试在 P1 起逐阶段添加。

#[test]
fn workspace_builds_and_tests_run() {
    // 10 ms @ 48 kHz = 480 样本（技术设计 §6.1 的统一参数）
    let sample_rate: u32 = 48_000;
    let frame_ms: u32 = 10;
    assert_eq!(sample_rate / 1000 * frame_ms, 480);
}
