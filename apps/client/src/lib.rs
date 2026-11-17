//! HushSquad 客户端：TLS 控制面 + UDP 语音面 + 麦克风/扬声器。
//!
//! 这个 crate 是**唯一**把三块拼起来的地方：
//!
//! ```text
//! config ──▶ control（TLS、证书、导出器）
//!        ──▶ voice（UDP socket）
//!        ──▶ audio（采集/播放 + 混音器）
//!        ──▶ app（握手 + 10 ms 事件循环）
//! ```
//!
//! 三块各自不知道对方存在：`client-core` 是 sans-io 状态机，`audio-core` 不碰网络，
//! `audio-io` 不碰编解码。拼接的全部知识都在 [`app`] 里，所以换传输或换 UI 时
//! 只需要重写 [`app`]。
//!
//! # 已知缺口（写在这里而不是藏起来）
//!
//! - **设备必须 48 kHz**：不做重采样（`rubato` 已在依赖里，留给后续）；
//! - **音频流 Fatal 不自动重建**：设备被拔会报一条错误，之后这一路就没有声音了；
//! - **不做回声消除**：外放 + 麦克风会啸叫，请戴耳机；
//! - **没有 PTT / 权限 / 频道密码**：v1 的服务器还不校验它们。

#![deny(unsafe_code)]
// 这是命令行程序：输出就是它的功能。实时线程里的日志另有纪律（见 audio-core 文档）。
#![allow(clippy::print_stdout)]

pub mod app;
pub mod audio;
pub mod config;
pub mod control;
pub mod local;
pub mod timer;
pub mod voice;

pub use app::{App, PeerSummary, Summary, run};
pub use config::Config;
