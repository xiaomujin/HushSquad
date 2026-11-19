//! 客户端的本地状态。
//!
//! 四样东西，都在一个库里：
//!
//! | 表 | 内容 |
//! |---|---|
//! | `identity` | 本机设备号（16 字节随机量，匿名登录的凭证） |
//! | `trust` | 每个服务器主机名对应的证书指纹（首次连接时记住，变了就拒） |
//! | `server_token` | 每个主机下发的免密 token |
//! | `settings` | 本机偏好（键值对：按键绑定、说话方式、处理档、视图……） |
//!
//! # 为什么设置是「键值对」而不是一堆列
//!
//! 偏好会一项一项长出来（今天按键，明天设备名，后天面板开合）。每加一项就迁移一次
//! 表结构，会把「库的版本」变成「偏好的版本」——两者根本没有关系。所以这里只存
//! `k/v` 两个字符串列，版本号只在**结构**变的时候才动。
//!
//! # 设备号是「随机量」而不是「硬件指纹」
//!
//! 用 CPU 序列号、主板 UUID 之类做设备号看起来很自然，但那是**永久指纹**：
//! 换不了、删不掉、跨服务可关联，而且重装系统就变。所以这里是一个 32 位的随机串
//! （128 位），用户可以随时删库重来，也可以用 `--device-id` 手动指定。
//!
//! # 信任是 TOFU（首次使用即信任）
//!
//! 记的是证书 DER 的 SHA-256。第一次连某个主机时接受并记下来，以后指纹不一致就拒绝 ——
//! 和 SSH 的 `known_hosts` 一个道理。这样用户不必把 cert.pem 拷来拷去，
//! 而中间人只有在「第一次」才有机会，且会被后续任何一次连接发现。

use std::path::Path;

use rusqlite::{OptionalExtension, params};

use crate::{StoreError, open_database};

const VERSION: i64 = 2;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS identity (
    k TEXT PRIMARY KEY,
    v BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS trust (
    host        TEXT PRIMARY KEY,
    fingerprint BLOB NOT NULL,
    first_seen  INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE TABLE IF NOT EXISTS server_token (
    host  TEXT PRIMARY KEY,
    token BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS settings (
    k TEXT PRIMARY KEY,
    v TEXT NOT NULL
);
";

const DEVICE_KEY: &str = "device_id";

/// 客户端本地库。
#[derive(Debug)]
pub struct ClientStore {
    conn: rusqlite::Connection,
}

impl ClientStore {
    /// 打开（必要时新建）。
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

    /// 本机设备号；还没生成过就是 `None`。
    pub fn device_id(&self) -> Result<Option<[u8; 16]>, StoreError> {
        let blob: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT v FROM identity WHERE k = ?1",
                params![DEVICE_KEY],
                |row| row.get(0),
            )
            .optional()?;
        Ok(blob.and_then(|bytes| <[u8; 16]>::try_from(bytes.as_slice()).ok()))
    }

    /// 存下设备号。
    pub fn set_device_id(&self, id: &[u8; 16]) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO identity (k, v) VALUES (?1, ?2) \
             ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![DEVICE_KEY, id.as_slice()],
        )?;
        Ok(())
    }

    /// 取一个设备号：没有就生成一个存下来。
    ///
    /// 生成失败（系统随机源不可用）会把错误往上抛，而不是退化成可预测的值 ——
    /// 设备号是身份凭证，可预测等于没有。
    pub fn device_id_or_create(&self) -> Result<[u8; 16], StoreError> {
        if let Some(id) = self.device_id()? {
            return Ok(id);
        }
        let mut id = [0u8; 16];
        getrandom::fill(&mut id).map_err(|e| StoreError::Schema(format!("取随机数失败：{e}")))?;
        self.set_device_id(&id)?;
        Ok(id)
    }

    /// 某个主机记住的证书指纹。
    pub fn fingerprint(&self, host: &str) -> Result<Option<[u8; 32]>, StoreError> {
        let blob: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT fingerprint FROM trust WHERE host = ?1",
                params![host],
                |row| row.get(0),
            )
            .optional()?;
        Ok(blob.and_then(|bytes| <[u8; 32]>::try_from(bytes.as_slice()).ok()))
    }

    /// 记住某个主机的证书指纹（首次连接时调用）。
    pub fn trust(&self, host: &str, fingerprint: &[u8; 32]) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO trust (host, fingerprint) VALUES (?1, ?2) \
             ON CONFLICT(host) DO UPDATE SET fingerprint = excluded.fingerprint",
            params![host, fingerprint.as_slice()],
        )?;
        Ok(())
    }

    /// 某个主机下发给我的设备 token（密码登录成功后服务器给的，下次免密用）。
    pub fn server_token(&self, host: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let blob: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT token FROM server_token WHERE host = ?1",
                params![host],
                |row| row.get(0),
            )
            .optional()?;
        Ok(blob)
    }

    /// 记住某个主机下发的设备 token。
    pub fn set_server_token(&self, host: &str, token: &[u8]) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO server_token (host, token) VALUES (?1, ?2) \
             ON CONFLICT(host) DO UPDATE SET token = excluded.token",
            params![host, token],
        )?;
        Ok(())
    }

    /// 忘掉某个主机下发的设备 token（服务器换了账号/删了设备绑定后用）。
    pub fn forget_server_token(&self, host: &str) -> Result<bool, StoreError> {
        let changed = self
            .conn
            .execute("DELETE FROM server_token WHERE host = ?1", params![host])?;
        Ok(changed > 0)
    }

    /// 忘掉某个主机的信任（换证书后用）。
    pub fn forget(&self, host: &str) -> Result<bool, StoreError> {
        let changed = self
            .conn
            .execute("DELETE FROM trust WHERE host = ?1", params![host])?;
        Ok(changed > 0)
    }

    /// 取一个设置项。没存过就是 `None`（调用方给默认值，别在这里编一个）。
    pub fn setting(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT v FROM settings WHERE k = ?1", params![key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    /// 存一个设置项。
    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO settings (k, v) VALUES (?1, ?2) \
             ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![key, value],
        )?;
        Ok(())
    }

    /// 删一个设置项（回到默认值）。
    pub fn forget_setting(&self, key: &str) -> Result<bool, StoreError> {
        let changed = self
            .conn
            .execute("DELETE FROM settings WHERE k = ?1", params![key])?;
        Ok(changed > 0)
    }
}

/// 把 16 字节设备号写成人能念的样子：`A1B2-C3D4-E5F6-7890-A1B2-C3D4-E5F6-7890`。
#[must_use]
pub fn format_device_id(id: &[u8; 16]) -> String {
    let mut text = String::with_capacity(40);
    for (index, byte) in id.iter().enumerate() {
        if index > 0 && index % 2 == 0 {
            text.push('-');
        }
        text.push_str(&format!("{byte:02X}"));
    }
    text
}

/// 解析 `--device-id` 的输入：容忍分隔符与大小写。
pub fn parse_device_id(text: &str) -> Result<[u8; 16], String> {
    let hex: String = text
        .chars()
        .filter(|c| !matches!(c, '-' | ' ' | ':' | '_'))
        .collect();
    if hex.len() != 32 {
        return Err(format!(
            "设备号要 32 个十六进制字符（当前 {} 个）：{text}",
            hex.len()
        ));
    }
    let mut id = [0u8; 16];
    for (index, chunk) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let pair = std::str::from_utf8(chunk).map_err(|_| "设备号里有非 ASCII 字符".to_owned())?;
        id[index] = u8::from_str_radix(pair, 16)
            .map_err(|_| format!("设备号里有非十六进制字符：{pair}"))?;
    }
    Ok(id)
}

/// 证书 DER 的 SHA-256（信任比对与显示都用它）。
#[must_use]
pub fn fingerprint_of(der: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(der).into()
}

/// 指纹写成人能核对的样子（SHA-256 的十六进制，每字节两位）。
#[must_use]
pub fn format_fingerprint(fingerprint: &[u8; 32]) -> String {
    let mut text = String::with_capacity(95);
    for (index, byte) in fingerprint.iter().enumerate() {
        if index > 0 {
            text.push(':');
        }
        text.push_str(&format!("{byte:02X}"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_device_id_is_created_once_and_then_stable() {
        let store = ClientStore::open_memory().expect("内存库");
        assert!(store.device_id().expect("读").is_none());
        let first = store.device_id_or_create().expect("生成");
        let second = store.device_id_or_create().expect("再取");
        assert_eq!(first, second, "同一个库必须给出同一个设备号");
        assert_ne!(first, [0u8; 16]);
    }

    #[test]
    fn device_ids_round_trip_through_the_human_format() {
        let id = [
            0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x78, 0x90, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
            0x77, 0x88,
        ];
        let text = format_device_id(&id);
        assert_eq!(text, "A1B2-C3D4-E5F6-7890-1122-3344-5566-7788");
        assert_eq!(parse_device_id(&text).unwrap(), id);
        // 容忍小写、冒号、空格
        assert_eq!(
            parse_device_id("a1b2 c3d4:e5f6_7890 1122 3344 5566 7788").unwrap(),
            id
        );
        assert!(parse_device_id("too-short").is_err());
        assert!(parse_device_id(&"Z".repeat(32)).is_err());
    }

    #[test]
    fn trust_is_per_host_and_can_be_forgotten() {
        let store = ClientStore::open_memory().expect("内存库");
        let fingerprint = [5u8; 32];
        assert!(store.fingerprint("a.example:64738").unwrap().is_none());
        store.trust("a.example:64738", &fingerprint).unwrap();
        assert_eq!(
            store.fingerprint("a.example:64738").unwrap(),
            Some(fingerprint)
        );
        assert!(
            store.fingerprint("b.example:64738").unwrap().is_none(),
            "信任是按主机记的，不能串台"
        );
        assert!(store.forget("a.example:64738").unwrap());
        assert!(store.fingerprint("a.example:64738").unwrap().is_none());
    }

    #[test]
    fn server_tokens_are_per_host_and_overwritable() {
        let store = ClientStore::open_memory().expect("内存库");
        assert!(store.server_token("a:1").unwrap().is_none());
        store.set_server_token("a:1", &[1, 2, 3]).unwrap();
        store.set_server_token("a:1", &[4, 5]).unwrap();
        assert_eq!(store.server_token("a:1").unwrap(), Some(vec![4, 5]));
        assert!(store.server_token("b:1").unwrap().is_none(), "不能串台");
        assert!(store.forget_server_token("a:1").unwrap());
        assert!(store.server_token("a:1").unwrap().is_none());
    }

    #[test]
    fn settings_round_trip_and_can_be_forgotten() {
        let store = ClientStore::open_memory().expect("内存库");
        assert!(
            store.setting("ptt_key").unwrap().is_none(),
            "没存过就是 None"
        );
        store.set_setting("ptt_key", "control+space").unwrap();
        store.set_setting("ptt_key", "alt+KeyV").unwrap();
        assert_eq!(
            store.setting("ptt_key").unwrap().as_deref(),
            Some("alt+KeyV"),
            "同一个键要覆盖，不是插两行"
        );
        assert!(store.setting("voice_mode").unwrap().is_none(), "不能串键");
        assert!(store.forget_setting("ptt_key").unwrap());
        assert!(store.setting("ptt_key").unwrap().is_none());
        assert!(
            !store.forget_setting("ptt_key").unwrap(),
            "删两遍第二遍是没动"
        );
    }

    /// v1 的客户端库（没有 `settings` 表）升上来之后：设备号还在、能存设置。
    #[test]
    fn a_v1_client_database_gains_settings_without_losing_the_device_id() {
        const V1: &str = "
        CREATE TABLE identity (
            k TEXT PRIMARY KEY,
            v BLOB NOT NULL
        );
        CREATE TABLE trust (
            host        TEXT PRIMARY KEY,
            fingerprint BLOB NOT NULL,
            first_seen  INTEGER NOT NULL DEFAULT (unixepoch())
        );
        CREATE TABLE server_token (
            host  TEXT PRIMARY KEY,
            token BLOB NOT NULL
        );
        ";
        let path = std::env::temp_dir().join(format!(
            "hushsquad-client-v1-{}-{:?}.db",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(V1).unwrap();
            conn.execute_batch("PRAGMA user_version = 1").unwrap();
            conn.execute(
                "INSERT INTO identity (k, v) VALUES ('device_id', ?1)",
                params![[7u8; 16].as_slice()],
            )
            .unwrap();
        }
        let store = ClientStore::open(&path).unwrap();
        assert_eq!(
            store.device_id().unwrap(),
            Some([7u8; 16]),
            "升级不能把身份弄丢"
        );
        store.set_setting("ptt_key", "alt+KeyV").unwrap();
        assert_eq!(
            store.setting("ptt_key").unwrap().as_deref(),
            Some("alt+KeyV")
        );
        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2, "迁移之后版本号要写回去");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fingerprints_are_sha256_and_print_as_colons() {
        let fingerprint = fingerprint_of(b"hello");
        // sha256("hello") 的前 4 字节
        assert_eq!(&fingerprint[..4], &[0x2c, 0xf2, 0x4d, 0xba]);
        let text = format_fingerprint(&fingerprint);
        assert!(text.starts_with("2C:F2:4D:BA:"));
        assert_eq!(text.split(':').count(), 32);
    }
}
