//! 前端能调的命令 —— `ui/src/lib/ipc.ts` 里 `Backend` 接口在 Rust 这一侧的实现。
//!
//! # 命名
//!
//! 命令名是 snake_case（`join_channel`），参数名在 JS 侧用 camelCase（`channelId`），
//! 这是 Tauri 2 的默认约定。前端 `tauri-backend.ts` 是唯一对齐这两边的地方。
//!
//! # 哪些是真的、哪些还不是
//!
//! | 命令 | 状态 |
//! |------|------|
//! | `connect` / `disconnect` / `join_channel` | 真的（跑真客户端） |
//! | `set_self`（静音 / 闭听） | 真的（改混音器与上行） |
//! | `set_user`（音量 / 静音 / 独听） | 真的（改本地混音器那一路） |
//! | `list_devices` | 真的（cpal 枚举 + 采样率区间） |
//! | `selftest` | **还没有**：返回一条明确的错误，不假装在测 |
//!
//! `setVoice`（语音激活 / PTT / 常开）目前**没有**对应命令：上行还没有 VAD 门控，
//! 麦克风一直在发。这件事写进了 `docs/UI设计-v0.1.md` 的缺口表，不在界面里装样子。

use std::sync::{Arc, Mutex};

use hushsquad_client::config::Config;
use hushsquad_client::host::Command;
use serde::{Deserialize, Serialize};
use tauri::State;
use tauri::ipc::Channel;

use crate::session::{Event, Session};

/// 全局状态：最多一条会话。
#[derive(Clone, Default)]
pub struct AppState {
    session: Arc<Mutex<Option<Session>>>,
}

impl AppState {
    /// 给正在跑的会话发一条指令。
    fn send(&self, command: Command) -> Result<(), String> {
        let guard = self
            .session
            .lock()
            .map_err(|_| "会话状态锁中毒".to_owned())?;
        match guard.as_ref() {
            Some(session) if !session.is_finished() => {
                session.send(command);
                Ok(())
            }
            _ => Err("还没有连接".to_owned()),
        }
    }
}

// ---------------------------------------------------------------- 连接

/// 前端发来的连接请求。
///
/// 注意 `trust` 字段前端会带上（`{kind:'remember', fingerprint}` / `{kind:'reject'}`），
/// 这里**故意不接**：v1 一律走 TOFU（第一次记住指纹，变了就拒绝），没有让用户
/// 手抄指纹的界面。未知字段 serde 默认忽略。
#[derive(Debug, Deserialize)]
pub struct ConnectRequest {
    /// `主机[:端口]`。
    pub addr: String,
    /// 身份。
    pub identity: Identity,
}

/// 身份。
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Identity {
    /// 按设备号匿名登录。
    Anonymous,
    /// 账号 + 密码。
    Account {
        /// 用户名。
        user: String,
        /// 密码。
        password: String,
    },
}

impl ConnectRequest {
    /// 变成客户端的配置。
    fn into_config(self) -> Result<Config, String> {
        let addr = self.addr.trim().to_owned();
        if addr.is_empty() {
            return Err("服务器地址不能为空".to_owned());
        }
        // 界面模式：不打印统计行、不打印设备清单（控制台是给开发者的），
        // 但保留 5 s 一次的打点 —— 网络面板的 RTT 就来自它。
        let mut cfg = Config {
            quiet: true,
            stats_interval: None,
            probe_interval: Some(std::time::Duration::from_secs(5)),
            ..Config::default()
        };
        cfg.server = addr;
        match self.identity {
            Identity::Anonymous => cfg.anonymous = true,
            Identity::Account { user, password } => {
                cfg.user = user;
                cfg.password = password;
            }
        }
        Ok(cfg)
    }
}

/// 连服务器并进默认频道。握手在后台线程上做，进度经 `events` 推回来。
///
/// 已经有会话时先停掉它（最多等 2 秒）。
#[tauri::command]
pub async fn connect(
    state: State<'_, AppState>,
    request: ConnectRequest,
    events: Channel<Event>,
) -> Result<(), String> {
    let app = state.inner().clone();
    let cfg = request.into_config()?;
    tauri::async_runtime::spawn_blocking(move || {
        let mut slot = app
            .session
            .lock()
            .map_err(|_| "会话状态锁中毒".to_owned())?;
        if let Some(previous) = slot.take() {
            previous.stop();
        }
        *slot = Some(Session::start(cfg, events));
        Ok(())
    })
    .await
    .map_err(|error| format!("起会话失败：{error}"))?
}

/// 断开。最多等 2 秒，等不到就不等了（界面不该被一个卡住的握手冻住）。
#[tauri::command]
pub async fn disconnect(state: State<'_, AppState>) -> Result<(), String> {
    let app = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut slot = app
            .session
            .lock()
            .map_err(|_| "会话状态锁中毒".to_owned())?;
        if let Some(session) = slot.take() {
            session.stop();
        }
        Ok(())
    })
    .await
    .map_err(|error| format!("停会话失败：{error}"))?
}

/// 换频道。
#[tauri::command]
pub fn join_channel(state: State<'_, AppState>, channel_id: u32) -> Result<(), String> {
    state.send(Command::JoinChannel(channel_id))
}

/// 自己静音 / 闭听。
#[tauri::command]
pub fn set_self(
    state: State<'_, AppState>,
    mute: Option<bool>,
    deafen: Option<bool>,
) -> Result<(), String> {
    if let Some(mute) = mute {
        state.send(Command::SetMute(mute))?;
    }
    if let Some(deafen) = deafen {
        state.send(Command::SetDeafen(deafen))?;
    }
    Ok(())
}

/// 调某个人：本机音量 / 静音 / 独听。
#[tauri::command]
pub fn set_user(
    state: State<'_, AppState>,
    user_id: u32,
    volume: Option<u16>,
    mute: Option<bool>,
    solo: Option<bool>,
) -> Result<(), String> {
    state.send(Command::SetUser {
        user_id,
        volume,
        mute,
        solo,
    })
}

// ---------------------------------------------------------------- 信任

/// 忘掉这台服务器记住的证书指纹与免密 token。
///
/// 用户看到「指纹变了」的报错时，这是唯一的出路 —— 命令行里是 `--reset-trust`，
/// 界面上不能没有对应的按钮，否则那个错误就是死胡同。
#[tauri::command]
pub fn forget_trust(addr: String) -> Result<(), String> {
    let store = hushsquad_client::local::open(None)?;
    store
        .forget(&addr)
        .map_err(|error| format!("忘掉指纹失败：{error}"))?;
    store
        .forget_server_token(&addr)
        .map_err(|error| format!("忘掉免密 token 失败：{error}"))?;
    Ok(())
}

// ---------------------------------------------------------------- 设备

/// 一个音频设备。
#[derive(Debug, Serialize)]
pub struct Device {
    /// 设备名（cpal 给的名字，用户就是按它认设备的）。
    pub name: String,
    /// 是不是系统默认设备。
    pub is_default: bool,
    /// 支持的采样率（人读的一句话）。
    pub rates: String,
}

/// 输入 / 输出两组设备。
#[derive(Debug, Serialize)]
pub struct Devices {
    /// 输入（麦克风）。
    pub inputs: Vec<Device>,
    /// 输出（扬声器）。
    pub outputs: Vec<Device>,
}

/// 列设备。不需要登录，进设置页时就能调。
#[tauri::command]
pub fn list_devices() -> Result<Devices, String> {
    let devices = audio_io::list_devices().map_err(|error| error.to_string())?;
    let collect = |list: &[audio_io::DeviceInfo], input: bool| {
        list.iter()
            .map(|device| Device {
                name: device.name.clone(),
                is_default: device.is_default,
                rates: audio_io::rate_ranges(Some(&device.name), input).map_or_else(
                    |_| "采样率：读不出来".to_owned(),
                    |ranges| hushsquad_client::audio::describe_rates(&ranges),
                ),
            })
            .collect()
    };
    Ok(Devices {
        inputs: collect(&devices.inputs, true),
        outputs: collect(&devices.outputs, false),
    })
}

// ---------------------------------------------------------------- 自检

/// 数字 / 声学回环自检。
///
/// 测量代码在 `audio_io::impulse`，但还没做成「会话里的一次往返」——
/// 所以这里如实报错，而不是转一圈假装测过了。
#[tauri::command]
pub fn selftest(kind: String) -> Result<(), String> {
    Err(format!(
        "自检（{kind}）还没接到会话上：测量代码在 audio-io 的 impulse 模块里，\
         但「播一段、录回来、算延迟」这条链路还没拼进客户端"
    ))
}
