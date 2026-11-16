//! HushSquad 音频处理核心。
//!
//! 本 crate 只吃 `[f32; 480]`、吐 `[f32; 480]`，**不依赖 I/O 与网络**，因此可以在 CI 里离线跑。
//!
//! # 实时纪律（任何 PR 违反都会被直接打回）
//!
//! 1. 热路径（音频回调与每帧处理）内**禁止**堆分配：不许 `Vec::push` 扩容、`format!`、
//!    `String`、`Box::new`、`collect`。所有缓冲在构造期预分配。
//! 2. 热路径内**禁止**加锁、禁止阻塞系统调用、禁止日志（`log::info!` 也会格式化）。
//! 3. 热路径内**禁止** panic：索引用 `get`/`chunks_exact`，除法前检查分母。
//! 4. 任何阶段都必须正确报告 `latency_samples()`，否则延迟预算表会静默失真。
//!
//! # 与其它模块的关系
//!
//! 依赖方向是单向的：`audio-core` 不认识 `transport`、`protocol`、`client-core`。
//! 这样才能把整套前处理平移到 LiveKit 或 QUIC 之上（见技术设计 §4.2）。

#![deny(unsafe_code)] // 确需 SIMD 等优化时，在具体项上 #[allow(unsafe_code)] 并写明理由
