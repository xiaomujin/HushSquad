//! 客户端状态机：控制面握手、频道/成员镜像、语音包的收发。
//!
//! 与 `server-core` 一样是 sans-io：没有 TCP、没有 UDP、没有线程。
//! UI 层负责把字节搬进搬出，这一层负责「现在该发什么」「收到这个之后该变成什么状态」。
//!
//! ```text
//! Idle ──hello()──> HelloSent ──ServerHello──> AwaitAuth ──AuthResult(ok)──> Ready
//!                                                    └──AuthResult(no)──> Closed
//! ```
//!
//! 语音面在 `Ready` 之后才可用：{@Client::bind_packet@} 取绑定凭证，
//! {@Client::encode_voice@} 加密上行，{@Client::receive@} 解密下行并按 sender 去重。

#![deny(unsafe_code)]

pub mod roster;

use std::collections::BTreeMap;

use protocol::control::proto as pb;
use protocol::control::proto::control_message::Kind;
use protocol::{
    ControlError, ControlMessage, Direction, Framer, HEADER_LEN, PROTOCOL_VERSION, PacketKind,
    TAG_LEN, VoiceError, VoiceHeader, VoicePacket, caps, flags, wire_len,
};
use transport::{CryptoError, Key, ReplayWindow, Verdict, VoiceKeys, seal};

pub use roster::{ChannelInfo, Member, Roster};

/// 客户端所处阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientState {
    /// 还没发 ClientHello。
    Idle,
    /// 已发 ClientHello，等 ServerHello。
    HelloSent,
    /// 已拿到 ServerHello，等 AuthResult。
    AwaitAuth,
    /// 认证通过。
    Ready,
    /// 已关闭（服务器要求断开，或本地主动断开）。
    Closed,
}

/// 客户端要执行的动作。
#[derive(Debug)]
pub enum ClientAction {
    /// 通过控制面连接发出。
    Send(ControlMessage),
    /// 往这个地址发一个 Bind 包（语音面绑定）。
    BindVoice {
        /// 主机名或 IP。
        host: String,
        /// 端口。
        port: u16,
        /// 16 字节绑定凭证。
        token: [u8; 16],
    },
    /// 频道 / 成员镜像变了，UI 该重画。
    RosterChanged,
    /// 服务器报错。
    ServerError {
        /// 错误码。
        code: u32,
        /// 人类可读信息。
        message: String,
        /// 服务器随后是否关闭连接。
        fatal: bool,
    },
    /// 连接关闭。
    Closed,
}

/// 收到一个语音面报文之后的解读结果。
#[derive(Debug)]
pub enum Incoming<'a> {
    /// 语音帧（或带冗余帧的语音帧）。
    Voice {
        /// 谁说的。
        sender_id: u32,
        /// 序号。
        seq: u32,
        /// 48 kHz 采样计数。
        timestamp: u32,
        /// Opus 载荷。
        payload: &'a [u8],
    },
    /// 对方停止说话（DTX 边界）。
    Silence {
        /// 谁。
        sender_id: u32,
    },
    /// 绑定成功，顺带给了服务器时间。
    BindAck {
        /// 服务器时间（毫秒）。
        server_time_ms: u64,
        /// 服务器分配的语音标识。
        sender_id: u32,
    },
    /// 打点回送。
    ProbeAck {
        /// 原样回送的客户端时间戳（微秒）。
        ts_us: u64,
    },
}

/// 收包失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    /// AEAD 或格式错误。
    Crypto(CryptoError),
    /// 重放（重复或太老）。
    Replayed,
}

impl core::fmt::Display for RecvError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Crypto(e) => write!(f, "{e}"),
            Self::Replayed => f.write_str("重放包"),
        }
    }
}

impl std::error::Error for RecvError {}

impl From<CryptoError> for RecvError {
    fn from(e: CryptoError) -> Self {
        Self::Crypto(e)
    }
}

/// 客户端。
#[derive(Debug)]
pub struct Client {
    state: ClientState,
    framer: Framer,
    roster: Roster,
    session_id: u64,
    sender_id: u32,
    voice_token: [u8; 16],
    voice_host: String,
    voice_port: u16,
    keys: VoiceKeys,
    next_seq: u32,
    /// 每个 sender 一个防重放窗口（含服务器自己的 `sender_id = 0`）。
    replay: BTreeMap<u32, ReplayWindow>,
    request_id: u64,
    /// 本端时间与服务器时间的差值（毫秒），仅用于显示。
    server_time_offset_ms: Option<i64>,
    caps: u64,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    /// 新建。
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: ClientState::Idle,
            framer: Framer::new(),
            roster: Roster::new(),
            session_id: 0,
            sender_id: 0,
            voice_token: [0u8; 16],
            voice_host: String::new(),
            voice_port: 0,
            keys: VoiceKeys::zeroed(),
            next_seq: 0,
            replay: BTreeMap::new(),
            request_id: 1,
            server_time_offset_ms: None,
            caps: caps::RED | caps::DTX | caps::STATS,
        }
    }

    /// 当前阶段。
    #[must_use]
    pub const fn state(&self) -> ClientState {
        self.state
    }

    /// 频道 / 成员镜像。
    #[must_use]
    pub const fn roster(&self) -> &Roster {
        &self.roster
    }

    /// 服务器分配的会话 id。
    #[must_use]
    pub const fn session_id(&self) -> u64 {
        self.session_id
    }

    /// 语音标识。
    #[must_use]
    pub const fn sender_id(&self) -> u32 {
        self.sender_id
    }

    /// 语音面地址。
    #[must_use]
    pub fn voice_endpoint(&self) -> (&str, u16) {
        (&self.voice_host, self.voice_port)
    }

    /// 语音密钥（测试与联调工具需要；产品代码不该读它）。
    #[must_use]
    pub const fn keys(&self) -> &VoiceKeys {
        &self.keys
    }

    /// 本端时间 − 服务器时间（毫秒）。
    #[must_use]
    pub const fn server_time_offset_ms(&self) -> Option<i64> {
        self.server_time_offset_ms
    }

    fn next_request_id(&mut self) -> u64 {
        let id = self.request_id;
        self.request_id += 1;
        id
    }

    /// 发 ClientHello。返回要发出的消息。
    pub fn hello(&mut self) -> ClientAction {
        self.state = ClientState::HelloSent;
        let request_id = self.next_request_id();
        ClientAction::Send(ControlMessage {
            protocol: PROTOCOL_VERSION,
            request_id,
            kind: Some(Kind::ClientHello(pb::ClientHello {
                protocol_min: PROTOCOL_VERSION,
                protocol_max: PROTOCOL_VERSION,
                client_version: env!("CARGO_PKG_VERSION").to_owned(),
                platform: std::env::consts::OS.to_owned(),
                device_id: String::new(),
                caps: self.caps,
            })),
        })
    }

    /// 发密码认证。
    pub fn auth_password(&mut self, username: &str, password: &str) -> ClientAction {
        let request_id = self.next_request_id();
        ClientAction::Send(ControlMessage {
            protocol: PROTOCOL_VERSION,
            request_id,
            kind: Some(Kind::Auth(pb::Auth {
                method: Some(pb::auth::Method::Password(pb::PasswordAuth {
                    username: username.to_owned(),
                    password: password.to_owned(),
                })),
                client_nonce: String::new(),
            })),
        })
    }

    /// 发设备 token 认证。
    pub fn auth_device_token(&mut self, username: &str, token: Vec<u8>) -> ClientAction {
        let request_id = self.next_request_id();
        ClientAction::Send(ControlMessage {
            protocol: PROTOCOL_VERSION,
            request_id,
            kind: Some(Kind::Auth(pb::Auth {
                method: Some(pb::auth::Method::DeviceToken(pb::DeviceTokenAuth {
                    username: username.to_owned(),
                    device_token: token,
                })),
                client_nonce: String::new(),
            })),
        })
    }

    /// 加入频道。
    pub fn join_channel(&mut self, channel_id: u32) -> ClientAction {
        let request_id = self.next_request_id();
        ClientAction::Send(ControlMessage {
            protocol: PROTOCOL_VERSION,
            request_id,
            kind: Some(Kind::JoinChannel(pb::JoinChannel {
                channel_id,
                password: String::new(),
            })),
        })
    }

    /// 离开当前频道。
    pub fn leave_channel(&mut self) -> ClientAction {
        let request_id = self.next_request_id();
        ClientAction::Send(ControlMessage {
            protocol: PROTOCOL_VERSION,
            request_id,
            kind: Some(Kind::LeaveChannel(pb::LeaveChannel {})),
        })
    }

    /// 改自己的静音 / 闭麦状态。
    pub fn set_self_state(&mut self, self_mute: bool, self_deaf: bool) -> ClientAction {
        let request_id = self.next_request_id();
        ClientAction::Send(ControlMessage {
            protocol: PROTOCOL_VERSION,
            request_id,
            kind: Some(Kind::SetSelfState(pb::SetSelfState {
                self_mute,
                self_deaf,
            })),
        })
    }

    /// 心跳。
    pub fn ping(&mut self, ts_us: u64) -> ClientAction {
        let request_id = self.next_request_id();
        ClientAction::Send(ControlMessage {
            protocol: PROTOCOL_VERSION,
            request_id,
            kind: Some(Kind::Ping(pb::Ping { ts_us })),
        })
    }

    /// 喂入控制面字节流，取出所有已完成的动作。
    pub fn on_bytes(&mut self, bytes: &[u8]) -> Result<Vec<ClientAction>, ControlError> {
        self.framer.push(bytes)?;
        let mut actions = Vec::new();
        while let Some(msg) = self.framer.poll_frame()? {
            actions.extend(self.on_message(msg));
        }
        Ok(actions)
    }

    /// 处理一条控制面消息。
    pub fn on_message(&mut self, msg: ControlMessage) -> Vec<ClientAction> {
        let mut actions = Vec::new();
        if self.roster.apply(&msg) {
            actions.push(ClientAction::RosterChanged);
        }
        match msg.kind {
            Some(Kind::ServerHello(hello)) => {
                self.session_id = hello.session_id;
                self.sender_id = hello.sender_id;
                self.voice_host = hello.voice_host;
                self.voice_port = hello.voice_port as u16;
                if let Ok(token) = <[u8; 16]>::try_from(hello.voice_token.as_slice()) {
                    self.voice_token = token;
                }
                // 密钥由 TLS 导出器派生，客户端在这一步已经知道了 session_id / sender_id，
                // 但**还需要 TLS 层把 exporter 交进来** —— 见 set_exporter。
                self.state = ClientState::AwaitAuth;
            }
            Some(Kind::AuthResult(result)) => {
                if result.ok {
                    self.state = ClientState::Ready;
                    actions.push(ClientAction::BindVoice {
                        host: self.voice_host.clone(),
                        port: self.voice_port,
                        token: self.voice_token,
                    });
                } else {
                    self.state = ClientState::Closed;
                    actions.push(ClientAction::ServerError {
                        code: result.error_code,
                        message: result.message.clone(),
                        fatal: true,
                    });
                    actions.push(ClientAction::Closed);
                }
            }
            Some(Kind::Error(err)) => {
                if err.fatal {
                    self.state = ClientState::Closed;
                }
                actions.push(ClientAction::ServerError {
                    code: err.code,
                    message: err.message,
                    fatal: err.fatal,
                });
                if err.fatal {
                    actions.push(ClientAction::Closed);
                }
            }
            Some(Kind::Pong(pong)) => {
                // 只记 RTT 用的原始时间戳；RTT 由调用方用它自己的时钟算。
                let _ = pong.ts_us;
            }
            _ => {}
        }
        actions
    }

    /// 把 TLS 导出器的 64 字节输出接进来，派生语音密钥。
    ///
    /// 必须在收到 ServerHello **之后**、发 Bind 之前调用 —— 因为派生要用到
    /// `session_id` 与 `voice_token`，而它们来自 ServerHello。
    pub fn set_exporter(&mut self, exporter: &[u8; transport::EXPORTER_LEN], key_epoch: u16) {
        self.keys = transport::derive_voice_keys(exporter, self.session_id, key_epoch);
    }

    /// 记下服务器时间，算本端偏移（只做差值，不做绝对同步）。
    pub fn note_server_time(&mut self, server_time_ms: u64, local_now_ms: u64) {
        self.server_time_offset_ms = Some(local_now_ms as i64 - server_time_ms as i64);
    }

    /// 组装 c2s Bind 包，返回长度。
    pub fn bind_packet(&self, out: &mut [u8]) -> usize {
        protocol::voice::encode_bind(&self.voice_token, out)
    }

    /// 当前频道的 epoch（头部要填的值）。不在频道里时返回 0。
    #[must_use]
    pub fn channel_epoch(&self) -> u16 {
        let id = self.roster.my_channel();
        if id == 0 {
            return 0;
        }
        self.roster.channels().get(&id).map_or(0, |c| c.epoch)
    }

    fn take_seq(&mut self) -> u32 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        seq
    }

    /// 加密一帧语音，返回写入 `out` 的长度。
    pub fn encode_voice(&mut self, opus: &[u8], out: &mut [u8]) -> Result<usize, CryptoError> {
        let seq = self.take_seq();
        let header = VoiceHeader {
            flags: 0,
            red_len: 0,
            channel_epoch: self.channel_epoch(),
            sender_id: self.sender_id,
            seq,
            timestamp: seq.wrapping_mul(protocol::FRAME_SAMPLES),
            opus_len: opus.len() as u16,
        };
        seal::seal_packet(&self.keys.c2s, header, opus, out)
    }

    /// 加密一帧「带冗余」的语音（当前帧 ++ 上一帧副本）。
    pub fn encode_voice_with_red(
        &mut self,
        opus: &[u8],
        red: &[u8],
        out: &mut [u8],
    ) -> Result<usize, CryptoError> {
        let seq = self.take_seq();
        let header = VoiceHeader {
            flags: flags::RED,
            red_len: red.len() as u8,
            channel_epoch: self.channel_epoch(),
            sender_id: self.sender_id,
            seq,
            timestamp: seq.wrapping_mul(protocol::FRAME_SAMPLES),
            opus_len: opus.len() as u16,
        };
        let mut body = Vec::with_capacity(opus.len() + red.len());
        body.extend_from_slice(opus);
        body.extend_from_slice(red);
        seal::seal_packet(&self.keys.c2s, header, &body, out)
    }

    /// 加密一个静音边界包。
    pub fn encode_silence(&mut self, out: &mut [u8]) -> Result<usize, CryptoError> {
        let seq = self.take_seq();
        let header = VoiceHeader {
            flags: flags::SILENCE,
            red_len: 0,
            channel_epoch: self.channel_epoch(),
            sender_id: self.sender_id,
            seq,
            timestamp: seq.wrapping_mul(protocol::FRAME_SAMPLES),
            opus_len: 0,
        };
        seal::seal_packet(&self.keys.c2s, header, &[], out)
    }

    /// 加密一个打点包。
    pub fn encode_probe(&mut self, ts_us: u64, out: &mut [u8]) -> Result<usize, CryptoError> {
        let seq = self.take_seq();
        let header = VoiceHeader {
            flags: flags::PROBE,
            red_len: 0,
            channel_epoch: 0,
            sender_id: self.sender_id,
            seq,
            timestamp: 0,
            opus_len: protocol::PROBE_PAYLOAD_LEN as u16,
        };
        let body = protocol::voice::probe_payload(ts_us);
        seal::seal_packet(&self.keys.c2s, header, &body, out)
    }

    /// 一包语音在网线上的最大长度（供调用方分配缓冲）。
    #[must_use]
    pub fn max_voice_packet(payload: usize) -> usize {
        wire_len(
            &VoiceHeader {
                opus_len: payload as u16,
                ..VoiceHeader::default()
            },
            true,
        )
    }

    /// 解密 + 去重 + 解读一个下行报文。
    pub fn receive<'a>(&mut self, buf: &'a mut [u8]) -> Result<Incoming<'a>, RecvError> {
        let header =
            VoiceHeader::parse(buf).map_err(|e| RecvError::Crypto(CryptoError::Malformed(e)))?;
        // 服务器发的包一律带 tag（Bind 只有 c2s 方向是明文）。
        let expected = HEADER_LEN + header.body_len() + TAG_LEN;
        if buf.len() != expected {
            return Err(RecvError::Crypto(CryptoError::BadLength {
                expected,
                got: buf.len(),
            }));
        }
        seal::open(&self.keys.s2c, buf)?;

        let window = self.replay.entry(header.sender_id).or_default();
        if window.accept(header.seq) != Verdict::Fresh {
            return Err(RecvError::Replayed);
        }

        let packet = VoicePacket::parse(buf, Direction::ServerToClient)
            .map_err(|e| RecvError::Crypto(CryptoError::Malformed(e)))?;
        match packet.kind() {
            PacketKind::Bind => {
                let (server_time_ms, sender_id) =
                    protocol::voice::parse_bind_ack_payload(packet.body())
                        .map_err(|e| RecvError::Crypto(CryptoError::Malformed(e)))?;
                Ok(Incoming::BindAck {
                    server_time_ms,
                    sender_id,
                })
            }
            PacketKind::Probe => {
                let ts_us = protocol::voice::parse_probe_payload(packet.body())
                    .map_err(|e| RecvError::Crypto(CryptoError::Malformed(e)))?;
                Ok(Incoming::ProbeAck { ts_us })
            }
            PacketKind::Silence => Ok(Incoming::Silence {
                sender_id: header.sender_id,
            }),
            PacketKind::Voice => Ok(Incoming::Voice {
                sender_id: header.sender_id,
                seq: header.seq,
                timestamp: header.timestamp,
                payload: packet.payload(),
            }),
        }
    }
}

/// 供 `VoiceError` 的使用方直接引用，省得再 import 一次 `protocol`。
pub type WireError = VoiceError;
/// 供调用方引用 c2s / s2c 密钥类型。
pub type C2sKey = Key;
