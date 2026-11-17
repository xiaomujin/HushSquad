//! 测量与验证工具（CLI）。
//!
//! 用法见 [`USAGE`]。真正的实现都在库里（lib.rs），这里只做参数分发——
//! 这样 `cargo test` 能直接调用 `run::run_samples` 验证「热路径零分配」。
//!
//! 本工具是唯一允许打印的地方，因此显式 allow `clippy::print_stdout`。

#![allow(clippy::print_stdout)]

use std::process::ExitCode;

const USAGE: &str = "\
HushSquad 离线测量工具\n\
\n\
用法：\n\
  testkit gen           --out <dir> [--seed 42] [--seconds 20]\n\
  testkit run           --in <wav> [--out <wav>] [--chain <desc>] [--codec <spec>] [--report <json>] [--warmup 50]\n\
  testkit latency-model [--chain <desc>] [--codec <spec>]\n\
\n\
链路描述 --chain：none | 逗号分隔的 highpass,agc2,rnn,dfn3,opus（已实现 none / highpass / agc2 / opus；rnn 待 P3-2、dfn3 待 P3-3）\n\
编解码   --codec：none | opus[:10ms][:32k]\n\
\n\
例：\n\
  cargo run -p testkit --release -- gen --out testdata/synthetic\n\
  cargo run -p testkit --release -- run --in testdata/synthetic/clean_48k_mono.wav --out out/proc.wav --chain opus --codec opus:10ms:32k --report out/report.json\n\
\n\
注意：耗时数字只在 --release 下有意义，debug 构建会在报告里标 profile=debug。";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("错误：{message}");
            eprintln!();
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("gen") => testkit::synth::main(&args[1..]),
        Some("run") => testkit::run::main(&args[1..]),
        Some("latency-model") => testkit::model::main(&args[1..]),
        Some("--help" | "-h") => {
            println!("{USAGE}");
            Ok(())
        }
        // 兼容执行方案 §14 里的写法：省略子命令，直接给 --in
        Some(flag) if flag.starts_with("--") => testkit::run::main(args),
        Some(other) => Err(format!("未知子命令 {other:?}")),
        None => {
            println!("{USAGE}");
            Ok(())
        }
    }
}
