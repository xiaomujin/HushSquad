//! HushSquad 桌面外壳：窗口 + IPC + 会话线程。
//!
//! ```text
//! ui/dist ──(tauri://localhost)──▶ WebView2
//!                                     │ invoke
//!                                     ▼
//!                                  ipc.rs ──▶ session.rs ──▶ hushsquad-client
//!                                     ▲                          │
//!                                     └──── Channel<Event> ◀─────┘
//! ```
//!
//! 这个 crate 只做三件事：开窗口、把命令转成 `hushsquad_client` 的调用、
//! 把会话状态推回前端。**没有任何音频或网络逻辑**——那些都在 `apps/client` 里，
//! 这样命令行客户端和界面跑的是同一条链路（见 ADR-0014）。

mod ipc;
mod session;

use ipc::AppState;

/// 起窗口。阻塞到窗口关闭。
pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            ipc::connect,
            ipc::disconnect,
            ipc::join_channel,
            ipc::set_self,
            ipc::set_user,
            ipc::set_voice,
            ipc::set_ptt,
            ipc::move_user,
            ipc::set_user_state,
            ipc::kick_user,
            ipc::list_users,
            ipc::set_user_perms,
            ipc::create_channel,
            ipc::update_channel,
            ipc::move_channel,
            ipc::delete_channel,
            ipc::forget_trust,
            ipc::list_devices,
            ipc::selftest,
        ])
        .run(tauri::generate_context!())
        .expect("启动窗口失败");
}
