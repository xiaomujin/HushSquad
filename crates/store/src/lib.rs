//! SQLite 存储。
//!
//! 两个**互不相干**的库，各自一个模块：
//!
//! | 模块 | 谁用 | 存什么 |
//! |---|---|---|
//! | [`server`] | 服务器 | 账号（用户名 + argon2id PHC 串 + 权限）、设备 token 摘要、频道树 |
//! | [`client`] | 客户端 | 本机设备号、信任过的服务器证书指纹 |
//!
//! 频道树的读写单独在 [`channels`]（它借服务器库的那条连接）。
//!
//! # 这里只做存储，不做密码学
//!
//! 这个 crate **不认识** argon2、也不认识 TLS：哈希与校验留在调用方
//! （`server-core::auth` 已经有现成的实现）。好处是存储层可以被任何认证方案复用，
//! 而且它的测试不需要构造真实的哈希串。
//!
//! # 为什么两个模块共处一个 crate
//!
//! 它们共享同一份「打开 + 建表 + 迁移」的样板，也共享同一个 `StoreError`。分成两个 crate
//! 只会让 `apps/server` 和 `apps/client` 各自多一行依赖，换不来任何隔离 —— 它们本来就不互相引用。

#![deny(unsafe_code)]

pub mod channels;
pub mod client;
pub mod server;

use std::path::Path;

/// 存储层错误。
#[derive(Debug)]
pub enum StoreError {
    /// SQLite 报的错。
    Sqlite(rusqlite::Error),
    /// 打不开库文件（路径不可写、目录不存在）。
    Open {
        /// 路径。
        path: String,
        /// 原因。
        reason: String,
    },
    /// 库里的数据不符合预期（例如 user_version 比程序新）。
    Schema(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(err) => write!(f, "SQLite 错误：{err}"),
            Self::Open { path, reason } => write!(f, "打开 {path} 失败：{reason}"),
            Self::Schema(message) => write!(f, "库结构不对：{message}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sqlite(err)
    }
}

/// 打开一个库文件并确保它的结构是最新的。
///
/// 目录不存在会先建出来：用户第一次跑服务器时不该还要 `mkdir`。
pub(crate) fn open_database(
    path: &Path,
    schema: &str,
    version: i64,
) -> Result<rusqlite::Connection, StoreError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent).map_err(|e| StoreError::Open {
            path: parent.display().to_string(),
            reason: e.to_string(),
        })?;
    }
    let conn = rusqlite::Connection::open(path).map_err(|e| StoreError::Open {
        path: path.display().to_string(),
        reason: e.to_string(),
    })?;
    // WAL：服务器会一边读用户表一边写（匿名登录要建号），WAL 下读写不互相阻塞。
    conn.pragma_update(None, "journal_mode", "WAL").ok();
    conn.pragma_update(None, "foreign_keys", true).ok();
    conn.pragma_update(None, "synchronous", "NORMAL").ok();
    migrate(&conn, schema, version)?;
    Ok(conn)
}

/// 建表 / 迁移。用 `user_version` 记版本，不猜表结构。
pub(crate) fn migrate(
    conn: &rusqlite::Connection,
    schema: &str,
    version: i64,
) -> Result<(), StoreError> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if current > version {
        return Err(StoreError::Schema(format!(
            "库是 v{current}，程序只认到 v{version} —— 别用旧程序开新库"
        )));
    }
    if current == version {
        return Ok(());
    }
    conn.execute_batch(schema)?;
    // PRAGMA 不支持参数绑定，这里只拼一个程序内部的常量。
    conn.execute_batch(&format!("PRAGMA user_version = {version}"))?;
    Ok(())
}
