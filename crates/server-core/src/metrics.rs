//! 计数器与丢弃原因分布（P5-6 的指标面）。
//!
//! 只记**事件与计数**，绝不记语音内容或明文消息体 —— 这是协议草案 §5 与
//! 技术设计 §7 的红线，也是这套东西能上线的前提。

/// 一个语音包被丢掉的原因。**顺序即指标数组下标，不得重排。**
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DropReason {
    /// 头部不合法（magic/version/flags/长度/超长）。
    Malformed,
    /// 源地址没有绑定到任何会话。
    Unbound,
    /// `channel_epoch` 与会话当前频道的 epoch 不符（跨频道 / 过期）。
    WrongEpoch,
    /// AEAD 认证失败。
    BadTag,
    /// 滑动窗口判定为重放（重复或太老）。
    Replay,
    /// 超速。
    RateLimited,
    /// 发送者不在任何频道。
    NoChannel,
    /// 频道里没有别的可收包的人。
    NoTargets,
    /// 发送者被服务器静音。
    ServerMuted,
    /// 头里的 `sender_id` 与会话的不符（冒充）。
    SenderMismatch,
}

impl DropReason {
    /// 原因种类数。
    pub const COUNT: usize = 10;

    /// 稳定的短名，进指标与日志。**改名字等于改仪表盘**，不要改。
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::Unbound => "unbound",
            Self::WrongEpoch => "wrong_epoch",
            Self::BadTag => "bad_tag",
            Self::Replay => "replay",
            Self::RateLimited => "rate_limited",
            Self::NoChannel => "no_channel",
            Self::NoTargets => "no_targets",
            Self::ServerMuted => "server_muted",
            Self::SenderMismatch => "sender_mismatch",
        }
    }

    /// 数组下标。
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// 全部原因，按指标顺序。
    #[must_use]
    pub const fn all() -> [Self; Self::COUNT] {
        [
            Self::Malformed,
            Self::Unbound,
            Self::WrongEpoch,
            Self::BadTag,
            Self::Replay,
            Self::RateLimited,
            Self::NoChannel,
            Self::NoTargets,
            Self::ServerMuted,
            Self::SenderMismatch,
        ]
    }
}

/// 服务器计数器。全部是单调累加，没有平均值（平均值会掩盖突发）。
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Metrics {
    /// 收到的 UDP 报文数。
    pub datagrams_in: u64,
    /// 收到的字节数。
    pub bytes_in: u64,
    /// 发出的 UDP 报文数。
    pub datagrams_out: u64,
    /// 发出的字节数。
    pub bytes_out: u64,
    /// fan-out 副本数（一个包发给 N 个人就 +N）。
    pub fanout_copies: u64,
    /// 成功绑定次数。
    pub binds: u64,
    /// 重新绑定次数（网络切换）。
    pub rebinds: u64,
    /// 打点包次数。
    pub probes: u64,
    /// 新建会话数。
    pub sessions_opened: u64,
    /// 关闭会话数。
    pub sessions_closed: u64,
    /// 因为连续认证失败被强制断开的会话数。
    pub sessions_killed_bad_tag: u64,
    /// 收到的控制面消息数。
    pub control_msgs: u64,
    /// 被控制面限速拒绝的消息数。
    pub control_rate_limited: u64,
    /// 被忽略的消息数（未知 / 未实现的 oneof 分支、状态不对）。
    pub control_ignored: u64,
    /// 认证失败次数。
    pub auth_failures: u64,
    /// 越权被拒的管理请求数（ADR-0016：这是唯一一处能看出「有人试着做不该做的事」的地方）。
    pub permission_denied: u64,
    /// 被管理员踢出的会话数。
    pub kicks: u64,
    /// 频道结构改动次数（新建 + 改名 + 移动 + 删除，一次请求算一次）。
    ///
    /// 它和 `permission_denied` 一起看：结构改动本该是低频事件，
    /// 突然变高要么是有人在做批量整理，要么是有人在拿脚本刷。
    pub channel_changes: u64,
    /// 各类丢弃原因的计数。
    dropped: [u64; DropReason::COUNT],
}

impl Metrics {
    /// 记一次丢弃。
    pub fn record_drop(&mut self, reason: DropReason) {
        self.dropped[reason.index()] += 1;
    }

    /// 某个原因的次数。
    #[must_use]
    pub fn dropped(&self, reason: DropReason) -> u64 {
        self.dropped[reason.index()]
    }

    /// 丢弃总数。
    #[must_use]
    pub fn dropped_total(&self) -> u64 {
        self.dropped.iter().sum()
    }

    /// 按原因遍历，方便打印/序列化。
    pub fn drop_breakdown(&self) -> impl Iterator<Item = (DropReason, u64)> + '_ {
        DropReason::all()
            .into_iter()
            .map(|r| (r, self.dropped[r.index()]))
    }

    /// 导出成一行 JSON，给运维端点 / 日志采集用。
    ///
    /// 丢弃原因按**名字**输出（而不是数组下标），这样重排枚举不会悄悄改坏仪表盘。
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut dropped = serde_json::Map::new();
        for (reason, count) in self.drop_breakdown() {
            dropped.insert(reason.name().to_owned(), serde_json::Value::from(count));
        }
        let mut value = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        if let serde_json::Value::Object(map) = &mut value {
            // `dropped` 是私有字段（数组），对外只暴露按名字索引的版本。
            map.remove("dropped");
            map.insert("dropped".to_owned(), serde_json::Value::Object(dropped));
            map.insert(
                "dropped_total".to_owned(),
                serde_json::Value::from(self.dropped_total()),
            );
        }
        serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indices_are_stable_and_unique() {
        for (i, reason) in DropReason::all().into_iter().enumerate() {
            assert_eq!(reason.index(), i, "{} 的下标漂了", reason.name());
        }
        assert_eq!(DropReason::all().len(), DropReason::COUNT);
    }

    #[test]
    fn counting_is_per_reason() {
        let mut m = Metrics::default();
        m.record_drop(DropReason::BadTag);
        m.record_drop(DropReason::BadTag);
        m.record_drop(DropReason::Replay);
        assert_eq!(m.dropped(DropReason::BadTag), 2);
        assert_eq!(m.dropped(DropReason::Replay), 1);
        assert_eq!(m.dropped(DropReason::Unbound), 0);
        assert_eq!(m.dropped_total(), 3);
    }
}
