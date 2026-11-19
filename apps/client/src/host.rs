//! 会话的观察点：把「打印给人看」换成「推给外壳」。
//!
//! # 为什么是 trait，不是回调闭包
//!
//! 外壳（Tauri）要的是三样东西，节奏各不相同：
//!
//! | 推送 | 频率 | 内容 |
//! |------|------|------|
//! | [`Host::state`] | 变化时（< 1 Hz） | 阶段、频道树、成员、自己的静音/闭听 |
//! | [`Host::meters`] | 20 Hz | 电平、谁在说话、上行有没有在发 |
//! | [`Host::net`] | 0.2 Hz | 抖动、丢包、RTT、码率 |
//!
//! 一个「每次都给全量快照」的闭包做不到这种分频，而全量快照按 20 Hz 推会白白搬
//! 几十个字符串。所以拆成三个方法，变化检测留在 [`crate::app`] 里做。
//!
//! # 这里**没有** tauri 依赖
//!
//! 视图类型只依赖 `serde`（外壳拿它直接序列化给前端）。这样 `apps/client` 仍是
//! 一个可以单独跑、单独测的 crate，换成别的外壳（Web / 移动 / 无头）也不用改这里。

use std::collections::BTreeMap;

use serde::Serialize;

/// 会话阶段。与 `ui/src/lib/ipc.ts` 的 `Phase` 一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// 还没连。
    #[default]
    Disconnected,
    /// TLS + 登录中。
    Authenticating,
    /// 登录成功、还没进频道。
    Connected,
    /// 正在进频道。
    Joining,
    /// 在频道里。
    InChannel,
    /// 断线重连（v1 还没有重连，留着占位）。
    Reconnecting,
    /// 出错了，`StateView::error` 里有原因。
    Error,
}

/// 频道树的一行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChannelView {
    /// 频道 id。
    pub id: u32,
    /// 父频道；0 表示根。
    pub parent: u32,
    /// 名字。
    pub name: String,
    /// 同级排序。
    pub position: u32,
    /// 人数上限；0 表示服务器默认（32）。
    pub max_users: u32,
    /// 位标志：1=临时 2=需要密码 4=只允许监听 8=分组（不可加入）。
    pub flags: u64,
    /// 本频道人数（不含子树；子树合计由前端算）。
    pub members: u32,
}

/// 成员的一行。
///
/// 注意 `mute` / `solo` / `volume` 是**本机**对别人的设置（混音器上的增益/独听），
/// 不是服务器状态；`server_mute` 才是管理员给的。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemberView {
    /// 账号 id。
    pub user_id: u32,
    /// 显示名。
    pub name: String,
    /// 所在频道。
    pub channel_id: u32,
    /// 本机把这个人静音了。
    pub mute: bool,
    /// 本机独听这个人。
    pub solo: bool,
    /// 被管理员静音。
    pub server_mute: bool,
    /// 显示音量 0–200，100 = 原样。
    pub volume: u16,
    /// 最近 500 ms 内有语音包。
    pub speaking: bool,
}

/// 低频状态快照。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct StateView {
    /// 阶段。
    pub phase: Phase,
    /// 阶段是 `error` 时的原因。
    pub error: Option<String>,
    /// 自己的账号 id。
    pub self_user_id: u32,
    /// 自己在哪个频道。
    pub self_channel: u32,
    /// 自己静音（不发上行）。
    pub muted: bool,
    /// 自己闭听（不听别人）。
    pub deafened: bool,
    /// 频道树（扁平表，前端按 `parent` 拼树）。
    pub channels: Vec<ChannelView>,
    /// 成员表。
    pub members: Vec<MemberView>,
}

/// 20 Hz 的电平/发送状态。
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct MetersView {
    /// 有人在说话。
    pub talking: bool,
    /// 自己上行电平 0–1。
    pub self_level: f32,
    /// 上一段（50 ms）里发过语音包。
    pub tx: bool,
    /// 累计发出的语音包数。
    pub frames_sent: u64,
    /// 上行处理链的算法延迟（ms）。
    pub chain_ms: f32,
    /// 每个 `user_id` 的瞬时电平 0–1。**只含本机真的量到的**：
    /// 自己和已经在混音器里的对方。没有键表示没量到，不是 0。
    pub levels: BTreeMap<u32, f32>,
}

/// 5 s 一次的网络数字。
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct NetView {
    /// 抖动缓冲当前深度（所有对方里最深的那个）。
    pub jitter_ms: u32,
    /// 抖动缓冲的目标延迟。
    pub jitter_target_ms: u32,
    /// 抖动缓冲的允许上限。
    pub jitter_max_ms: u32,
    /// 序列号空洞推断出的丢包数（含打点包占用的序号）。
    pub loss: u64,
    /// 语音面 RTT 的 p50（ms）。
    pub rtt_p50_ms: f32,
    /// 语音面 RTT 的 p95（ms）。
    pub rtt_p95_ms: f32,
    /// 混音欠载帧数。
    pub underruns: u64,
    /// 上行码率（kbps）。
    pub kbps: f32,
    /// 实际 tick 频率（Hz），10 ms 节奏应当接近 100。
    pub tick_hz: f32,
}

/// 外壳 → 会话的指令。
///
/// 会话每个 tick（10 ms）拉一次，所以指令延迟上限就是一个 tick。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// 进某个频道。
    JoinChannel(u32),
    /// 自己静音 / 取消。
    SetMute(bool),
    /// 自己闭听 / 取消。
    SetDeafen(bool),
    /// 调某个人的本机音量 / 静音 / 独听。
    SetUser {
        /// 账号 id。
        user_id: u32,
        /// 显示音量 0–200。
        volume: Option<u16>,
        /// 本机静音。
        mute: Option<bool>,
        /// 本机独听。
        solo: Option<bool>,
    },
    /// 断开并退出会话循环。
    Disconnect,
}

/// 外壳实现这个 trait，接收会话的推送并给出指令。
///
/// # 纪律
///
/// - 三个推送方法都在**会话线程**上被调用，**不得阻塞**（写一个通道就返回）；
/// - [`Host::command`] 每个 tick 调一次，没指令要返回 `None`，不要在里面等。
pub trait Host: Send {
    /// 低频状态（变化时才调）。
    fn state(&self, state: &StateView);
    /// 20 Hz 电平。
    fn meters(&self, meters: &MetersView);
    /// 5 s 网络数字。
    fn net(&self, net: &NetView);
    /// 取一条待执行指令。
    fn command(&self) -> Option<Command>;
}
