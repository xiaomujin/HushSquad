//! SQLite 账号库 + 三种登录方式。
//!
//! | 方式 | 谁来 | 服务器看到什么 |
//! |---|---|---|
//! | 用户名 + 密码 | 有账号的人 | `PasswordAuth` |
//! | 用户名 + 设备 token | 上次密码登录过的人 | `DeviceTokenAuth`（用户名非空） |
//! | 只要设备号 | 谁都可以（可关） | `DeviceTokenAuth`（用户名为空） |
//!
//! # 密码永远不落明文
//!
//! 库里存的是 argon2id 的 PHC 串，校验走恒定时间比较；用户不存在时也算一遍哈希，
//! 免得响应时间泄露「这个用户名在不在」。
//!
//! # 设备号为什么是摘要
//!
//! 库文件里存 `SHA-256(设备号)`。拿到库文件的人没法直接冒充任何设备 —— 他只拿到摘要。
//! 设备号本身是客户端生成的 128 位随机量，没有低熵问题，所以不需要加盐。
//!
//! # 匿名账号的边界
//!
//! 匿名账号能说话、能听，但没有密码（`password_hash` 为空），所以**不能**用密码登录；
//! 它只认那一个设备号。上限由 `--max-anonymous` 控制，默认 200。
//! 关掉匿名登录（`--no-anonymous`）后，未登记的设备会收到明确的 `anonymous_disabled`。

use std::path::Path;
use std::sync::Mutex;

use server_core::auth::{AuthError, AuthMethod, AuthOutcome, Authenticator};

use server_core::UserDb;
use store::users::{UserRow, UserStore};

/// 下发/校验设备 token 的长度。128 位随机量，够用且短。
pub const DEVICE_TOKEN_LEN: usize = 16;

/// SQLite 认证器。
pub struct SqliteAuth {
    /// 存储层。`Authenticator::authenticate` 只拿 `&self`，而按设备号登录要写 `last_seen`，
    /// 所以里面套一层锁。临界区只有一条查询，8 人规模下从来不是瓶颈。
    store: Mutex<UserStore>,
    /// 是否允许「只凭设备号」新建账号。
    allow_anonymous: bool,
    /// 匿名账号上限。
    max_anonymous: u32,
}

impl core::fmt::Debug for SqliteAuth {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // **不打印**任何用户名/哈希：日志里不该出现账号内容。
        f.debug_struct("SqliteAuth")
            .field("allow_anonymous", &self.allow_anonymous)
            .field("max_anonymous", &self.max_anonymous)
            .finish_non_exhaustive()
    }
}

impl SqliteAuth {
    /// 打开账号库。
    pub fn open(path: &Path, allow_anonymous: bool, max_anonymous: u32) -> Result<Self, String> {
        let store = UserStore::open(path).map_err(|e| e.to_string())?;
        Ok(Self {
            store: Mutex::new(store),
            allow_anonymous,
            max_anonymous,
        })
    }

    /// 用内存库构造（测试用）。
    pub fn in_memory(allow_anonymous: bool) -> Result<Self, String> {
        Ok(Self {
            store: Mutex::new(UserStore::open_memory().map_err(|e| e.to_string())?),
            allow_anonymous,
            max_anonymous: 200,
        })
    }

    /// 直接拿到存储层（CLI 的 adduser / ls / del 用）。
    pub fn with_store<T>(&self, f: impl FnOnce(&mut UserStore) -> T) -> T {
        let mut store = self.store.lock().expect("锁没中毒");
        f(&mut store)
    }
}

fn outcome_of(row: &UserRow, device_token: Vec<u8>) -> Result<AuthOutcome, AuthError> {
    Ok(AuthOutcome {
        // 存储层用 i64（SQLite 的整数宽度），协议里是 u32。
        user_id: u32::try_from(row.user_id).map_err(|_| AuthError::Internal)?,
        name: row.username.clone(),
        perms: row.perms,
        device_token,
    })
}

/// 设备号的摘要（库里存的就是它）。
#[must_use]
pub fn device_digest(token: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(token).into()
}

/// 新设备号（客户端第一次密码登录时下发的那个）。
fn fresh_device_token() -> Result<Vec<u8>, AuthError> {
    let mut token = vec![0u8; DEVICE_TOKEN_LEN];
    getrandom::fill(&mut token).map_err(|_| AuthError::Internal)?;
    Ok(token)
}

impl Authenticator for SqliteAuth {
    fn authenticate(&self, method: AuthMethod<'_>) -> Result<AuthOutcome, AuthError> {
        match method {
            AuthMethod::Password { username, password } => {
                let record = {
                    let store = self.store.lock().expect("锁没中毒");
                    store.find(username).map_err(|_| AuthError::Internal)?
                };
                let Some(record) = record else {
                    // 用户不存在也付一次哈希开销：否则响应时间就能区分「有没有这个号」。
                    let _ = UserDb::hash_password("unknown-user").ok();
                    return Err(AuthError::UnknownUser);
                };
                if record.password_hash.is_empty() {
                    // 匿名账号没有密码，走密码登录等于「这个号不对」。
                    return Err(AuthError::BadPassword);
                }
                let parsed = argon2::password_hash::phc::PasswordHash::new(&record.password_hash)
                    .map_err(|_| AuthError::Internal)?;
                use argon2::PasswordVerifier;
                argon2::Argon2::default()
                    .verify_password(password.as_bytes(), &parsed)
                    .map_err(|_| AuthError::BadPassword)?;

                // 顺手下发一个设备 token：下次就能免密（也就能离线凭证登录）。
                // 下发失败不该让登录失败 —— 它只是个便利。
                let token = fresh_device_token().unwrap_or_default();
                if !token.is_empty() {
                    let mut store = self.store.lock().expect("锁没中毒");
                    let _ = store.bind_device(record.user_id, &device_digest(&token));
                }
                outcome_of(&record, token)
            }
            AuthMethod::DeviceToken { username, token } => {
                let digest = device_digest(token);
                let store = self.store.lock().expect("锁没中毒");
                let Some(record) = store.find(username).map_err(|_| AuthError::Internal)? else {
                    return Err(AuthError::UnknownUser);
                };
                match store.by_device(&digest).map_err(|_| AuthError::Internal)? {
                    Some(bound) if bound.user_id == record.user_id => {
                        outcome_of(&record, Vec::new())
                    }
                    _ => Err(AuthError::BadToken),
                }
            }
            AuthMethod::Anonymous { token } => {
                let digest = device_digest(token);
                let mut store = self.store.lock().expect("锁没中毒");
                // 见过的设备：直接放行（这就是「免登录再用一次」）。
                if let Some(row) = store.by_device(&digest).map_err(|_| AuthError::Internal)? {
                    return outcome_of(&row, Vec::new());
                }
                if !self.allow_anonymous {
                    return Err(AuthError::AnonymousDisabled);
                }
                let count = store.anonymous_count().map_err(|_| AuthError::Internal)?;
                if count >= self.max_anonymous {
                    return Err(AuthError::TooManyAnonymous);
                }
                // 名字从摘要里取，稳定且不泄露设备号本身。
                let name = format!(
                    "游客-{:02X}{:02X}{:02X}{:02X}",
                    digest[0], digest[1], digest[2], digest[3]
                );
                let row = store
                    .create_anonymous(&name, 1, &digest)
                    .map_err(|_| AuthError::Internal)?;
                outcome_of(&row, Vec::new())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> SqliteAuth {
        let auth = SqliteAuth::in_memory(true).expect("内存库");
        auth.with_store(|store| {
            let hash = UserDb::hash_password("correct horse").expect("哈希");
            store.add_user("alice", &hash, 1).expect("建号");
        });
        auth
    }

    #[test]
    fn password_login_works_and_hands_out_a_device_token() {
        let auth = auth();
        let outcome = auth
            .authenticate(AuthMethod::Password {
                username: "alice",
                password: "correct horse",
            })
            .expect("登录");
        assert_eq!(outcome.name, "alice");
        assert_eq!(outcome.perms, 1);
        assert_eq!(
            outcome.device_token.len(),
            DEVICE_TOKEN_LEN,
            "首次密码登录应当下发设备 token"
        );
    }

    #[test]
    fn the_handed_out_token_logs_in_without_a_password() {
        let auth = auth();
        let first = auth
            .authenticate(AuthMethod::Password {
                username: "alice",
                password: "correct horse",
            })
            .expect("登录");
        let again = auth
            .authenticate(AuthMethod::DeviceToken {
                username: "alice",
                token: &first.device_token,
            })
            .expect("免密登录");
        assert_eq!(again.user_id, first.user_id);
    }

    #[test]
    fn a_wrong_password_and_an_unknown_user_are_both_rejected() {
        let auth = auth();
        assert_eq!(
            auth.authenticate(AuthMethod::Password {
                username: "alice",
                password: "wrong",
            }),
            Err(AuthError::BadPassword)
        );
        assert_eq!(
            auth.authenticate(AuthMethod::Password {
                username: "nobody",
                password: "whatever",
            }),
            Err(AuthError::UnknownUser)
        );
    }

    #[test]
    fn a_token_cannot_be_used_for_somebody_elses_name() {
        let auth = auth();
        auth.with_store(|store| {
            let hash = UserDb::hash_password("other").unwrap();
            store.add_user("bob", &hash, 1).unwrap();
        });
        let alice = auth
            .authenticate(AuthMethod::Password {
                username: "alice",
                password: "correct horse",
            })
            .unwrap();
        assert_eq!(
            auth.authenticate(AuthMethod::DeviceToken {
                username: "bob",
                token: &alice.device_token,
            }),
            Err(AuthError::BadToken),
            "alice 的 token 不能拿来冒充 bob"
        );
    }

    #[test]
    fn the_same_device_gets_the_same_anonymous_identity_every_time() {
        let auth = auth();
        let device = [42u8; DEVICE_TOKEN_LEN];
        let first = auth
            .authenticate(AuthMethod::Anonymous { token: &device })
            .expect("匿名登录");
        let second = auth
            .authenticate(AuthMethod::Anonymous { token: &device })
            .expect("再来一次");
        assert_eq!(first.user_id, second.user_id, "同一台设备必须是同一个身份");
        assert_eq!(first.name, second.name);
        assert!(
            first.name.starts_with("游客-"),
            "匿名账号应当有个能念的名字，实际 {}",
            first.name
        );
    }

    #[test]
    fn different_devices_get_different_identities() {
        let auth = auth();
        let a = auth
            .authenticate(AuthMethod::Anonymous { token: &[1u8; 16] })
            .expect("匿名登录");
        let b = auth
            .authenticate(AuthMethod::Anonymous { token: &[2u8; 16] })
            .expect("匿名登录");
        assert_ne!(a.user_id, b.user_id);
        assert_ne!(a.name, b.name);
    }

    #[test]
    fn anonymous_can_be_turned_off() {
        let auth = SqliteAuth::in_memory(false).expect("内存库");
        assert_eq!(
            auth.authenticate(AuthMethod::Anonymous { token: &[7u8; 16] }),
            Err(AuthError::AnonymousDisabled)
        );
    }

    #[test]
    fn the_anonymous_quota_is_enforced() {
        let mut auth = SqliteAuth::in_memory(true).expect("内存库");
        auth.max_anonymous = 1;
        auth.authenticate(AuthMethod::Anonymous { token: &[1u8; 16] })
            .expect("第一个");
        assert_eq!(
            auth.authenticate(AuthMethod::Anonymous { token: &[2u8; 16] }),
            Err(AuthError::TooManyAnonymous)
        );
        assert!(
            auth.authenticate(AuthMethod::Anonymous { token: &[1u8; 16] })
                .is_ok(),
            "已登记的设备不该被上限挡住（重连必须还能进）"
        );
    }
}
