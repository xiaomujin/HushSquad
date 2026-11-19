//! 联调用的测试夹具：把「建服务器 → 建会话 → 握手 → 绑定 UDP」走一遍真流程。
//!
//! 刻意**不**提供「直接把会话设成 Ready」的后门：测试走的就是生产走的那条路，
//! 一旦握手逻辑坏了，语音面的测试也会一起红 —— 这正是想要的耦合。

#![allow(dead_code)]

use std::net::SocketAddr;

use protocol::control::proto as pb;
use protocol::control::proto::control_message::Kind;
use protocol::{ControlMessage, VoiceHeader, flags};
use server_core::{Action, Authenticator, Server, ServerConfig, SessionId, UserDb, VecSink};
use transport::{Key, seal};

/// 所有测试用的假时钟起点。
pub const NOW: u64 = 1_700_000_000_000;

/// 一个配好账号的服务器。
pub fn server() -> Server {
    let mut db = UserDb::new();
    db.add_user(1, "alice", "alice-pw", 1).unwrap();
    db.add_user(2, "bob", "bob-pw", 1).unwrap();
    db.add_user(3, "carol", "carol-pw", 1).unwrap();
    let mut server = Server::new(ServerConfig::default());
    server.set_authenticator(Box::new(db));
    server
}

/// 认证器换成自定义的（测失败路径用）。
pub fn server_with_auth(auth: Box<dyn Authenticator>) -> Server {
    let mut server = Server::new(ServerConfig::default());
    server.set_authenticator(auth);
    server
}

/// 账号管理的内存实现（测试用）。
///
/// 生产里是 SQLite（`apps/server` 的 `SqliteAuth`），但权限判定的逻辑全在
/// `server-core`，所以这里用一张表就能把「谁能改谁」测干净。
///
/// `Clone` 是共享的（`Arc<Mutex<_>>`）：测试把一份塞进服务器，自己留一份看结果。
#[derive(Debug, Clone, Default)]
pub struct MemAdmin {
    users: std::sync::Arc<std::sync::Mutex<Vec<server_core::UserEntry>>>,
}

impl MemAdmin {
    /// 按 `(user_id, 用户名, 权限位)` 建。
    #[must_use]
    pub fn new(rows: &[(u32, &str, u64)]) -> Self {
        Self {
            users: std::sync::Arc::new(std::sync::Mutex::new(
                rows.iter()
                    .map(|(user_id, username, perms)| server_core::UserEntry {
                        user_id: *user_id,
                        username: (*username).to_owned(),
                        perms: *perms,
                        anonymous: false,
                    })
                    .collect(),
            )),
        }
    }

    /// 现在的权限位（断言用）。
    #[must_use]
    pub fn perms_of(&self, user_id: u32) -> u64 {
        self.users
            .lock()
            .expect("锁没中毒")
            .iter()
            .find(|entry| entry.user_id == user_id)
            .map_or(0, |entry| entry.perms)
    }
}

impl server_core::UserAdmin for MemAdmin {
    fn list_users(&self) -> Result<Vec<server_core::UserEntry>, server_core::auth::AuthError> {
        Ok(self.users.lock().expect("锁没中毒").clone())
    }

    fn set_perms(&self, user_id: u32, perms: u64) -> Result<(), server_core::auth::AuthError> {
        let mut users = self.users.lock().expect("锁没中毒");
        let Some(entry) = users.iter_mut().find(|entry| entry.user_id == user_id) else {
            return Err(server_core::auth::AuthError::UnknownUser);
        };
        entry.perms = perms;
        Ok(())
    }
}

/// alice 是服主、bob 是管理员、carol 是成员。权限相关的测试都用它。
///
/// 返回账号库的一份把手：权限改动落在这里，测试可以直接断言「库里现在是什么」，
/// 而不是只看服务器回了什么消息。
#[must_use]
pub fn roles() -> (Server, MemAdmin) {
    use server_core::perms;
    let mut db = UserDb::new();
    db.add_user(1, "alice", "alice-pw", perms::OWNER_ROLE)
        .unwrap();
    db.add_user(2, "bob", "bob-pw", perms::ADMIN_ROLE).unwrap();
    db.add_user(3, "carol", "carol-pw", perms::MEMBER).unwrap();
    let admin = MemAdmin::new(&[
        (1, "alice", perms::OWNER_ROLE),
        (2, "bob", perms::ADMIN_ROLE),
        (3, "carol", perms::MEMBER),
    ]);
    let mut server = Server::new(ServerConfig::default());
    server.set_authenticator(Box::new(db));
    server.set_user_admin(Box::new(admin.clone()));
    (server, admin)
}

/// 频道树存储的内存实现（测试用）。
///
/// 生产里是 SQLite（`store::channels`），但「什么时候写、写进去的是什么」全由
/// `server-core` 决定，所以一张表就能测干净。
#[derive(Debug, Clone, Default)]
pub struct MemTree {
    rows: std::sync::Arc<std::sync::Mutex<Vec<server_core::ChannelRow>>>,
}

impl MemTree {
    /// 空库。
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// 按 `(id, 父级, 名字, 顺序)` 预置一棵树。
    #[must_use]
    pub fn with(rows: &[(u32, u32, &str, u32)]) -> Self {
        Self {
            rows: std::sync::Arc::new(std::sync::Mutex::new(
                rows.iter()
                    .map(|(id, parent, name, position)| server_core::ChannelRow {
                        channel_id: *id,
                        parent_id: *parent,
                        name: (*name).to_owned(),
                        position: *position,
                        max_users: 0,
                        flags: 0,
                        description: String::new(),
                    })
                    .collect(),
            )),
        }
    }

    /// 现在库里是什么（断言用）。
    #[must_use]
    pub fn rows(&self) -> Vec<server_core::ChannelRow> {
        self.rows.lock().expect("锁没中毒").clone()
    }

    /// 库里的名字，按 `(父级, 顺序)` 排好。
    #[must_use]
    pub fn names(&self) -> Vec<(u32, u32, String)> {
        let mut rows = self.rows();
        rows.sort_by_key(|row| (row.parent_id, row.position, row.channel_id));
        rows.into_iter()
            .map(|row| (row.parent_id, row.position, row.name))
            .collect()
    }
}

impl server_core::ChannelStore for MemTree {
    fn load(&self) -> Result<Vec<server_core::ChannelRow>, String> {
        Ok(self.rows())
    }

    fn save(&self, rows: &[server_core::ChannelRow]) -> Result<(), String> {
        *self.rows.lock().expect("锁没中毒") = rows.to_vec();
        Ok(())
    }
}

/// 一个写不进去的存储（测「落盘失败」的提示）。
#[derive(Debug, Clone, Copy)]
pub struct BrokenTree;

impl server_core::ChannelStore for BrokenTree {
    fn load(&self) -> Result<Vec<server_core::ChannelRow>, String> {
        Ok(Vec::new())
    }

    fn save(&self, _rows: &[server_core::ChannelRow]) -> Result<(), String> {
        Err("磁盘满了".to_owned())
    }
}

fn hello() -> ControlMessage {
    ControlMessage {
        protocol: 1,
        request_id: 1,
        kind: Some(Kind::ClientHello(pb::ClientHello {
            protocol_min: 1,
            protocol_max: 1,
            client_version: "test".to_owned(),
            platform: "windows".to_owned(),
            device_id: String::new(),
            caps: 0,
        })),
    }
}

fn auth(user: &str, password: &str) -> ControlMessage {
    ControlMessage {
        protocol: 1,
        request_id: 2,
        kind: Some(Kind::Auth(pb::Auth {
            method: Some(pb::auth::Method::Password(pb::PasswordAuth {
                username: user.to_owned(),
                password: password.to_owned(),
            })),
            client_nonce: String::new(),
        })),
    }
}

/// 收集动作里的消息，按顺序。
pub fn sent_messages(actions: &[Action]) -> Vec<(SessionId, &ControlMessage)> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Send { to, msg } => Some((*to, msg)),
            Action::Close { .. } => None,
        })
        .collect()
}

/// 建会话 + 握手 + 认证，但**不**绑定 UDP（联调测试要绑到真实 socket 上）。
pub fn connect(server: &mut Server, user: &str, password: &str, token_seed: u8) -> Connected {
    let exporter = [token_seed; 64];
    let voice_token = [token_seed; 16];
    let id = server.open_session(user, voice_token).expect("会话已满");
    assert!(server.install_keys(id, &exporter), "会话刚建好就该在");

    server.handle_control(id, hello(), NOW);
    let actions = server.handle_control(id, auth(user, password), NOW);
    assert!(
        actions.iter().any(|a| matches!(
            a,
            Action::Send { msg, .. } if matches!(&msg.kind, Some(Kind::AuthResult(r)) if r.ok)
        )),
        "{user} 认证没通过"
    );

    let keys = server.session(id).expect("会话存在").keys.clone();
    Connected {
        id,
        c2s: keys.c2s,
        s2c: keys.s2c,
        sender_id: server.session(id).expect("会话存在").sender_id,
        voice_token,
    }
}

/// 已认证、未绑定的会话。
#[derive(Debug, Clone)]
pub struct Connected {
    /// 会话 id。
    pub id: SessionId,
    /// 客户端 → 服务器 密钥。
    pub c2s: Key,
    /// 服务器 → 客户端 密钥。
    pub s2c: Key,
    /// 语音包里的发送者标识。
    pub sender_id: u32,
    /// 绑定凭证。
    pub voice_token: [u8; 16],
}

/// 建会话 → 握手 → 认证 → 绑定一个**假的**源地址，够单元测试用。
pub fn join(server: &mut Server, user: &str, password: &str, token_seed: u8) -> Joined {
    let connected = connect(server, user, password, token_seed);
    let addr: SocketAddr = format!("127.0.0.1:{}", 40_000 + u16::from(token_seed))
        .parse()
        .unwrap();
    let mut sink = VecSink::new();
    let mut buf = [0u8; 64];
    let n = protocol::voice::encode_bind(&connected.voice_token, &mut buf);
    let outcome = server.handle_datagram(addr, &mut buf[..n], NOW, &mut sink);
    assert_eq!(outcome, server_core::Outcome::Replied, "{user} 绑定失败");
    assert_eq!(sink.sent.len(), 1, "{user} 没收到 BindAck");

    Joined {
        id: connected.id,
        addr,
        c2s: connected.c2s,
        s2c: connected.s2c,
        sender_id: connected.sender_id,
        voice_token: connected.voice_token,
    }
}

/// 一个已就绪的客户端。
#[derive(Debug, Clone)]
pub struct Joined {
    /// 会话 id。
    pub id: SessionId,
    /// 绑定的 UDP 源地址。
    pub addr: SocketAddr,
    /// 客户端 → 服务器 密钥。
    pub c2s: Key,
    /// 服务器 → 客户端 密钥。
    pub s2c: Key,
    /// 语音包里的发送者标识。
    pub sender_id: u32,
    /// 绑定凭证。
    pub voice_token: [u8; 16],
}

impl Joined {
    /// 造一包语音（已加密），`epoch` 是包头的 channel_epoch。
    pub fn voice(&self, seq: u32, epoch: u16, payload: &[u8]) -> Vec<u8> {
        let header = VoiceHeader {
            flags: 0,
            red_len: 0,
            channel_epoch: epoch,
            sender_id: self.sender_id,
            seq,
            timestamp: seq.wrapping_mul(480),
            opus_len: payload.len() as u16,
        };
        let mut buf = vec![0u8; protocol::HEADER_LEN + payload.len() + protocol::TAG_LEN];
        let n = seal::seal_packet(&self.c2s, header, payload, &mut buf).unwrap();
        buf.truncate(n);
        buf
    }

    /// 造一个 Probe 包（已加密）。
    pub fn probe(&self, seq: u32, epoch: u16, ts_us: u64) -> Vec<u8> {
        let header = VoiceHeader {
            flags: flags::PROBE,
            red_len: 0,
            channel_epoch: epoch,
            sender_id: self.sender_id,
            seq,
            timestamp: 0,
            opus_len: protocol::PROBE_PAYLOAD_LEN as u16,
        };
        let body = protocol::voice::probe_payload(ts_us);
        let mut buf = vec![0u8; protocol::HEADER_LEN + body.len() + protocol::TAG_LEN];
        let n = seal::seal_packet(&self.c2s, header, &body, &mut buf).unwrap();
        buf.truncate(n);
        buf
    }
}

/// 发一条 JoinChannel。
pub fn join_channel(server: &mut Server, id: SessionId, channel_id: u32) -> Vec<Action> {
    let msg = ControlMessage {
        protocol: 1,
        request_id: 3,
        kind: Some(Kind::JoinChannel(pb::JoinChannel {
            channel_id,
            password: String::new(),
        })),
    };
    server.handle_control(id, msg, NOW)
}

/// 发一条任意控制消息。
pub fn send_control(
    server: &mut Server,
    id: SessionId,
    kind: Kind,
    request_id: u64,
) -> Vec<Action> {
    let msg = ControlMessage {
        protocol: 1,
        request_id,
        kind: Some(kind),
    };
    server.handle_control(id, msg, NOW)
}

/// 频道当前的 epoch。
pub fn epoch_of(server: &Server, channel_id: u32) -> u16 {
    server.channel(channel_id).expect("频道存在").epoch
}

/// 送一包进去，返回 (结果, 发出的字节)。
pub fn deliver(
    server: &mut Server,
    from: SocketAddr,
    bytes: &[u8],
    now_ms: u64,
) -> (server_core::Outcome, VecSink) {
    let mut buf = bytes.to_vec();
    let mut sink = VecSink::new();
    let outcome = server.handle_datagram(from, &mut buf, now_ms, &mut sink);
    (outcome, sink)
}

/// 解一个服务器发出的包，返回明文载荷。
pub fn decrypt(key: &Key, bytes: &[u8]) -> Vec<u8> {
    let mut buf = bytes.to_vec();
    let packet = transport::open_packet(key, &mut buf, protocol::Direction::ServerToClient)
        .expect("服务器发出的包应当能解开");
    packet.payload().to_vec()
}
