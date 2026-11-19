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
//! | `set_voice`（语音激活 / 按住说话 / 常开） | 真的（上行门控，`audio_core::TxGate`） |
//! | `set_ptt`（按住 / 松开按键） | 真的（同上）。真机上由全局热键直接驱动，见 `ptt.rs` |
//! | `ptt_settings` / `set_ptt_key`（按键绑定） | 真的（全局热键 + 落进本地库） |
//! | `list_devices` | 真的（cpal 枚举 + 采样率区间） |
//! | `selftest` | **还没有**：返回一条明确的错误，不假装在测 |

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use hushsquad_client::VoiceMode;
use hushsquad_client::config::Config;
use hushsquad_client::host::Command;
use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tauri::{AppHandle, State};
use tauri_plugin_global_shortcut::GlobalShortcutExt;

use crate::ptt::{self, KeyBinding};
use crate::session::{Event, Session};
use crate::supervisor;

/// 全局状态：最多一条会话 + 上行设置。
///
/// 上行设置（说话方式、按键）放在这里而不是只放在会话里，是因为**热键注册与否由它们
/// 决定**：换到语音激活要立刻把键收回来，断线也要。会话对象随时可能被换掉（重连会
/// 新建一个），而这两个字段要跨会话存活。
#[derive(Clone)]
pub struct AppState {
    session: Arc<Mutex<Option<Session>>>,
    voice: Arc<Mutex<VoiceMode>>,
    ptt: Arc<Mutex<KeyBinding>>,
    /// 键现在注册着吗。与 `ptt` 分开：键可以改，注册状态是运行时事实。
    ptt_registered: Arc<AtomicBool>,
    /// 重连监督线程用的共享标志（代次 / 断开 / 立即重试 / 压掉重连期间的错误）。
    pub(crate) signals: Arc<supervisor::Signals>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            session: Arc::default(),
            voice: Arc::new(Mutex::new(VoiceMode::Vad)),
            ptt: Arc::new(Mutex::new(KeyBinding::default())),
            ptt_registered: Arc::default(),
            signals: Arc::default(),
        }
    }
}

impl AppState {
    /// 给正在跑的会话发一条指令。
    pub(crate) fn send(&self, command: Command) -> Result<(), String> {
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

    /// 会话还活着吗。
    pub(crate) fn session_alive(&self) -> bool {
        self.session
            .lock()
            .map(|guard| guard.as_ref().is_some_and(|session| !session.is_finished()))
            .unwrap_or(false)
    }

    /// 当前说话方式。
    pub(crate) fn voice_mode(&self) -> VoiceMode {
        self.voice.lock().map(|mode| *mode).unwrap_or_default()
    }

    pub(crate) fn set_voice_mode(&self, mode: VoiceMode) {
        if let Ok(mut slot) = self.voice.lock() {
            *slot = mode;
        }
    }

    /// 当前按键绑定（克隆：调用方拿去注册，不持有锁）。
    pub(crate) fn ptt_key(&self) -> KeyBinding {
        self.ptt
            .lock()
            .map(|binding| binding.clone())
            .unwrap_or_default()
    }

    pub(crate) fn set_ptt_key(&self, binding: KeyBinding) {
        if let Ok(mut slot) = self.ptt.lock() {
            *slot = binding;
        }
    }

    pub(crate) fn ptt_registered(&self) -> bool {
        self.ptt_registered.load(Ordering::Acquire)
    }

    pub(crate) fn set_ptt_registered(&self, registered: bool) {
        self.ptt_registered.store(registered, Ordering::Release);
    }

    /// 换掉当前会话（重连用）。旧的那个已经自己退出了。
    pub(crate) fn replace_session(&self, session: Session) {
        if let Ok(mut slot) = self.session.lock() {
            *slot = Some(session);
        }
    }

    /// 重连监督线程的代次。
    pub(crate) fn generation(&self) -> u64 {
        self.signals.generation()
    }

    /// 用户要断开。
    pub(crate) fn is_stopping(&self) -> bool {
        self.signals.stopping()
    }

    /// 取走「立即重试」这一次请求。
    pub(crate) fn take_retry_now(&self) -> bool {
        self.signals.take_retry_now()
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
    /// 上行语音设置。界面一进来就是「语音激活」，所以这个值要跟着连接一起
    /// 下去——不能等用户去设置页点一下才开始门控。
    pub voice: Option<VoiceSettings>,
}

/// 上行语音设置。字段名与 `ui/src/lib/ipc.ts` 的 `VoiceSettings` 一致。
#[derive(Debug, Clone, Deserialize)]
pub struct VoiceSettings {
    /// `vad` / `ptt` / `open`。
    pub mode: String,
    /// 能量门限（dBFS）。界面滑块给的是整数，这里仍按浮点收，免得 -37.5 直接反序列化失败。
    pub threshold_db: f32,
    /// 开门时补发几帧。
    pub preroll_frames: u32,
}

impl VoiceSettings {
    /// 变成一条会话指令。名字不认识就报错，不静默退回默认值——
    /// 静默退回会让界面上选的东西与实际行为不一致，那种 bug 最难查。
    fn into_command(self) -> Result<Command, String> {
        Ok(Command::SetVoice {
            mode: VoiceMode::parse(&self.mode)?,
            // 限幅到门控有意义的一小段，避免界面传进来一个 +20 或 -200。
            threshold_db: self.threshold_db.clamp(-90.0, 0.0).round() as i16,
            preroll_frames: self.preroll_frames.min(10),
        })
    }

    /// 应用到客户端配置（连接前就要生效的那一份）。
    fn apply(self, cfg: &mut Config) -> Result<(), String> {
        cfg.voice = VoiceMode::parse(&self.mode)?;
        cfg.vad_threshold_db = self.threshold_db.clamp(-90.0, 0.0);
        cfg.preroll_frames = self.preroll_frames.min(10);
        Ok(())
    }
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
            // 产品默认是语音激活（命令行默认常开，见 config.rs 的说明）。
            voice: VoiceMode::Vad,
            ..Config::default()
        };
        if let Some(voice) = self.voice {
            voice.apply(&mut cfg)?;
        }
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
    app: AppHandle,
    state: State<'_, AppState>,
    request: ConnectRequest,
    events: Channel<Event>,
) -> Result<(), String> {
    let inner = state.inner().clone();
    let cfg = request.into_config()?;
    let mode = cfg.voice;
    let notice = events.clone();
    // 监督线程要用同一份配置重起会话（服务器地址、证书、身份、音频设备都在里面）。
    let cfg_for_watch = cfg.clone();
    let for_blocking = inner.clone();
    let for_blocking_app = app.clone();
    // 新会话一开就把代次加一：上一条监督线程看到代次变了会自己退场，
    // 免得用户手动重连之后有两条线程抢着起会话。
    inner.signals.set_stopping(false);
    inner.signals.set_suppress_errors(false);
    inner.signals.bump();
    let signals = Arc::clone(&inner.signals);
    let watcher = tauri::async_runtime::spawn_blocking(move || -> Result<_, String> {
        let mut slot = for_blocking
            .session
            .lock()
            .map_err(|_| "会话状态锁中毒".to_owned())?;
        if let Some(previous) = slot.take() {
            previous.stop();
        }
        let session = Session::start(cfg, events, for_blocking_app, signals);
        let watcher = session.watcher();
        *slot = Some(session);
        Ok(watcher)
    })
    .await
    .map_err(|error| format!("起会话失败：{error}"))??;

    // 掉线之后自动重连（B2）：监督线程看着这条会话，断了就重起一条。
    supervisor::watch(
        inner.clone(),
        cfg_for_watch,
        notice.clone(),
        watcher,
        app.clone(),
    );

    // 说话方式跟着连接一起生效：按住说话要在连上之后才注册热键（没连接时按键没意义）。
    inner.set_voice_mode(mode);
    if let Err(error) = ptt::sync(&app, &inner) {
        // 注册失败不该让连接失败：会话已经在跑了，能听能说，只是没有按键。
        let _ = notice.send(Event::Notice { message: error });
    }
    Ok(())
}

/// 断开。最多等 2 秒，等不到就不等了（界面不该被一个卡住的握手冻住）。
#[tauri::command]
pub async fn disconnect(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let inner = state.inner().clone();
    // 先告诉监督线程「别重连了」，再停会话：反过来的话它会看到会话结束、
    // 以为掉线了，立刻起一条新的。
    inner.signals.set_stopping(true);
    let for_blocking = inner.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let mut slot = for_blocking
            .session
            .lock()
            .map_err(|_| "会话状态锁中毒".to_owned())?;
        if let Some(session) = slot.take() {
            session.stop();
        }
        Ok(())
    })
    .await
    .map_err(|error| format!("停会话失败：{error}"))??;
    // 会话没了，热键也该收回去（否则那个键在别的程序里会继续被吃掉）。
    ptt::sync(&app, &inner)
}

/// 立刻重连，不等退避睡完（界面上那颗「立即重试」）。
///
/// 只是把监督线程的闹钟拨快：真正起会话的仍然是它，所以不会出现两条线程抢着连。
/// 没有在重连时调它也不报错——那是界面上的按钮被点快了，不是错误。
#[tauri::command]
pub fn reconnect(state: State<'_, AppState>) -> Result<(), String> {
    state.inner().signals.request_retry();
    Ok(())
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

/// 换说话方式与门限。
///
/// 顺带同步热键：换到按住说话要注册，换走要注销。注册失败（键被别人占了）会作为
/// 错误返回，界面把原因显示在设置页上——**不静默**，否则用户会以为按住说话坏了。
#[tauri::command]
pub async fn set_voice(
    app: AppHandle,
    state: State<'_, AppState>,
    mode: String,
    threshold_db: f32,
    preroll_frames: u32,
) -> Result<(), String> {
    let settings = VoiceSettings {
        mode,
        threshold_db,
        preroll_frames,
    };
    let mode = VoiceMode::parse(&settings.mode)?;
    let command = settings.into_command()?;
    state.send(command)?;
    state.set_voice_mode(mode);
    ptt::sync(&app, state.inner())
}

/// 按住 / 松开。
///
/// 真机上这条命令**不是**按键的入口：全局热键在 Rust 侧直接推 `Command::SetPtt`
/// （见 `ptt.rs`），因为窗口没焦点时前端收不到 keydown。留着它是给浏览器 Mock 与
/// 脚本联调用的——两边走的是同一个门控。
#[tauri::command]
pub fn set_ptt(state: State<'_, AppState>, held: bool) -> Result<(), String> {
    state.send(Command::SetPtt(held))
}

// ---------------------------------------------------------------- 按键说话（B1）

/// 当前的按键绑定与注册状态。
#[tauri::command]
pub fn ptt_settings(state: State<'_, AppState>) -> ptt::PttView {
    ptt::view(state.inner())
}

/// 换一个按键，返回规范化之后的写法。
///
/// 三步都可能失败，且失败点不同，所以错误文案要分清楚：认不出组合 / 键被别的程序
/// 占了 / 存不下来。中间那步失败会把旧键装回去，不留一个「看起来改了其实没改」的状态。
#[tauri::command]
pub async fn set_ptt_key(
    app: AppHandle,
    state: State<'_, AppState>,
    key: String,
) -> Result<String, String> {
    let inner = state.inner().clone();
    let binding = KeyBinding::parse(&key)?;
    let previous = inner.ptt_key();
    if inner.ptt_registered() {
        // 旧的还注册着：先收回来，否则新旧两个键会同时有效。
        let _ = app.global_shortcut().unregister(previous.shortcut());
        inner.set_ptt_registered(false);
    }
    inner.set_ptt_key(binding.clone());
    if let Err(error) = ptt::sync(&app, &inner) {
        inner.set_ptt_key(previous);
        let _ = ptt::sync(&app, &inner);
        return Err(error);
    }
    ptt::save(&binding)
        .map_err(|error| format!("按键已经生效，但{error}（下次启动会回到默认）"))?;
    Ok(binding.display().to_owned())
}

// ---------------------------------------------------------------- 设置落盘（B4）

/// 读出本机全部设置。界面启动时读一次。
///
/// 键值对而不是一个结构体：偏好的项会一项一项长出来，每加一项就改一次两边结构体，
/// 迟早对不上。缺的项由界面用自己的默认值补——这里**不**替它编默认值。
#[tauri::command]
pub fn load_settings() -> Result<std::collections::BTreeMap<String, String>, String> {
    let store = hushsquad_client::local::open(None)?;
    let rows = store
        .settings()
        .map_err(|error| format!("读设置失败：{error}"))?;
    Ok(rows.into_iter().collect())
}

/// 存一项设置。
#[tauri::command]
pub fn save_setting(key: String, value: String) -> Result<(), String> {
    let store = hushsquad_client::local::open(None)?;
    store
        .set_setting(&key, &value)
        .map_err(|error| format!("存设置失败：{error}"))
}

// ---------------------------------------------------------------- 管理（ADR-0016）

/// 把别人移到另一个频道（需要 `MOVE_ANYONE`）。
#[tauri::command]
pub fn move_user(state: State<'_, AppState>, user_id: u32, channel_id: u32) -> Result<(), String> {
    state.send(Command::MoveUser {
        user_id,
        channel_id,
    })
}

/// 服务器静音 / 闭麦别人（需要 `MUTE_OTHERS`）。
#[tauri::command]
pub fn set_user_state(
    state: State<'_, AppState>,
    user_id: u32,
    server_mute: Option<bool>,
    server_deaf: Option<bool>,
) -> Result<(), String> {
    state.send(Command::SetUserState {
        user_id,
        server_mute,
        server_deaf,
    })
}

/// 踢出服务器（需要 `KICK`）。
#[tauri::command]
pub fn kick_user(state: State<'_, AppState>, user_id: u32, reason: String) -> Result<(), String> {
    state.send(Command::KickUser { user_id, reason })
}

/// 请求账号列表（需要 `ADMIN`）。结果走 `Event::Accounts`，不走返回值。
#[tauri::command]
pub fn list_users(state: State<'_, AppState>) -> Result<(), String> {
    state.send(Command::ListUsers)
}

/// 改某个账号的权限位（需要 `ADMIN`）。
///
/// 服务器会再校验一遍「授予的位必须是自己的子集」，所以这里不做二次判断——
/// 两处判断迟早会不一致，而**服务器那次才算数**。
#[tauri::command]
pub fn set_user_perms(state: State<'_, AppState>, user_id: u32, perms: u64) -> Result<(), String> {
    state.send(Command::SetUserPerms { user_id, perms })
}

// ---------------------------------------------------------------- 频道结构（P7-2）

/// 新建频道（需要 `CREATE_CHANNEL`）。`max_users = 0` 表示用服务器默认。
#[tauri::command]
pub fn create_channel(
    state: State<'_, AppState>,
    parent_id: u32,
    name: String,
    flags: u64,
    max_users: u32,
) -> Result<(), String> {
    state.send(Command::CreateChannel {
        parent_id,
        name,
        flags,
        max_users,
    })
}

/// 改名 / 人数上限 / 标志位（需要 `MANAGE_CHANNEL`）。`None` 表示不改这一项。
#[tauri::command]
pub fn update_channel(
    state: State<'_, AppState>,
    channel_id: u32,
    name: Option<String>,
    max_users: Option<u32>,
    flags: Option<u64>,
) -> Result<(), String> {
    state.send(Command::UpdateChannel {
        channel_id,
        name,
        max_users,
        flags,
    })
}

/// 把频道移到另一个父级的第 `position` 位（需要 `MANAGE_CHANNEL`）。
#[tauri::command]
pub fn move_channel(
    state: State<'_, AppState>,
    channel_id: u32,
    parent_id: u32,
    position: u32,
) -> Result<(), String> {
    state.send(Command::MoveChannel {
        channel_id,
        parent_id,
        position,
    })
}

/// 删除频道（连同子树，需要 `MANAGE_CHANNEL`）。服务器会拒绝删「里面还有人的」。
#[tauri::command]
pub fn delete_channel(state: State<'_, AppState>, channel_id: u32) -> Result<(), String> {
    state.send(Command::DeleteChannel { channel_id })
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
