//! 认证：argon2id 密码校验 + 设备 token 校验。
//!
//! 密码永远不落明文：存的是 PHC 字符串（`$argon2id$v=19$m=19456,t=2,p=1$...`），
//! 校验走 {@Argon2::default@} 的恒定时间比较。协议草案 §5 的
//! `PasswordAuth.password` 只在 TLS 内传输，明文永远不落盘、不落日志。
//!
//! 设备 token 是「下次免密」的凭证：服务器只存 SHA-256 摘要，比对摘要。

use std::collections::BTreeMap;

use argon2::password_hash::phc::PasswordHash;
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use sha2::{Digest, Sha256};

use crate::channel::UserId;

/// 一次认证请求的输入。
#[derive(Debug, Clone, Copy)]
pub enum AuthMethod<'a> {
    /// 用户名 + 密码。
    Password {
        /// 用户名。
        username: &'a str,
        /// 密码（只在 TLS 内出现）。
        password: &'a str,
    },
    /// 用户名 + 设备 token（下次免密）。
    DeviceToken {
        /// 用户名。
        username: &'a str,
        /// 上一次登录时下发的 token。
        token: &'a [u8],
    },
    /// 只凭设备号登录。`token` 是本机生成的设备号，服务器没见过就按匿名账号建一个。
    ///
    /// 与 [`AuthMethod::DeviceToken`] 的区别是**没有用户名**：这条路上服务器不校验「这个
    /// token 属于某个指定账号」，而是「这个 token 是谁」——没见过就是新用户。
    Anonymous {
        /// 本机设备号（客户端自己生成的随机量）。
        token: &'a [u8],
    },
}

/// 认证成功后的身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthOutcome {
    /// 账号 id（跨会话稳定）。
    pub user_id: UserId,
    /// 显示名。
    pub name: String,
    /// 权限位图。
    pub perms: u64,
    /// 要下发给客户端的设备 token（免密登录用）。空表示不下发。
    pub device_token: Vec<u8>,
}

/// 认证失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// 用户名不存在。
    UnknownUser,
    /// 密码不对。
    BadPassword,
    /// 设备 token 不对。
    BadToken,
    /// 服务器不支持这种方式（例如还没配密码库）。
    Unsupported,
    /// 这台服务器不接受设备号匿名登录。
    AnonymousDisabled,
    /// 匿名账号数已达上限。
    TooManyAnonymous,
    /// 服务器内部错误（哈希串坏了）。
    Internal,
}

impl core::fmt::Display for AuthError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            Self::UnknownUser => "unknown user",
            Self::BadPassword => "bad password",
            Self::BadToken => "bad token",
            Self::Unsupported => "unsupported method",
            Self::AnonymousDisabled => "anonymous login is disabled",
            Self::TooManyAnonymous => "too many anonymous accounts",
            Self::Internal => "internal",
        };
        f.write_str(s)
    }
}

impl std::error::Error for AuthError {}

/// 认证器。服务器核心只依赖这个 trait，不关心密码存哪儿。
/// `Send + Sync` 是硬要求：服务器必然多线程（控制面、语音面各一条），
/// 认证器会在这些线程之间共享。
pub trait Authenticator: core::fmt::Debug + Send + Sync {
    /// 认证一次。
    fn authenticate(&self, method: AuthMethod<'_>) -> Result<AuthOutcome, AuthError>;
}

/// 一条用户记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRecord {
    /// 账号 id。
    pub user_id: UserId,
    /// 用户名。
    pub username: String,
    /// argon2id 的 PHC 字符串。
    pub password_hash: String,
    /// 权限位图。
    pub perms: u64,
}

/// 用户库。
#[derive(Debug, Default, Clone)]
pub struct UserDb {
    users: BTreeMap<String, UserRecord>,
    /// 设备 token 摘要 → 账号 id。
    tokens: BTreeMap<[u8; 32], UserId>,
}

impl UserDb {
    /// 空库。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 用户名 → 记录。
    #[must_use]
    pub fn get(&self, username: &str) -> Option<&UserRecord> {
        self.users.get(username)
    }

    /// 用户数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.users.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }

    /// 用明文密码算一个 PHC 字符串（注册/改密时用，**不要在登录路径上调**）。
    ///
    /// 盐由 {@PasswordHasher::hash_password@} 内部用系统 CSPRNG 生成 16 字节，
    /// 因此同一个密码每次得到的串都不同 —— 这是对的，不是 bug。
    pub fn hash_password(password: &str) -> Result<String, AuthError> {
        let hash: PasswordHash = Argon2::default()
            .hash_password(password.as_bytes())
            .map_err(|_| AuthError::Internal)?;
        Ok(hash.to_string())
    }

    /// 加一个用户，密码现算哈希。
    pub fn add_user(
        &mut self,
        user_id: UserId,
        username: &str,
        password: &str,
        perms: u64,
    ) -> Result<(), AuthError> {
        let password_hash = Self::hash_password(password)?;
        self.users.insert(
            username.to_owned(),
            UserRecord {
                user_id,
                username: username.to_owned(),
                password_hash,
                perms,
            },
        );
        Ok(())
    }

    /// 直接插入一条已算好哈希的记录。
    pub fn insert(&mut self, record: UserRecord) {
        self.users.insert(record.username.clone(), record);
    }

    /// 登记一个设备 token（服务器下发时调用）。
    pub fn register_device_token(&mut self, user_id: UserId, token: &[u8]) {
        self.tokens.insert(Sha256::digest(token).into(), user_id);
    }

    /// 解析 `用户名:PHC串:权限位` 每行一条的配置文件。
    ///
    /// PHC 串本身含 `:`（`$argon2id$v=19$m=...` 里没有，但盐和哈希用 base64，
    /// 标准 base64 字母表不含 `:`），所以按**第一个和最后一个**冒号切。
    pub fn parse(text: &str) -> Result<Self, AuthError> {
        let mut db = Self::new();
        for (lineno, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some(first) = line.find(':') else {
                return Err(AuthError::Internal);
            };
            let Some(last) = line.rfind(':') else {
                return Err(AuthError::Internal);
            };
            if first == last {
                return Err(AuthError::Internal);
            }
            let username = &line[..first];
            let hash = &line[first + 1..last];
            let perms = line[last + 1..]
                .parse::<u64>()
                .map_err(|_| AuthError::Internal)?;
            // 校验哈希串能被解析，免得运行到登录时才发现配置写错了。
            PasswordHash::new(hash).map_err(|_| AuthError::Internal)?;
            let _ = lineno;
            db.users.insert(
                username.to_owned(),
                UserRecord {
                    // 配置里不写 user_id：按插入顺序分配，稳定且够用（v1 无账号库）。
                    user_id: (db.users.len() as UserId) + 1,
                    username: username.to_owned(),
                    password_hash: hash.to_owned(),
                    perms,
                },
            );
        }
        Ok(db)
    }
}

impl Authenticator for UserDb {
    fn authenticate(&self, method: AuthMethod<'_>) -> Result<AuthOutcome, AuthError> {
        match method {
            AuthMethod::Password { username, password } => {
                let Some(record) = self.users.get(username) else {
                    // 用户不存在也走一遍同样的哈希开销，避免用响应时间区分
                    // 「用户存在但密码错」与「用户不存在」。
                    let _ = Argon2::default()
                        .hash_password_with_salt(password.as_bytes(), b"unknown-user-salt");
                    return Err(AuthError::UnknownUser);
                };
                let parsed =
                    PasswordHash::new(&record.password_hash).map_err(|_| AuthError::Internal)?;
                Argon2::default()
                    .verify_password(password.as_bytes(), &parsed)
                    .map_err(|_| AuthError::BadPassword)?;
                Ok(AuthOutcome {
                    user_id: record.user_id,
                    name: record.username.clone(),
                    perms: record.perms,
                    device_token: Vec::new(),
                })
            }
            AuthMethod::Anonymous { .. } => Err(AuthError::Unsupported),
            AuthMethod::DeviceToken { username, token } => {
                let Some(record) = self.users.get(username) else {
                    return Err(AuthError::UnknownUser);
                };
                let digest: [u8; 32] = Sha256::digest(token).into();
                match self.tokens.get(&digest) {
                    Some(user_id) if *user_id == record.user_id => Ok(AuthOutcome {
                        user_id: record.user_id,
                        name: record.username.clone(),
                        perms: record.perms,
                        device_token: Vec::new(),
                    }),
                    _ => Err(AuthError::BadToken),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> UserDb {
        let mut db = UserDb::new();
        db.add_user(7, "alice", "correct horse", 1).unwrap();
        db
    }

    #[test]
    fn password_login_works() {
        let out = db()
            .authenticate(AuthMethod::Password {
                username: "alice",
                password: "correct horse",
            })
            .unwrap();
        assert_eq!(out.user_id, 7);
        assert_eq!(out.name, "alice");
        assert_eq!(out.perms, 1);
    }

    #[test]
    fn wrong_password_is_rejected() {
        let err = db()
            .authenticate(AuthMethod::Password {
                username: "alice",
                password: "correct horse battery",
            })
            .unwrap_err();
        assert_eq!(err, AuthError::BadPassword);
    }

    #[test]
    fn unknown_user_is_rejected() {
        let err = db()
            .authenticate(AuthMethod::Password {
                username: "bob",
                password: "whatever",
            })
            .unwrap_err();
        assert_eq!(err, AuthError::UnknownUser);
    }

    #[test]
    fn the_stored_hash_never_contains_the_password() {
        let db = db();
        let record = db.get("alice").unwrap();
        assert!(!record.password_hash.contains("correct horse"));
        assert!(record.password_hash.starts_with("$argon2"));
    }

    #[test]
    fn two_hashes_of_the_same_password_differ() {
        // 盐是随机的：同样的密码不能产生同样的哈希。
        let a = UserDb::hash_password("same").unwrap();
        let b = UserDb::hash_password("same").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn device_tokens_are_matched_by_digest() {
        let mut db = db();
        db.register_device_token(7, b"0123456789abcdef");
        let out = db
            .authenticate(AuthMethod::DeviceToken {
                username: "alice",
                token: b"0123456789abcdef",
            })
            .unwrap();
        assert_eq!(out.user_id, 7);
        assert_eq!(
            db.authenticate(AuthMethod::DeviceToken {
                username: "alice",
                token: b"fedcba9876543210",
            })
            .unwrap_err(),
            AuthError::BadToken
        );
    }

    #[test]
    fn config_file_parsing_round_trips() {
        let hash = UserDb::hash_password("pw").unwrap();
        let text = format!("# 注释行\n\nalice:{hash}:3\n");
        let db = UserDb::parse(&text).unwrap();
        assert_eq!(db.len(), 1);
        let out = db
            .authenticate(AuthMethod::Password {
                username: "alice",
                password: "pw",
            })
            .unwrap();
        assert_eq!(out.perms, 3);
    }

    #[test]
    fn a_broken_config_line_is_rejected() {
        assert!(UserDb::parse("alice:not-a-phc-string:3").is_err());
        assert!(UserDb::parse("alice:nohash").is_err());
        assert!(UserDb::parse("alice:$argon2id$v=19$m=1,t=1,p=1$c2FsdA$aGFzaA:x").is_err());
    }
}
