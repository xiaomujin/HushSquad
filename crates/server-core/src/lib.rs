//! 服务器核心：控制面频道树/ACL + 语音面 SFU 反射器。
//!
//! v1 的语音面只做四件事：成员校验、`channel_epoch` 校验、fan-out、异常计数。
//! **不做** MCU 混音（与「按人调音量」互斥，见技术设计 §8.1）。
//!
//! ## sans-io
//!
//! 这里没有 socket。`Server::handle_datagram` 收一块字节 + 源地址 + 当前时间，
//! 通过 `DatagramSink` 把要发的包吐出去。于是：
//!
//! - 四条安全性质（跨频道、旧 epoch、错 tag、超速）都能用字节数组直接验证；
//! - 真正的 `recvfrom`/`sendto` 可以换成 `recvmmsg` 或 tokio，不影响任何判定逻辑。
//!
//! ## 两处与协议草案的偏离（都是有意为之，见各函数注释）
//!
//! 1. **AEAD 在校验窗口之前**（§4 写的是反过来）：`seq` 在明文头里，
//!    先更新窗口等于给伪造源地址的攻击者一个「推走窗口」的把手。
//! 2. **`channel_epoch` 是全局单调计数器**（§1 写的是「频道成员快照变化时 +1」）：
//!    每频道各自计数会让两个频道撞到同一个 epoch，跨频道校验就形同虚设。

#![deny(unsafe_code)]

pub mod auth;
pub mod channel;
pub mod control;
pub mod events;
pub mod metrics;
pub mod session;
pub mod voice;

use std::collections::BTreeMap;
use std::net::SocketAddr;

use protocol::VOICE_TOKEN_LEN;
use protocol::control::proto::Limits;
use transport::{EXPORTER_LEN, derive_voice_keys};

pub use auth::{AuthMethod, AuthOutcome, Authenticator, UserDb, UserRecord};
pub use channel::{Channel, ChannelId, UserId};
pub use control::Action;
pub use events::{Event, KillReason, MAX_EVENTS};
pub use metrics::{DropReason, Metrics};
pub use session::{RateMeter, SenderId, Session, SessionId, SessionState};
pub use voice::{DatagramSink, Outcome, VecSink};

/// 服务器配置。全部有默认值，且默认值就是协议草案 §8 写的限额。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// 单会话语音包速率上限（pps）。
    pub max_voice_pps: u32,
    /// 单会话控制消息速率上限（条/秒）。
    pub max_control_msgs_per_sec: u32,
    /// 频道默认人数上限。
    pub max_channel_users: u32,
    /// 同时在线会话上限。
    pub max_sessions: usize,
    /// 连续多少次 AEAD 失败就断开这个会话。
    pub auth_failure_limit: u32,
    /// 当前语音密钥版本。
    pub voice_key_epoch: u16,
    /// 下发给客户端的限额（原样进 ServerHello）。
    pub limits: Limits,
    /// 服务器版本字符串（进 ServerHello，只用于诊断）。
    pub server_version: String,
    /// 语音面地址，写进 ServerHello 让客户端知道往哪发。
    pub voice_host: String,
    /// 语音面端口。
    pub voice_port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_voice_pps: 150,
            max_control_msgs_per_sec: 50,
            max_channel_users: 64,
            max_sessions: 256,
            // 10 个包 = 100 ms 的连续伪造。正常网络抖动不会连错 10 个。
            auth_failure_limit: 10,
            voice_key_epoch: 0,
            limits: Limits {
                max_control_msg_bytes: protocol::MAX_CONTROL_FRAME as u32,
                max_control_msgs_per_sec: 50,
                max_voice_pps: 150,
                max_voice_payload_bytes: 400,
                max_channel_users: 64,
                max_listening_channels: 8,
                ping_interval_ms: 15_000,
                ping_timeout_ms: 45_000,
            },
            server_version: "0.0.0".to_owned(),
            voice_host: "127.0.0.1".to_owned(),
            voice_port: 0,
        }
    }
}

/// 服务器。
pub struct Server {
    config: ServerConfig,
    authenticator: Box<dyn Authenticator>,
    sessions: BTreeMap<SessionId, Session>,
    by_addr: BTreeMap<SocketAddr, SessionId>,
    by_token: BTreeMap<[u8; VOICE_TOKEN_LEN], SessionId>,
    channels: BTreeMap<ChannelId, Channel>,
    metrics: Metrics,
    next_session_id: u64,
    next_sender_id: u32,
    // 注意：**没有** next_user_id。用户 id 来自用户表（认证结果），不是会话序号 ——
    // 用会话序号当 user_id 会让「谁是谁」取决于连接顺序（见 on_auth 的注释）。
    next_channel_id: u32,
    /// 全局单调的成员快照版本，见模块注释第 2 条。
    next_epoch: u16,
    /// 未绑定源地址的限速（防 Bind 洪泛）。
    unbound_rate: RateMeter,
    /// fan-out 复用的临时缓冲：一个包发给 N 个人，只需一份拷贝 + N 次原地加密。
    scratch: Vec<u8>,
    /// 有界事件队列，见 `events` 模块。
    events: std::collections::VecDeque<Event>,
    /// 因为队列满而丢掉的事件数。
    events_dropped: u64,
}

impl core::fmt::Debug for Server {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // 手写而不是 derive：Session 里有机密（语音密钥），
        // 而 Debug 迟早会被打进日志。
        f.debug_struct("Server")
            .field("config", &self.config)
            .field("sessions", &self.sessions.len())
            .field("channels", &self.channels.len())
            .field("metrics", &self.metrics)
            .finish_non_exhaustive()
    }
}

impl Server {
    /// 新建。
    #[must_use]
    pub fn new(config: ServerConfig) -> Self {
        Self {
            unbound_rate: RateMeter::new(config.max_voice_pps),
            authenticator: Box::new(UserDb::new()),
            config,
            sessions: BTreeMap::new(),
            by_addr: BTreeMap::new(),
            by_token: BTreeMap::new(),
            channels: BTreeMap::new(),
            metrics: Metrics::default(),
            // 0 是「未分配」的哨兵值，从 1 开始发号。
            next_session_id: 1,
            next_sender_id: 1,
            next_channel_id: 1,
            next_epoch: 1,
            scratch: Vec::with_capacity(protocol::MAX_ON_WIRE),
            events: std::collections::VecDeque::with_capacity(MAX_EVENTS),
            events_dropped: 0,
        }
    }

    /// 记一条事件。队列满时丢最老的（并计数），绝不无限增长。
    pub fn emit(&mut self, event: Event) {
        if self.events.len() >= MAX_EVENTS {
            self.events.pop_front();
            self.events_dropped += 1;
        }
        self.events.push_back(event);
    }

    /// 取走当前所有事件。
    pub fn drain_events(&mut self) -> Vec<Event> {
        self.events.drain(..).collect()
    }

    /// 因为队列满被丢掉的事件数。
    #[must_use]
    pub const fn events_dropped(&self) -> u64 {
        self.events_dropped
    }

    /// 取下一条「服务器自己发的包」的序号。
    ///
    /// 每条会话独立计数，配合 `sender_id = 0` 保证 (key, nonce) 永不重复。
    pub(crate) fn next_server_seq(&mut self, session_id: SessionId) -> Option<u32> {
        let session = self.sessions.get_mut(&session_id)?;
        let seq = session.server_seq;
        session.server_seq = session.server_seq.wrapping_add(1);
        Some(seq)
    }

    /// 换一个认证器（账号库、LDAP、测试桩都行）。
    pub fn set_authenticator(&mut self, authenticator: Box<dyn Authenticator>) {
        self.authenticator = authenticator;
    }

    /// 当前认证器。
    #[must_use]
    pub fn authenticator(&self) -> &dyn Authenticator {
        &*self.authenticator
    }

    /// 配置。
    #[must_use]
    pub const fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// 计数器。
    #[must_use]
    pub const fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// 可变的计数器（测试与运维端点用）。
    pub const fn metrics_mut(&mut self) -> &mut Metrics {
        &mut self.metrics
    }

    /// 建一个频道，返回 id。
    pub fn add_channel(&mut self, parent: ChannelId, name: impl Into<String>) -> ChannelId {
        let id = self.next_channel_id;
        self.next_channel_id += 1;
        let mut channel = Channel::new(id, parent, name);
        channel.epoch = self.bump_epoch();
        self.channels.insert(id, channel);
        id
    }

    /// 频道。
    #[must_use]
    pub fn channel(&self, id: ChannelId) -> Option<&Channel> {
        self.channels.get(&id)
    }

    /// 频道（可变）。改配置不会动 `epoch` —— 只有成员变化才会。
    pub fn channel_mut(&mut self, id: ChannelId) -> Option<&mut Channel> {
        self.channels.get_mut(&id)
    }

    /// 全部频道。
    #[must_use]
    pub const fn channels(&self) -> &BTreeMap<ChannelId, Channel> {
        &self.channels
    }

    /// 会话。
    #[must_use]
    pub fn session(&self, id: SessionId) -> Option<&Session> {
        self.sessions.get(&id)
    }

    /// 会话（可变）。
    pub fn session_mut(&mut self, id: SessionId) -> Option<&mut Session> {
        self.sessions.get_mut(&id)
    }

    /// 按源地址找会话。
    #[must_use]
    pub fn session_by_addr(&self, addr: &SocketAddr) -> Option<SessionId> {
        self.by_addr.get(addr).copied()
    }

    /// 在线会话数。
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// 已认证、可以参与频道与语音的会话。
    pub fn sessions_ready(&self) -> impl Iterator<Item = &Session> {
        self.sessions.values().filter(|s| s.is_ready())
    }

    /// 发一个新的成员快照版本号。
    ///
    /// **全局单调**，不是每频道各数各的。理由：每频道独立计数时，
    /// 两个频道会各自从 0 数到 1、2、3……，于是「频道 A 的 epoch」和
    /// 「频道 B 的 epoch」大面积重叠，跨频道校验就完全拦不住东西。
    /// u16 会回绕，但一次成员变化才 +1，65536 次变化远长于任何一个在途包的生命。
    pub fn bump_epoch(&mut self) -> u16 {
        let epoch = self.next_epoch;
        self.next_epoch = self.next_epoch.wrapping_add(1);
        epoch
    }

    /// 新建一次控制面会话（TLS 握手完成、还没收到 ClientHello 之前）。
    ///
    /// `voice_token` 是服务器生成的 16 字节 UDP 绑定凭证。密钥**不在这里装**：
    /// TLS 导出器的上下文是 `session_id || voice_token`，而 `session_id` 正是本函数
    /// 分配的 —— 所以调用方要拿返回值去算上下文、调导出器，再 {@install_keys@}。
    pub fn open_session(
        &mut self,
        name: impl Into<String>,
        voice_token: [u8; VOICE_TOKEN_LEN],
    ) -> Option<SessionId> {
        if self.sessions.len() >= self.config.max_sessions {
            return None;
        }
        let id = self.next_session_id;
        self.next_session_id += 1;
        let sender_id = self.next_sender_id;
        self.next_sender_id += 1;

        let keys = transport::VoiceKeys::zeroed();
        let session = Session {
            id,
            // 认证前没有身份；`on_auth` 成功后才写入用户表里的真实 id。
            user_id: 0,
            sender_id,
            name: name.into(),
            voice_token,
            keys,
            replay: transport::ReplayWindow::new(),
            addr: None,
            channel: None,
            state: SessionState::AwaitHello,
            self_mute: false,
            self_deaf: false,
            server_mute: false,
            server_deaf: false,
            perms: 0,
            auth_failures: 0,
            server_seq: 0,
            control_rate: RateMeter::new(self.config.max_control_msgs_per_sec),
            voice_rate: RateMeter::new(self.config.max_voice_pps),
        };
        self.sessions.insert(id, session);
        self.by_token.insert(voice_token, id);
        self.metrics.sessions_opened += 1;
        self.emit(Event::SessionOpened { session: id });
        Some(id)
    }

    /// 装入语音密钥（TLS 握手完成后由传输层调用）。
    ///
    /// 在这之前会话的密钥是全零，{@Server::handle_datagram@} 会把它当成「密钥不对」而丢弃 ——
    /// 这是对的：没走完 TLS 的连接本来就不该发语音。
    pub fn install_keys(&mut self, id: SessionId, exporter: &[u8; EXPORTER_LEN]) -> bool {
        let Some(session) = self.sessions.get_mut(&id) else {
            return false;
        };
        session.keys = derive_voice_keys(exporter, id, self.config.voice_key_epoch);
        true
    }

    /// 关闭并清理一个会话。
    pub fn close_session(&mut self, id: SessionId) {
        if let Some(session) = self.sessions.remove(&id) {
            if let Some(addr) = session.addr {
                self.by_addr.remove(&addr);
            }
            self.by_token.remove(&session.voice_token);
            if let Some(channel_id) = session.channel {
                let epoch = self.bump_epoch();
                if let Some(channel) = self.channels.get_mut(&channel_id) {
                    channel.members.remove(&session.user_id);
                    channel.epoch = epoch;
                }
            }
            self.metrics.sessions_closed += 1;
            self.emit(Event::SessionClosed { session: id });
        }
    }

    /// 把会话放进某个频道（不检查容量，那是 control 层的事）。
    pub(crate) fn place_in_channel(&mut self, session_id: SessionId, channel_id: ChannelId) {
        self.emit(Event::JoinedChannel {
            session: session_id,
            channel: channel_id,
        });
        let Some(user_id) = self.sessions.get(&session_id).map(|s| s.user_id) else {
            return;
        };
        // 先从旧频道出来。
        if let Some(old) = self.sessions.get(&session_id).and_then(|s| s.channel) {
            let epoch = self.bump_epoch();
            if let Some(channel) = self.channels.get_mut(&old) {
                channel.members.remove(&user_id);
                channel.epoch = epoch;
            }
        }
        let epoch = self.bump_epoch();
        if let Some(channel) = self.channels.get_mut(&channel_id) {
            channel.members.insert(user_id);
            channel.epoch = epoch;
        }
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.channel = Some(channel_id);
        }
    }

    /// 把会话从频道里拿出来。
    pub(crate) fn remove_from_channel(&mut self, session_id: SessionId) {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let Some(channel_id) = session.channel.take() else {
            return;
        };
        let user_id = session.user_id;
        self.emit(Event::LeftChannel {
            session: session_id,
            channel: channel_id,
        });
        let epoch = self.bump_epoch();
        if let Some(channel) = self.channels.get_mut(&channel_id) {
            channel.members.remove(&user_id);
            channel.epoch = epoch;
        }
    }

    /// 同频道里的其他会话 id。
    #[must_use]
    pub fn channel_peers(&self, session_id: SessionId) -> Vec<SessionId> {
        let Some(session) = self.sessions.get(&session_id) else {
            return Vec::new();
        };
        let Some(channel_id) = session.channel else {
            return Vec::new();
        };
        let Some(channel) = self.channels.get(&channel_id) else {
            return Vec::new();
        };
        channel
            .members
            .iter()
            .filter(|user| **user != session.user_id)
            .filter_map(|user| {
                self.sessions
                    .values()
                    .find(|s| s.user_id == *user)
                    .map(|s| s.id)
            })
            .collect()
    }
}
