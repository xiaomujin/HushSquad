//! 测量与验证工具（CLI）。
//!
//! 目标用法（P1 落地）：
//!
//! ```text
//! cargo run -p testkit -- --in testdata/synthetic/clean.wav --out out/proc.wav \
//!   --chain rnn,agc2 --codec opus:10ms:32k --report out/report.json
//! ```
//!
//! 本工具是唯一允许打印的地方，因此显式 allow `clippy::print_stdout`。

#![allow(clippy::print_stdout)]

fn main() {
    // P0 骨架：只证明工具链与工作区能构建。真正的 CLI 在 P1-4 落地。
    println!("testkit {} — P0 骨架，尚未实现", env!("CARGO_PKG_VERSION"));
}
