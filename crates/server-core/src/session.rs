//! 会话与限速表。
//!
//! 一个 `Session` 对应**一次控制面连接**；它持有语音密钥、滑动窗口、绑定的 UDP 源地址，
//! 以及「当前在哪个频道」。`sender_id` 由服务器单调分配、会话内永不复用
//! （它参与 AEAD nonce，复用即灾难）。

use std::net::SocketAddr;

use protocol::VOICE_TOKEN_LEN;
use transport::{ReplayWindow, VoiceKeys};

use crate::channel::{ChannelId, UserId};

/// 会话标识。
pub type SessionId = u64;
/// 语音包里的发送者标识。
pub type SenderId = u32;

/// 控制面握手状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// 已建连接，还没收到 ClientHello。
    AwaitHello,
    /// 已回 ServerHello，等 Auth。
    AwaitAuth,
    /// 认证通过，可以收发语音。
    Ready,
    /// 已关闭；后续一切消息都丢弃。
    Closed,
}

/// 1 秒滑动窗口限速器。
///
/// 用「窗口起点 + 计数」而不是令牌桶：超速判定要**可解释**，
/// 「这一秒你已经发了 151 个」比「令牌不够」好排查。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateMeter {
    window_start_ms: u64,
    count: u32,
    limit: u32,
}

impl RateMeter {
    /// 新建，限速 `limit` 次/秒。
    #[must_use]
    pub const fn new(limit: u32) -> Self {
        Self {
            window_start_ms: 0,
            count: 0,
            limit,
        }
    }

    /// 放行一次就返回 `true`。
    pub fn allow(&mut self, now_ms: u64) -> bool {
        if now_ms.saturating_sub(self.window_start_ms) >= 1000 {
            self.window_start_ms = now_ms;
            self.count = 0;
        }
        self.count += 1;
        self.count <= self.limit
    }

    /// 当前窗口内已计的数。
    #[must_use]
    pub const fn count(&self) -> u32 {
        self.count
    }

    /// 限值。
    #[must_use]
    pub const fn limit(&self) -> u32 {
        self.limit
    }
}

/// 一次控制面会话。
#[derive(Debug)]
pub struct Session {
    /// 会话 id。
    pub id: SessionId,
    /// 账号 id。
    pub user_id: UserId,
    /// 语音包标识，会话内永不复用。
    pub sender_id: SenderId,
    /// 显示名。
    pub name: String,
    /// UDP 首包绑定凭证。
    pub voice_token: [u8; VOICE_TOKEN_LEN],
    /// 语音密钥（当前 `key_epoch`）。
    pub keys: VoiceKeys,
    /// 防重放窗口。
    pub replay: ReplayWindow,
    /// 已绑定的 UDP 源地址；`None` 表示还没 Bind。
    pub addr: Option<SocketAddr>,
    /// 当前所在频道。
    pub channel: Option<ChannelId>,
    /// 握手状态。
    pub state: SessionState,
    /// 自己静音。
    pub self_mute: bool,
    /// 自己闭麦（不收任何人的语音）。
    pub self_deaf: bool,
    /// 被管理员静音。
    pub server_mute: bool,
    /// 被管理员闭麦。
    pub server_deaf: bool,
    /// 权限位图。
    pub perms: u64,
    /// 连续 AEAD 失败次数。
    pub auth_failures: u32,
    /// 服务器自己发的包（BindAck / ProbeAck）用的序号。
    ///
    /// 与 `sender_id = 0` 搭配构成 nonce，所以它必须**每条会话独立且单调**：
    /// 重绑定时如果复用序号 0，同一个 (key, nonce) 就会加密两条不同的 BindAck，
    /// ChaCha20-Poly1305 下这是可恢复明文的灾难。
    pub server_seq: u32,
    /// 控制面限速。
    pub control_rate: RateMeter,
    /// 语音面限速。
    pub voice_rate: RateMeter,
    /// 最后一次**收到**这条会话的东西（控制消息或语音包）的时刻，毫秒。
    ///
    /// 用来回收「连接还在、人已经不在了」的会话：半开连接（网线被拔、NAT 表被清、
    /// 进程被强杀而 FIN 没送到）不会触发 TCP 的 EOF，服务器只能靠这个时间戳。
    /// 阈值就是下发给客户端的 `ping_timeout_ms`（45 s）—— 承诺与执行用同一个数。
    pub last_seen_ms: u64,
}

impl Session {
    /// 是否已经可以收发语音。
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self.state, SessionState::Ready)
    }

    /// 是否完全听不见（自己闭麦或被闭麦）。
    #[must_use]
    pub const fn is_deaf(&self) -> bool {
        self.self_deaf || self.server_deaf
    }

    /// 是否被禁止说话。
    #[must_use]
    pub const fn is_muted(&self) -> bool {
        self.server_mute
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_meter_allows_exactly_the_limit_per_second() {
        let mut m = RateMeter::new(3);
        assert!(m.allow(1000));
        assert!(m.allow(1000));
        assert!(m.allow(1000));
        assert!(!m.allow(1000), "第 4 次应当被拒");
        assert!(!m.allow(1999), "同一秒内仍然是第 5 次");
        assert!(m.allow(2000), "下一秒重置");
    }

    #[test]
    fn rate_meter_resets_on_a_long_gap() {
        let mut m = RateMeter::new(1);
        assert!(m.allow(0));
        assert!(!m.allow(10));
        assert!(m.allow(60_000));
        assert_eq!(m.count(), 1);
    }
}
