//! 频道树（服务器侧）。
//!
//! 这张表存的是**结构**：父子关系、名字、同级顺序、人数上限、标志位。
//! 不存成员、也不存 `channel_epoch` —— 那两样是运行时状态，每次成员变化都要变，
//! 落盘只会让「库」和「内存」多一处可能不一致的地方。
//!
//! # 为什么整棵树一次写
//!
//! 真相只有一份：服务器内存里的那棵树。落盘时把它整个写下去（一个事务里
//! `DELETE` + 逐行 `INSERT`），换来三件事：
//!
//! - **顺序天然一致**：拖拽要重排兄弟的 `position`，一次写就不用担心「改了一半」；
//! - **少一套账**：不用维护「哪个操作对应哪条 `UPDATE`」，将来加一种结构操作也不用改这里；
//! - **id 稳定**：`channel_id` 由服务器发号并显式写回，删掉一个频道之后迟到的客户端
//!   拿着旧 id 只会得到 `CHANNEL_NOT_FOUND`，不会误操作到别的频道上。
//!
//! 代价是每次结构变化都重写全表。100 个频道 = 100 行，在「管理员点了一下」这个频率下
//! 完全不值得优化 —— 一次重写大约 0.1 ms，而一次拖拽要等人抬手。

use rusqlite::{Connection, params};

use crate::StoreError;

/// 频道树的一行（与 `server-core::channel_store::ChannelRow` 字段一致）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelRow {
    /// 频道 id。
    pub channel_id: u32,
    /// 父频道；0 表示根。
    pub parent_id: u32,
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

/// 频道表的读写句柄（借服务器库的那条连接）。
#[derive(Debug)]
pub struct Channels<'a> {
    conn: &'a Connection,
}

impl<'a> Channels<'a> {
    pub(crate) const fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// 读出整棵树。
    ///
    /// 排序按 `parent_id, position, channel_id`：前两个决定界面顺序，最后一个是
    /// 兜底 —— 两个人同时改出相同 `position` 时（手工改库才会发生），顺序仍然是确定的。
    pub fn load(&self) -> Result<Vec<ChannelRow>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT channel_id, parent_id, name, position, max_users, flags, description \
             FROM channels ORDER BY parent_id, position, channel_id",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(ChannelRow {
                    channel_id: row.get::<_, i64>(0)? as u32,
                    parent_id: row.get::<_, i64>(1)? as u32,
                    name: row.get(2)?,
                    position: row.get::<_, i64>(3)? as u32,
                    max_users: row.get::<_, i64>(4)? as u32,
                    flags: row.get::<_, i64>(5)? as u64,
                    description: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 用这一份快照替换整张表（一个事务）。
    pub fn replace_all(&self, rows: &[ChannelRow]) -> Result<(), StoreError> {
        // `unchecked_transaction` 而不是 `transaction`：句柄拿的是 `&Connection`
        // （服务器那边同一时刻只有一把锁，不存在第二条写路径）。
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM channels", [])?;
        {
            let mut statement = tx.prepare(
                "INSERT INTO channels \
                 (channel_id, parent_id, name, position, max_users, flags, description) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for row in rows {
                statement.execute(params![
                    i64::from(row.channel_id),
                    i64::from(row.parent_id),
                    row.name,
                    i64::from(row.position),
                    i64::from(row.max_users),
                    row.flags as i64,
                    row.description,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::ServerStore;

    fn row(id: u32, parent: u32, name: &str, position: u32) -> ChannelRow {
        ChannelRow {
            channel_id: id,
            parent_id: parent,
            name: name.to_owned(),
            position,
            max_users: 0,
            flags: 0,
            description: String::new(),
        }
    }

    #[test]
    fn an_empty_database_has_no_channels() {
        let store = ServerStore::open_memory().expect("内存库");
        assert!(store.channels().load().expect("读").is_empty());
    }

    #[test]
    fn a_snapshot_survives_a_round_trip() {
        let store = ServerStore::open_memory().expect("内存库");
        let mut container = row(1, 0, "常用", 0);
        container.flags = 8;
        container.max_users = 4;
        container.description = "日常".to_owned();
        let rows = vec![container.clone(), row(2, 1, "大厅", 0)];
        store.channels().replace_all(&rows).expect("写");
        assert_eq!(store.channels().load().expect("读"), rows);
    }

    #[test]
    fn replacing_drops_what_is_gone_and_keeps_ids() {
        let store = ServerStore::open_memory().expect("内存库");
        store
            .channels()
            .replace_all(&[row(7, 0, "大厅", 0), row(9, 0, "挂机", 1)])
            .expect("写");
        // 删掉 9、把 7 改个名并挪到第二位。
        let kept = vec![row(7, 0, "大厅（新）", 1)];
        store.channels().replace_all(&kept).expect("再写");
        let loaded = store.channels().load().expect("读");
        assert_eq!(loaded, kept, "id 必须原样保留，而不是重新发号");
    }

    #[test]
    fn load_is_ordered_by_parent_then_position() {
        let store = ServerStore::open_memory().expect("内存库");
        store
            .channels()
            .replace_all(&[
                row(5, 0, "第二", 1),
                row(4, 0, "第一", 0),
                row(6, 4, "子", 0),
            ])
            .expect("写");
        let ids: Vec<u32> = store
            .channels()
            .load()
            .expect("读")
            .iter()
            .map(|r| r.channel_id)
            .collect();
        assert_eq!(ids, vec![4, 5, 6], "根在前、按 position，子节点跟在后面");
    }
}
