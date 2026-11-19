//! 薄壳：把窗口跑起来。所有逻辑在 `hushsquad_app` 里（方便将来加无头测试）。

// Windows 上 release 版不要顺带弹出控制台窗口（调试版留着，方便看 panic）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    hushsquad_app::run();
}
