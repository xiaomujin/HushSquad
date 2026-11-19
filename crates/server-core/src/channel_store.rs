//! 频道树的持久化接口。
//!
//! 与 [`crate::auth::UserAdmin`] 同一个套路：`server-core` **不依赖 SQLite**，
//! 具体怎么存由外壳决定（`apps/server` 接的是 `store::channels`，测试里是一个 `Vec`）。
//!
//! # 为什么只有「读整棵树」和「写整棵树」两个方法
//!
//! 真相只有一份：`Server` 内存里的那棵树。所以持久化不需要「插入一行」「改一行」
//! 「删一行」这种账 —— 每次结构变化之后把整棵树写下去就行。这样做换来的是
//! **原子性天然成立**：拖拽一次要重排一堆兄弟的 `position`，一次写不可能只写一半。
//!
//! 代价是每次改动重写全表。频道数以百计时这仍然是微秒级的事，而改动频率是
//! 「管理员点了一下」。

use crate::channel::ChannelId;

/// 频道树的一行。
///
/// 只有**结构**：父子、名字、顺序、上限、标志。成员和 `epoch` 是运行时状态，
/// 不落盘（见 `store::channels` 的模块注释）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelRow {
    /// 频道 id。
    pub channel_id: ChannelId,
    /// 父频道；0 表示根。
    pub parent_id: ChannelId,
    /// 名字。
    pub name: String,
    /// 同级排序。
    pub position: u32,
    /// 人数上限；0 表示用服务器默认。
    pub max_users: u32,
    /// 位标志（8 = 分组容器）。
    pub flags: u64,
    /// 描述。
    pub description: String,
}

/// 频道树的存取。
pub trait ChannelStore: core::fmt::Debug + Send + Sync {
    /// 读出整棵树（顺序无所谓，`Server` 会按 `parent` + `position` 自己排）。
    fn load(&self) -> Result<Vec<ChannelRow>, String>;

    /// 用这一份快照替换整张表。
    fn save(&self, rows: &[ChannelRow]) -> Result<(), String>;
}
