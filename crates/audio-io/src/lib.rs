//! 平台音频 I/O 适配层。
//!
//! 桌面与 Android 都走 `cpal`（0.18 已有 `cfg(target_os = android)` 的 AAudio 后端）。
//! 平台差异（权限、前台服务、音频路由、蓝牙 SCO）收敛在本 crate，不进热路径。
//!
//! 回调纪律：只做 `rtrb` 的 push/pop，缓冲不足时播放写静音、采集丢最旧，并各自计数 `xruns`。
