//! 全局按键说话（B1）。
//!
//! 这个模块只管三件事：**一个键**、**什么时候注册它**、**存哪**。门控的判定在
//! `audio_core::TxGate`，音频与网络在 `apps/client`，这里只是把「按下 / 松开」变成
//! 两条 `Command::SetPtt`。
//!
//! # 为什么必须是全局热键
//!
//! 开黑时人在游戏里，窗口没有焦点。窗口内的 `keydown` 收不到，按住说话就等于没有。
//! 全局热键（Windows 上是 `RegisterHotKey`）由系统在**任何**前台窗口下投递给我们。
//!
//! # 代价必须写在明处
//!
//! 注册之后这个组合会被系统从所有程序手里抢走：设成 `Space`，任何地方都打不出空格。
//! 所以默认给的是 `Ctrl+Shift+Space`，界面对「没有修饰键」的键给红字警告，但仍然允许——
//! 有些人就是要用 `V` 或鼠标侧键那种手感，那是他的机器。
//!
//! # 为什么只在「按住说话」模式下注册
//!
//! 按键是按住说话这个模式的输入设备，不是所有模式的旁路（`audio_core::gate` 的模块
//! 文档解释了为什么改掉「三种模式都能强制发送」）。别的模式下**连注册都不做**：
//! 不注册就不会抢键，也就不可能「在语音激活里按住键强行发出去」。
//!
//! # 两个必须记住的坑
//!
//! 1. **不能在主线程上调 `register` / `unregister`**：插件把它们转到主线程执行并
//!    `recv()` 等待，而主线程正卡在这一句上 —— 直接死锁。所以所有注册都发生在
//!    `async` 命令（跑在 tokio 工作线程）里，热键回调里那一次「会话没了就注销」
//!    也另起一个线程去做。
//! 2. **按下事件在主线程、松开事件在另一个线程**：`global-hotkey` 收到 `WM_HOTKEY`
//!    后在主线程发「按下」，再起一个线程每 50 ms 轮询 `GetAsyncKeyState` 直到松手。
//!    所以松开最多晚 50 ms 才到 —— 对按住说话是听不出来的尾巴，但要写进文档，
//!    免得有人拿它去量延迟。

use std::str::FromStr;

use hushsquad_client::VoiceMode;
use hushsquad_client::host::Command;
use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_global_shortcut::{
    Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutEvent, ShortcutState,
};

use crate::ipc::AppState;

/// 默认键。带修饰键是刻意的：裸键会把那个键从所有程序手里抢走。
pub const DEFAULT_KEY: &str = "Ctrl+Shift+Space";

/// 本地库里的键名。
pub(crate) const STORE_KEY: &str = "ptt_key";

/// 一个解析好的按键绑定。
#[derive(Debug, Clone)]
pub struct KeyBinding {
    shortcut: Shortcut,
    /// 界面上显示的写法。
    display: String,
}

impl KeyBinding {
    /// 解析用户给的组合。写法与插件一致（`Ctrl+Shift+Space` / `Alt+F13` / `V`）。
    pub fn parse(spec: &str) -> Result<Self, String> {
        let trimmed = spec.trim();
        if trimmed.is_empty() {
            return Err("按键不能为空".to_owned());
        }
        let shortcut = Shortcut::from_str(trimmed)
            .map_err(|error| format!("认不出这个按键组合「{trimmed}」：{error}"))?;
        Ok(Self {
            display: format_shortcut(&shortcut),
            shortcut,
        })
    }

    /// 界面显示用的写法（`Ctrl+Shift+Space`）。
    #[must_use]
    pub fn display(&self) -> &str {
        &self.display
    }

    /// 存进库里、也从库里读出来的写法。
    ///
    /// 与 [`Self::display`] 分开：显示要好看（`Win`、`A`、`1`），存取要能被插件原样
    /// 解析回去（`super`、`KeyA`、`Digit1`）。用同一个字符串迟早会在某个键上对不上。
    #[must_use]
    pub fn spec(&self) -> String {
        self.shortcut.into_string()
    }

    /// 有没有修饰键。没有就意味着「这个键在所有程序里都没了」。
    #[must_use]
    pub fn has_modifier(&self) -> bool {
        !self.shortcut.mods.is_empty()
    }

    /// 插件要的那一份。
    #[must_use]
    pub fn shortcut(&self) -> Shortcut {
        self.shortcut
    }
}

impl Default for KeyBinding {
    fn default() -> Self {
        Self::parse(DEFAULT_KEY).expect("默认键必须能解析")
    }
}

/// 人读的写法：修饰键固定顺序，字母去掉 `Key` 前缀，数字去掉 `Digit` 前缀。
fn format_shortcut(shortcut: &Shortcut) -> String {
    let mut text = String::new();
    for (flag, name) in [
        (Modifiers::CONTROL, "Ctrl"),
        (Modifiers::ALT, "Alt"),
        (Modifiers::SHIFT, "Shift"),
        (Modifiers::SUPER, "Win"),
    ] {
        if shortcut.mods.contains(flag) {
            text.push_str(name);
            text.push('+');
        }
    }
    text.push_str(&pretty_key(shortcut.key));
    text
}

fn pretty_key(code: Code) -> String {
    let name = code.to_string();
    if let Some(rest) = name.strip_prefix("Key") {
        return rest.to_owned();
    }
    if let Some(rest) = name.strip_prefix("Digit") {
        return rest.to_owned();
    }
    name
}

/// 现在该不该注册这个键。
///
/// 两个条件都要：**按住说话模式**、**会话还活着**。掉线之后按键没有意义，
/// 留着注册只会让那个键在别的程序里也用不了。
#[must_use]
pub fn wanted(state: &AppState) -> bool {
    state.voice_mode() == VoiceMode::Ptt && state.session_alive()
}

/// 让「注册状态」追上「该不该注册」。幂等：状态一致时什么都不做。
///
/// **只能在非主线程上调**（见模块文档的坑 1）。
pub fn sync<R: Runtime>(app: &AppHandle<R>, state: &AppState) -> Result<(), String> {
    let want = wanted(state);
    if want == state.ptt_registered() {
        return Ok(());
    }
    let binding = state.ptt_key();
    let shortcuts = app.global_shortcut();
    if want {
        shortcuts.register(binding.shortcut()).map_err(|error| {
            format!(
                "注册全局按键「{}」失败：{error}。多半是被别的程序占了，换一个组合试试。",
                binding.display()
            )
        })?;
        state.set_ptt_registered(true);
    } else {
        // 注销失败不报错：进程退出时系统自己会收回，而且这时候报错也没人能处理。
        let _ = shortcuts.unregister(binding.shortcut());
        state.set_ptt_registered(false);
    }
    Ok(())
}

/// 热键回调。按下 / 松开各来一次。
///
/// 跑在主线程（按下）或插件的轮询线程（松开）上，所以这里只做一件事：
/// 把状态推给会话。**不做注册类操作**（会死锁）。
pub fn on_shortcut<R: Runtime>(app: &AppHandle<R>, event: ShortcutEvent) {
    let held = event.state == ShortcutState::Pressed;
    let state = app.state::<AppState>();
    if state.send(Command::SetPtt(held)).is_ok() {
        return;
    }
    // 会话已经没了：这个键留着没用，收回去。注销必须换线程做（坑 1）。
    state.set_ptt_registered(false);
    let app = app.clone();
    std::thread::spawn(move || {
        let _ = app.global_shortcut().unregister_all();
    });
}

/// 从本地库读回上次设的键。读不到就用默认值 —— 一个存坏的键不该让程序起不来。
pub fn load(state: &AppState) {
    let spec = match hushsquad_client::local::open(None).and_then(|store| {
        store
            .setting(STORE_KEY)
            .map_err(|error| format!("读按键设置失败：{error}"))
    }) {
        Ok(Some(spec)) => spec,
        Ok(None) => return,
        Err(error) => {
            eprintln!("{error}，按键用默认值");
            return;
        }
    };
    match KeyBinding::parse(&spec) {
        Ok(binding) => state.set_ptt_key(binding),
        Err(error) => eprintln!("库里存的按键「{spec}」不认识了，用默认值：{error}"),
    }
}

/// 存下当前键。
pub fn save(binding: &KeyBinding) -> Result<(), String> {
    let store = hushsquad_client::local::open(None)?;
    store
        .set_setting(STORE_KEY, &binding.spec())
        .map_err(|error| format!("存按键失败：{error}"))
}

/// 会话线程结束之后调用：把注册状态清掉。
///
/// 会话是自己结束的（服务器关了、握手失败）时候，界面那边只会看到 `phase = error`，
/// 没人会记得来注销热键。所以在会话收尾的地方调一下。
pub fn on_session_finished<R: Runtime>(app: &AppHandle<R>) {
    let state = app.state::<AppState>();
    if !state.ptt_registered() {
        return;
    }
    state.set_ptt_registered(false);
    let app = app.clone();
    std::thread::spawn(move || {
        let _ = app.global_shortcut().unregister_all();
    });
}

/// 给前端的一份快照。
#[derive(Debug, Clone, serde::Serialize)]
pub struct PttView {
    /// 显示用的写法（`Ctrl+Shift+Space`）。
    pub key: String,
    /// 有没有修饰键。没有的话界面要红字提醒「这个键会被抢走」。
    pub has_modifier: bool,
    /// 现在注册着吗（= 按住说话模式 + 已连接）。
    pub registered: bool,
}

/// 读当前设置。
#[must_use]
pub fn view(state: &AppState) -> PttView {
    let binding = state.ptt_key();
    PttView {
        key: binding.display().to_owned(),
        has_modifier: binding.has_modifier(),
        registered: state.ptt_registered(),
    }
}
