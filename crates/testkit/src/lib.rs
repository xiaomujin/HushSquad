//! testkit 的可复用部分：报告结构、离线回放、素材生成、延迟模型。CLI 入口在 main.rs。
//!
//! 全项目只有这个 crate 允许打印（实时线程禁止日志），所以整个库统一放开 `print_stdout`。

#![allow(clippy::print_stdout)]
//!
//! 这些模块只在测量工具里用，**不**进实时链路：实时线程不允许碰文件、不允许分配。

pub mod alloc;
pub mod baseline;
pub mod drift;
pub mod gate;
pub mod lossmatrix;
pub mod model;
pub mod netsim;
pub mod p5bench;
pub mod report;
pub mod run;
pub mod synth;
pub mod wav;

/// 合成素材目录（`testkit gen` 的输出位置）。基线从这里读，路径只在这里定义一次。
pub const SYNTHETIC_DIR: &str = "testdata/synthetic";

/// 读 `--flag value` 的取值，并把游标推进一格。
pub fn next_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} 缺少取值"))
}

/// 分配计数器必须挂在库上：`cargo test` 的测试进程链接的是库，挂在 main.rs 里量不到。
#[global_allocator]
static GLOBAL: alloc::CountingAlloc = alloc::CountingAlloc;
