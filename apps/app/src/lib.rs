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
mod ptt;
mod session;
mod supervisor;

use ipc::AppState;
use tauri::Manager;

/// 起窗口。阻塞到窗口关闭。
pub fn run() {
    tauri::Builder::default()
        // 全局按键说话（B1）：按下 / 松开都从系统热键来，这样窗口没有焦点（人在游戏里）
        // 也能说话。为什么只在「按住说话」模式下注册，见 `ptt.rs`。
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| ptt::on_shortcut(app, event))
                .build(),
        )
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            ipc::connect,
            ipc::disconnect,
            ipc::reconnect,
            ipc::join_channel,
            ipc::set_self,
            ipc::set_user,
            ipc::set_voice,
            ipc::set_ptt,
            ipc::ptt_settings,
            ipc::set_ptt_key,
            ipc::load_settings,
            ipc::save_setting,
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
        .setup(|app| {
            // 读回上次设的键。**不能在这里注册热键**：插件的 register 会把活转回主线程
            // 并阻塞等待结果，而我们此刻就在主线程上 —— 会死锁（`ptt.rs` 模块文档坑 1）。
            // 真正的注册发生在连接之后（那时候已经知道说话方式了）。
            let state = app.state::<AppState>().inner().clone();
            ptt::load(&state);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("启动窗口失败");
}
