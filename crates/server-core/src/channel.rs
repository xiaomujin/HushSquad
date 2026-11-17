//! 频道树与成员快照。
//!
//! `channel_epoch` 是**服务器全局单调**的，不是每频道各数各的 —— 见 `Server::bump_epoch`
//! 的注释：如果每个频道各自从 0 数，两个频道很容易撞到同一个 epoch，
//! 那时「跨频道包被拒」这条安全性质就是假的。

use std::collections::BTreeSet;

/// 频道标识。
pub type ChannelId = u32;
/// 用户标识（账号级，跨会话稳定）。
pub type UserId = u32;

/// 一个频道。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    /// 频道 id。
    pub id: ChannelId,
    /// 父频道；0 表示根。
    pub parent: ChannelId,
    /// 名字。
    pub name: String,
    /// 同级排序。
    pub position: u32,
    /// 人数上限；0 表示用服务器默认值。
    pub max_users: u32,
    /// 位：1=临时频道 2=需要密码 4=只允许监听。
    pub flags: u64,
    /// 描述。
    pub description: String,
    /// 成员快照版本；成员变化时由服务器统一发号。
    pub epoch: u16,
    /// 当前成员。
    pub members: BTreeSet<UserId>,
}

impl Channel {
    /// 新建一个空频道。
    #[must_use]
    pub fn new(id: ChannelId, parent: ChannelId, name: impl Into<String>) -> Self {
        Self {
            id,
            parent,
            name: name.into(),
            position: 0,
            max_users: 0,
            flags: 0,
            description: String::new(),
            epoch: 0,
            members: BTreeSet::new(),
        }
    }

    /// 有效人数上限。
    #[must_use]
    pub const fn effective_max_users(&self, server_default: u32) -> u32 {
        if self.max_users == 0 {
            server_default
        } else {
            self.max_users
        }
    }

    /// 还能不能再进人。
    #[must_use]
    pub fn has_room(&self, server_default: u32) -> bool {
        (self.members.len() as u32) < self.effective_max_users(server_default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_uses_the_server_default_when_zero() {
        let mut c = Channel::new(1, 0, "大厅");
        c.max_users = 0;
        assert_eq!(c.effective_max_users(64), 64);
        c.max_users = 2;
        assert_eq!(c.effective_max_users(64), 2);
        c.members.insert(1);
        c.members.insert(2);
        assert!(!c.has_room(64));
    }
}
