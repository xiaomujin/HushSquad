//! 服务器侧的库：账号表 + 频道树。
//!
//! ```sql
//! users(user_id, username UNIQUE, password_hash, perms, anonymous, created_at)
//! devices(token_hash BLOB PRIMARY KEY, user_id REFERENCES users)
//! channels(channel_id, parent_id, name, position, max_users, flags, description, created_at)
//! ```
//!
//! 三张表共处**一个**库文件：它们同属「服务器自己的状态」，备份就是拷一个文件。
//! 频道表的读写细节在 [`crate::channels`]，这里只负责开库、建表与迁移。
//!
//! # 为什么要 `devices` 这张表
//!
//! 设备号匿名登录需要「这个 32 字节 token 以前见过吗」。存**摘要**而不是 token 本身：
//! 拿到库文件的人不能直接冒充任何人的设备。token 是 32 字节随机量，
//! 摘要用 SHA-256 就够（不需要加盐 —— 它不是密码，没有低熵可言）。
//!
//! # 为什么 `anonymous` 是一个字段而不是「权限位的某一位」
//!
//! 它要能被查询（「最多允许多少个匿名号」），而权限位是给业务判断用的。
//! 把两件事塞进一个整数里，迟早会有人用错掩码。
//!
//! # 库结构版本
//!
//! v1 = 账号 + 设备绑定；v2 = 加了 `channels`（P7-2）。迁移是**只加表**：
//! 所有语句都是 `CREATE TABLE IF NOT EXISTS`，所以 v1 的库打开后自动变成 v2，
//! 老账号一条都不动。

use std::path::Path;

use rusqlite::{OptionalExtension, params};

use crate::{StoreError, open_database};

/// 库结构版本。加字段就 +1 并在 [`SCHEMA`] 里写清迁移。
const VERSION: i64 = 2;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS users (
    user_id       INTEGER PRIMARY KEY AUTOINCREMENT,
    username      TEXT    NOT NULL UNIQUE,
    password_hash TEXT    NOT NULL DEFAULT '',
    perms         INTEGER NOT NULL DEFAULT 1,
    anonymous     INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE TABLE IF NOT EXISTS devices (
    token_hash BLOB    PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    last_seen  INTEGER NOT NULL DEFAULT (unixepoch())
);
-- v2（P7-2）：频道树。channel_id 由服务器发号并**显式写回**，所以不用 AUTOINCREMENT：
-- 发号只有一个来源（服务器的计数器），否则「读出来再插回去」会换 id。
CREATE TABLE IF NOT EXISTS channels (
    channel_id  INTEGER PRIMARY KEY,
    parent_id   INTEGER NOT NULL DEFAULT 0,
    name        TEXT    NOT NULL,
    position    INTEGER NOT NULL DEFAULT 0,
    max_users   INTEGER NOT NULL DEFAULT 0,
    flags       INTEGER NOT NULL DEFAULT 0,
    description TEXT    NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL DEFAULT (unixepoch())
);
";

/// 一行账号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRow {
    /// 账号 id（跨会话稳定）。
    pub user_id: i64,
    /// 用户名。
    pub username: String,
    /// argon2id 的 PHC 串；匿名账号为空串（它只能用设备号登录）。
    pub password_hash: String,
    /// 权限位图。
    pub perms: u64,
    /// 是否是设备号匿名建出来的号。
    pub anonymous: bool,
}

/// 服务器侧的库。
#[derive(Debug)]
pub struct ServerStore {
    conn: rusqlite::Connection,
}

impl ServerStore {
    /// 打开（必要时新建）一个库文件。
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Ok(Self {
            conn: open_database(path, SCHEMA, VERSION)?,
        })
    }

    /// 内存库，测试用。
    pub fn open_memory() -> Result<Self, StoreError> {
        let conn = rusqlite::Connection::open_in_memory()?;
        crate::migrate(&conn, SCHEMA, VERSION)?;
        Ok(Self { conn })
    }

    /// 频道树的读写句柄（借同一条连接）。
    #[must_use]
    pub const fn channels(&self) -> crate::channels::Channels<'_> {
        crate::channels::Channels::new(&self.conn)
    }

    /// 按用户名查。
    pub fn find(&self, username: &str) -> Result<Option<UserRow>, StoreError> {
        let row = self
            .conn
            .query_row(
                "SELECT user_id, username, password_hash, perms, anonymous FROM users WHERE username = ?1",
                params![username],
                row_of,
            )
            .optional()?;
        Ok(row)
    }

    /// 按设备 token 摘要查，并顺手更新 `last_seen`。
    pub fn by_device(&self, token_hash: &[u8; 32]) -> Result<Option<UserRow>, StoreError> {
        let row = self
            .conn
            .query_row(
                "SELECT u.user_id, u.username, u.password_hash, u.perms, u.anonymous \
                 FROM devices d JOIN users u ON u.user_id = d.user_id WHERE d.token_hash = ?1",
                params![token_hash.as_slice()],
                row_of,
            )
            .optional()?;
        if row.is_some() {
            self.conn.execute(
                "UPDATE devices SET last_seen = unixepoch() WHERE token_hash = ?1",
                params![token_hash.as_slice()],
            )?;
        }
        Ok(row)
    }

    /// 加一个账号。用户名重复会报错（`UNIQUE` 约束），而不是悄悄覆盖别人的密码。
    pub fn add_user(
        &mut self,
        username: &str,
        password_hash: &str,
        perms: u64,
    ) -> Result<i64, StoreError> {
        self.conn.execute(
            "INSERT INTO users (username, password_hash, perms, anonymous) VALUES (?1, ?2, ?3, 0)",
            params![username, password_hash, perms as i64],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// 改密码。
    pub fn set_password(
        &mut self,
        username: &str,
        password_hash: &str,
    ) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "UPDATE users SET password_hash = ?2 WHERE username = ?1",
            params![username, password_hash],
        )?;
        Ok(changed > 0)
    }

    /// 改一个账号的权限位图。返回是否命中（账号不存在时 `false`）。
    pub fn set_perms(&mut self, user_id: i64, perms: u64) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "UPDATE users SET perms = ?2 WHERE user_id = ?1",
            params![user_id, perms as i64],
        )?;
        Ok(changed > 0)
    }

    /// 有没有哪个账号带 `mask` 里的任意一位。
    ///
    /// 「服务器有没有服主」就靠它判：没有就自动提一个，否则整台机器没人能管。
    pub fn any_with_perms(&mut self, mask: u64) -> Result<bool, StoreError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM users WHERE (perms & ?1) != 0",
            params![mask as i64],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// 第一个非匿名账号（升级老库时把它提成服主）。
    pub fn first_real_user(&mut self) -> Result<Option<UserRow>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT user_id, username, password_hash, perms, anonymous FROM users \
             WHERE anonymous = 0 ORDER BY user_id LIMIT 1",
        )?;
        let mut rows = statement.query([])?;
        match rows.next()? {
            Some(row) => Ok(Some(row_of(row)?)),
            None => Ok(None),
        }
    }

    /// 建一个匿名账号并绑定设备 token（一次事务里做完）。
    pub fn create_anonymous(
        &mut self,
        username: &str,
        perms: u64,
        token_hash: &[u8; 32],
    ) -> Result<UserRow, StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO users (username, password_hash, perms, anonymous) VALUES (?1, '', ?2, 1)",
            params![username, perms as i64],
        )?;
        let user_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO devices (token_hash, user_id) VALUES (?1, ?2)",
            params![token_hash.as_slice(), user_id],
        )?;
        tx.commit()?;
        Ok(UserRow {
            user_id,
            username: username.to_owned(),
            password_hash: String::new(),
            perms,
            anonymous: true,
        })
    }

    /// 把一个设备 token 绑到已有账号（下次免密）。
    pub fn bind_device(&mut self, user_id: i64, token_hash: &[u8; 32]) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO devices (token_hash, user_id) VALUES (?1, ?2) \
             ON CONFLICT(token_hash) DO UPDATE SET user_id = excluded.user_id, last_seen = unixepoch()",
            params![token_hash.as_slice(), user_id],
        )?;
        Ok(())
    }

    /// 全部账号（按 id 排序）。
    pub fn list(&self) -> Result<Vec<UserRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT user_id, username, password_hash, perms, anonymous FROM users ORDER BY user_id",
        )?;
        let rows = stmt.query_map([], row_of)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 删账号（设备绑定跟着级联删）。
    pub fn remove(&mut self, username: &str) -> Result<bool, StoreError> {
        let changed = self
            .conn
            .execute("DELETE FROM users WHERE username = ?1", params![username])?;
        Ok(changed > 0)
    }

    /// 账号总数。
    pub fn count(&self) -> Result<u32, StoreError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;
        Ok(n as u32)
    }

    /// 匿名账号数（用来卡上限）。
    pub fn anonymous_count(&self) -> Result<u32, StoreError> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM users WHERE anonymous = 1",
            [],
            |row| row.get(0),
        )?;
        Ok(n as u32)
    }
}

fn row_of(row: &rusqlite::Row<'_>) -> rusqlite::Result<UserRow> {
    Ok(UserRow {
        user_id: row.get(0)?,
        username: row.get(1)?,
        password_hash: row.get(2)?,
        perms: row.get::<_, i64>(3)? as u64,
        anonymous: row.get::<_, i64>(4)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> ServerStore {
        ServerStore::open_memory().expect("内存库")
    }

    #[test]
    fn adding_and_finding_a_user() {
        let mut s = store();
        let id = s.add_user("alice", "$argon2id$fake", 1).expect("建号");
        let row = s.find("alice").expect("查").expect("应当存在");
        assert_eq!(row.user_id, id);
        assert_eq!(row.perms, 1);
        assert!(!row.anonymous);
        assert!(s.find("bob").expect("查").is_none());
    }

    #[test]
    fn a_duplicate_username_is_refused_instead_of_overwriting() {
        let mut s = store();
        s.add_user("alice", "h1", 1).expect("建号");
        assert!(s.add_user("alice", "h2", 1).is_err(), "重名必须报错");
        assert_eq!(s.find("alice").unwrap().unwrap().password_hash, "h1");
    }

    #[test]
    fn anonymous_users_are_created_with_their_device_binding() {
        let mut s = store();
        let token = [7u8; 32];
        let row = s.create_anonymous("anon-abc", 1, &token).expect("建匿名号");
        assert!(row.anonymous);
        assert!(row.password_hash.is_empty(), "匿名号没有密码");
        let found = s.by_device(&token).expect("按设备查").expect("应当存在");
        assert_eq!(found.user_id, row.user_id);
        assert_eq!(s.anonymous_count().unwrap(), 1);
        assert_eq!(s.count().unwrap(), 1);
    }

    #[test]
    fn rebinding_a_device_moves_it_to_the_new_account() {
        let mut s = store();
        let token = [9u8; 32];
        let alice = s.add_user("alice", "h", 1).unwrap();
        s.bind_device(alice, &token).unwrap();
        assert_eq!(s.by_device(&token).unwrap().unwrap().user_id, alice);
        let bob = s.add_user("bob", "h", 1).unwrap();
        s.bind_device(bob, &token).unwrap();
        assert_eq!(
            s.by_device(&token).unwrap().unwrap().user_id,
            bob,
            "同一个 token 重新绑定应当覆盖，而不是报主键冲突"
        );
    }

    #[test]
    fn removing_a_user_takes_its_device_bindings_with_it() {
        let mut s = store();
        let token = [3u8; 32];
        let id = s.add_user("alice", "h", 1).unwrap();
        s.bind_device(id, &token).unwrap();
        assert!(s.remove("alice").unwrap());
        assert!(s.by_device(&token).unwrap().is_none(), "绑定必须级联删掉");
        assert!(!s.remove("alice").unwrap(), "删两次第二次返回 false");
    }

    #[test]
    fn setting_a_password_reports_whether_anything_changed() {
        let mut s = store();
        s.add_user("alice", "h1", 1).unwrap();
        assert!(s.set_password("alice", "h2").unwrap());
        assert_eq!(s.find("alice").unwrap().unwrap().password_hash, "h2");
        assert!(!s.set_password("nobody", "h2").unwrap());
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA user_version = 99").unwrap();
        let err = crate::migrate(&conn, SCHEMA, VERSION).unwrap_err();
        assert!(err.to_string().contains("v99"), "报错要说清楚版本：{err}");
    }

    /// v1 的库（只有账号表）升上来之后：账号还在、频道表被建出来、版本号是 2。
    ///
    /// 这条测试守的是「老服务器装上新程序还能用」——迁移里任何一个
    /// `DROP` / `ALTER` 都会让它红。
    #[test]
    fn a_v1_database_gains_the_channel_table_without_losing_accounts() {
        const V1: &str = "
        CREATE TABLE users (
            user_id       INTEGER PRIMARY KEY AUTOINCREMENT,
            username      TEXT    NOT NULL UNIQUE,
            password_hash TEXT    NOT NULL DEFAULT '',
            perms         INTEGER NOT NULL DEFAULT 1,
            anonymous     INTEGER NOT NULL DEFAULT 0,
            created_at    INTEGER NOT NULL DEFAULT (unixepoch())
        );
        CREATE TABLE devices (
            token_hash BLOB    PRIMARY KEY,
            user_id    INTEGER NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
            last_seen  INTEGER NOT NULL DEFAULT (unixepoch())
        );
        ";
        let path = std::env::temp_dir().join(format!(
            "hushsquad-store-v1-{}-{:?}.db",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let conn = rusqlite::Connection::open(&path).expect("建 v1 库");
            conn.execute_batch(V1).expect("v1 结构");
            conn.execute_batch("PRAGMA user_version = 1").expect("版本");
            conn.execute(
                "INSERT INTO users (username, password_hash, perms) VALUES ('alice', 'h', 255)",
                [],
            )
            .expect("老账号");
        }

        let store = ServerStore::open(&path).expect("打开老库");
        let alice = store.find("alice").expect("查").expect("老账号必须还在");
        assert_eq!(alice.perms, 255);
        assert!(
            store.channels().load().expect("读频道").is_empty(),
            "新表应当是空的，而不是报错"
        );

        let conn = rusqlite::Connection::open(&path).expect("再开一次读版本");
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("读版本");
        assert_eq!(version, 2);
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }
}
