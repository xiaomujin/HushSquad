//! 事件日志：**只记事件，不记语音内容**。
//!
//! 这条红线写在协议草案 §5 与技术设计 §7 里，也是这套东西能上线的前提：
//! 服务器可以知道「谁在几点几分进了哪个频道」，不可以知道「他说了什么」。
//! 因此这里的事件里没有载荷、没有明文、没有消息体，只有 id、地址与计数。
//!
//! 事件进一个有界队列（{@MAX_EVENTS@} 条），由运维侧定期取走。队列满了就丢最老的，
//! 并累加 {@Server::events_dropped@} —— 宁可丢日志，也不让日志把服务器内存吃穿。

use std::net::SocketAddr;

use serde::Serialize;

use crate::channel::{ChannelId, UserId};
use crate::session::SessionId;

/// 队列上限。一条事件约 60 字节，1024 条约 60 KB。
pub const MAX_EVENTS: usize = 1024;

/// 会话被强制断开的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KillReason {
    /// 连续 AEAD 失败（伪造或密钥不同步）。
    BadTag,
    /// 被管理员踢出（ADR-0016）。踢人时填的原因不进日志，只发给被踢的人。
    Kicked,
}

impl KillReason {
    /// 稳定短名。
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::BadTag => "bad_tag",
            Self::Kicked => "kicked",
        }
    }
}

/// 一条事件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// 新建了一次控制面会话（还没认证）。
    SessionOpened {
        /// 会话 id。
        session: SessionId,
    },
    /// 会话正常关闭。
    SessionClosed {
        /// 会话 id。
        session: SessionId,
    },
    /// 会话被服务器强制断开。
    SessionKilled {
        /// 会话 id。
        session: SessionId,
        /// 原因。
        reason: KillReason,
    },
    /// 认证通过。
    AuthOk {
        /// 会话 id。
        session: SessionId,
        /// 账号 id。
        user_id: UserId,
    },
    /// 认证失败。
    AuthFailed {
        /// 会话 id。
        session: SessionId,
        /// 原因短名（不含用户名/密码）。
        reason: &'static str,
    },
    /// 语音面首次绑定。
    VoiceBound {
        /// 会话 id。
        session: SessionId,
        /// 绑定的源地址。
        addr: SocketAddr,
    },
    /// 语音面重新绑定（换网络）。
    VoiceRebound {
        /// 会话 id。
        session: SessionId,
        /// 旧地址。
        from: SocketAddr,
        /// 新地址。
        to: SocketAddr,
    },
    /// 进入频道。
    JoinedChannel {
        /// 会话 id。
        session: SessionId,
        /// 频道 id。
        channel: ChannelId,
    },
    /// 离开频道。
    LeftChannel {
        /// 会话 id。
        session: SessionId,
        /// 频道 id。
        channel: ChannelId,
    },
    /// 某个账号的权限位变了（ADR-0016）。只记账号 id，不记是谁改的——
    /// 那条信息在「同一个时间窗口里有谁的 SetUserPerms 被接受」里，不必再存一份。
    PermsChanged {
        /// 账号 id。
        user_id: UserId,
    },
    /// 频道结构变了（P7-2）。只记频道 id 与动作名，不记名字 ——
    /// 频道名是用户内容，和「谁说了什么」同一类东西。
    ChannelChanged {
        /// 频道 id。
        channel: ChannelId,
        /// 动作短名：`create` / `update` / `move` / `delete`。
        action: &'static str,
    },
}

impl Event {
    /// 稳定短名，用于日志过滤与计数。
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::SessionOpened { .. } => "session_opened",
            Self::SessionClosed { .. } => "session_closed",
            Self::SessionKilled { .. } => "session_killed",
            Self::AuthOk { .. } => "auth_ok",
            Self::AuthFailed { .. } => "auth_failed",
            Self::VoiceBound { .. } => "voice_bound",
            Self::VoiceRebound { .. } => "voice_rebound",
            Self::JoinedChannel { .. } => "joined_channel",
            Self::LeftChannel { .. } => "left_channel",
            Self::PermsChanged { .. } => "perms_changed",
            Self::ChannelChanged { .. } => "channel_changed",
        }
    }

    /// 一行 JSON。
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_flat_and_tagged() {
        let json = Event::VoiceBound {
            session: 7,
            addr: "127.0.0.1:40000".parse().unwrap(),
        }
        .to_json();
        assert!(json.contains("\"event\":\"voice_bound\""), "{json}");
        assert!(json.contains("\"session\":7"), "{json}");
        assert!(json.contains("127.0.0.1:40000"), "{json}");
    }

    #[test]
    fn no_event_carries_a_payload() {
        // 「不记内容」这条红线的机器可检查版本：事件的字段名只允许白名单里的这些，
        // 所以没人能顺手加一个 payload / message / opus 字段而不被发现。
        const ALLOWED: &[&str] = &[
            "event", "session", "user_id", "addr", "from", "to", "channel", "reason", "action",
        ];
        let samples = [
            Event::SessionOpened { session: 1 },
            Event::SessionClosed { session: 1 },
            Event::SessionKilled {
                session: 1,
                reason: KillReason::BadTag,
            },
            Event::AuthOk {
                session: 1,
                user_id: 2,
            },
            Event::AuthFailed {
                session: 1,
                reason: "bad_password",
            },
            Event::VoiceBound {
                session: 1,
                addr: "127.0.0.1:1".parse().unwrap(),
            },
            Event::VoiceRebound {
                session: 1,
                from: "127.0.0.1:1".parse().unwrap(),
                to: "127.0.0.1:2".parse().unwrap(),
            },
            Event::JoinedChannel {
                session: 1,
                channel: 3,
            },
            Event::LeftChannel {
                session: 1,
                channel: 3,
            },
            Event::PermsChanged { user_id: 4 },
            Event::ChannelChanged {
                channel: 3,
                action: "create",
            },
        ];
        for sample in samples {
            let json = sample.to_json();
            let value: serde_json::Value =
                serde_json::from_str(&json).expect("事件必须是合法 JSON");
            let object = value.as_object().expect("事件必须是对象");
            for key in object.keys() {
                assert!(
                    ALLOWED.contains(&key.as_str()),
                    "事件 {} 里出现了未登记的字段 {key}",
                    sample.kind()
                );
            }
            assert_eq!(object["event"], sample.kind());
        }
    }

    #[test]
    fn kill_reason_names_are_stable() {
        assert_eq!(KillReason::BadTag.name(), "bad_tag");
        assert_eq!(KillReason::Kicked.name(), "kicked");
    }
}
