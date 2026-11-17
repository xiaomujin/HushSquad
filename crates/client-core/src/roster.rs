//! 客户端侧的频道 / 成员镜像。
//!
//! UI 需要三件事：频道树、成员列表、**谁在说话**。前两件靠控制面同步，
//! 第三件靠语音包的 `sender_id` 查表 —— 所以这张表就是「把 sender_id 翻成人名」
//! 的唯一地方。

use std::collections::BTreeMap;

use protocol::ControlMessage;
use protocol::control::proto as pb;
use protocol::control::proto::control_message::Kind;

/// 频道。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelInfo {
    /// 频道 id。
    pub id: u32,
    /// 父频道；0 表示根。
    pub parent: u32,
    /// 名字。
    pub name: String,
    /// 同级排序。
    pub position: u32,
    /// 人数上限；0 表示服务器默认。
    pub max_users: u32,
    /// 位标志。
    pub flags: u64,
    /// 描述。
    pub description: String,
    /// 成员快照版本。
    pub epoch: u16,
}

impl From<&pb::ChannelState> for ChannelInfo {
    fn from(c: &pb::ChannelState) -> Self {
        Self {
            id: c.channel_id,
            parent: c.parent_id,
            name: c.name.clone(),
            position: c.position,
            max_users: c.max_users,
            flags: c.flags,
            description: c.description.clone(),
            epoch: c.channel_epoch as u16,
        }
    }
}

/// 成员。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// 账号 id。
    pub user_id: u32,
    /// 语音包里的标识；0 表示服务器自己（BindAck / ProbeAck）。
    pub sender_id: u32,
    /// 显示名。
    pub name: String,
    /// 所在频道；0 表示不在任何频道。
    pub channel_id: u32,
    /// 自己静音。
    pub self_mute: bool,
    /// 自己闭麦。
    pub self_deaf: bool,
    /// 被管理员静音。
    pub server_mute: bool,
    /// 被管理员闭麦。
    pub server_deaf: bool,
    /// 最近一次收到他语音包的时刻（毫秒），用于「说话中」指示。
    pub last_voice_ms: u64,
}

impl From<&pb::UserState> for Member {
    fn from(u: &pb::UserState) -> Self {
        Self {
            user_id: u.user_id,
            sender_id: u.sender_id,
            name: u.name.clone(),
            channel_id: u.channel_id,
            self_mute: u.self_mute,
            self_deaf: u.self_deaf,
            server_mute: u.server_mute,
            server_deaf: u.server_deaf,
            last_voice_ms: 0,
        }
    }
}

/// 频道 + 成员镜像。
#[derive(Debug, Default, Clone)]
pub struct Roster {
    channels: BTreeMap<u32, ChannelInfo>,
    members: BTreeMap<u32, Member>,
    self_user_id: u32,
}

impl Roster {
    /// 空表。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 自己的账号 id（认证通过后才有）。
    #[must_use]
    pub const fn self_user_id(&self) -> u32 {
        self.self_user_id
    }

    /// 自己的成员记录。
    #[must_use]
    pub fn myself(&self) -> Option<&Member> {
        self.members.get(&self.self_user_id)
    }

    /// 自己在哪个频道。
    #[must_use]
    pub fn my_channel(&self) -> u32 {
        self.myself().map_or(0, |m| m.channel_id)
    }

    /// 频道表。
    #[must_use]
    pub const fn channels(&self) -> &BTreeMap<u32, ChannelInfo> {
        &self.channels
    }

    /// 成员表。
    #[must_use]
    pub const fn members(&self) -> &BTreeMap<u32, Member> {
        &self.members
    }

    /// 某个频道里的成员。
    pub fn members_in(&self, channel_id: u32) -> impl Iterator<Item = &Member> {
        self.members
            .values()
            .filter(move |m| m.channel_id == channel_id)
    }

    /// `sender_id` → 显示名。收到语音包时用这个把 id 翻成人名。
    #[must_use]
    pub fn name_of_sender(&self, sender_id: u32) -> Option<&str> {
        if sender_id == 0 {
            return Some("服务器");
        }
        self.members
            .values()
            .find(|m| m.sender_id == sender_id)
            .map(|m| m.name.as_str())
    }

    /// 记一次「这个人刚说过话」。
    pub fn note_voice(&mut self, sender_id: u32, now_ms: u64) {
        if let Some(m) = self.members.values_mut().find(|m| m.sender_id == sender_id) {
            m.last_voice_ms = now_ms;
        }
    }

    /// 应用一条控制面消息，返回是否改动了镜像。
    pub fn apply(&mut self, msg: &ControlMessage) -> bool {
        match &msg.kind {
            Some(Kind::ChannelSync(sync)) => {
                self.channels.clear();
                self.members.clear();
                for c in &sync.channels {
                    self.channels.insert(c.channel_id, ChannelInfo::from(c));
                }
                for u in &sync.users {
                    self.members.insert(u.user_id, Member::from(u));
                }
                true
            }
            Some(Kind::AuthResult(result)) if result.ok => {
                self.self_user_id = result.user_id;
                true
            }
            Some(Kind::ChannelAdded(added)) => {
                let Some(c) = &added.channel else {
                    return false;
                };
                self.channels.insert(c.channel_id, ChannelInfo::from(c));
                true
            }
            Some(Kind::ChannelRemoved(removed)) => {
                self.channels.remove(&removed.channel_id).is_some()
            }
            Some(Kind::ChannelUpdated(updated)) => {
                let Some(c) = &updated.channel else {
                    return false;
                };
                self.channels.insert(c.channel_id, ChannelInfo::from(c));
                true
            }
            Some(Kind::UserJoined(joined)) => {
                let Some(u) = &joined.user else {
                    return false;
                };
                self.members.insert(u.user_id, Member::from(u));
                true
            }
            Some(Kind::UserLeft(left)) => self.members.remove(&left.user_id).is_some(),
            Some(Kind::UserUpdated(updated)) => {
                let Some(u) = &updated.user else {
                    return false;
                };
                let previous = self.members.get(&u.user_id);
                let mut member = Member::from(u);
                // 保留本地的「刚说过话」时间戳，服务器不关心它。
                if let Some(p) = previous {
                    member.last_voice_ms = p.last_voice_ms;
                }
                self.members.insert(u.user_id, member);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::control::proto::control_message::Kind;
    use protocol::{ControlMessage, PROTOCOL_VERSION};

    fn msg(kind: Kind) -> ControlMessage {
        ControlMessage {
            protocol: PROTOCOL_VERSION,
            request_id: 0,
            kind: Some(kind),
        }
    }

    fn user(id: u32, sender: u32, name: &str, channel: u32) -> pb::UserState {
        pb::UserState {
            user_id: id,
            sender_id: sender,
            name: name.to_owned(),
            channel_id: channel,
            ..Default::default()
        }
    }

    #[test]
    fn sync_replaces_everything() {
        let mut r = Roster::new();
        r.apply(&msg(Kind::ChannelSync(pb::ChannelSync {
            channels: vec![pb::ChannelState {
                channel_id: 1,
                name: "大厅".to_owned(),
                channel_epoch: 7,
                ..Default::default()
            }],
            users: vec![user(1, 10, "alice", 1)],
            self_channel_id: 1,
        })));
        assert_eq!(r.channels().len(), 1);
        assert_eq!(r.channels()[&1].epoch, 7);
        assert_eq!(r.members_in(1).count(), 1);
    }

    #[test]
    fn sender_ids_translate_to_names() {
        let mut r = Roster::new();
        r.apply(&msg(Kind::UserJoined(pb::UserJoined {
            user: Some(user(1, 10, "alice", 1)),
        })));
        assert_eq!(r.name_of_sender(10), Some("alice"));
        assert_eq!(r.name_of_sender(0), Some("服务器"));
        assert_eq!(r.name_of_sender(99), None);
    }

    #[test]
    fn updates_keep_the_local_speaking_timestamp() {
        let mut r = Roster::new();
        r.apply(&msg(Kind::UserJoined(pb::UserJoined {
            user: Some(user(1, 10, "alice", 1)),
        })));
        r.note_voice(10, 1234);
        let mut updated = user(1, 10, "alice", 1);
        updated.self_mute = true;
        r.apply(&msg(Kind::UserUpdated(pb::UserUpdated {
            user: Some(updated),
        })));
        assert!(r.members()[&1].self_mute);
        assert_eq!(r.members()[&1].last_voice_ms, 1234);
    }

    #[test]
    fn leaving_removes_the_member() {
        let mut r = Roster::new();
        r.apply(&msg(Kind::UserJoined(pb::UserJoined {
            user: Some(user(1, 10, "alice", 1)),
        })));
        assert!(r.apply(&msg(Kind::UserLeft(pb::UserLeft { user_id: 1 }))));
        assert!(r.members().is_empty());
    }
}
