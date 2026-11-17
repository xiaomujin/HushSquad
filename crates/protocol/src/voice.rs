//! 语音包线格式：**20 字节固定头 + 载荷 + 16 字节 AEAD tag**，全部大端。
//!
//! 依据 `docs/协议草案-v0.1.md` §4。字段偏移是**破坏性契约**：任何偏移或长度变化
//! 都必须提升主版本（`version` 字节），不做「同版本号下行为漂移」。
//!
//! 本模块只做**格式**校验（magic / version / flags / 长度自洽 / 载荷语义）。
//! 认证（AEAD）与去重（滑动窗口）分别在 `transport` 里，因为它们需要密钥与状态，
//! 而格式是纯函数 —— 这一层没有密钥、没有时钟、没有分配。
//!
//! ## 方向是格式的一部分
//!
//! 协议草案给 Bind 和 BindAck **同一个 flags 位（bit3）**，区别只在载荷长度
//! （16 B token vs 12 B 时间戳+id）与「有没有 tag」。单看字节流无法区分：
//! 20+16 与 20+12+16 都可能出现在网线上。所以解析必须带方向：
//! **只有「客户端 → 服务器」的 Bind 包是明文无 tag**，其余一切（含 BindAck、Probe、
//! ProbeAck、Silence）都走 AEAD。方向由谁在收包决定，不需要额外的标志位。

use core::fmt;

/// 头第一个字节，固定 `0x48`（ASCII 'H'）。用于极早期丢弃垃圾包与端口扫描流量。
pub const MAGIC: u8 = 0x48;
/// 当前线格式版本。
pub const VERSION: u8 = 0x01;
/// 固定头长度。
pub const HEADER_LEN: usize = 20;
/// AEAD 认证标签长度（ChaCha20-Poly1305）。
pub const TAG_LEN: usize = 16;
/// `voice_token` 长度。
pub const VOICE_TOKEN_LEN: usize = 16;
/// Probe / ProbeAck 载荷长度（`client_ts_us`，u64 大端）。
pub const PROBE_PAYLOAD_LEN: usize = 8;
/// BindAck 载荷长度（`server_time_ms` u64 + `sender_id` u32）。
pub const BIND_ACK_PAYLOAD_LEN: usize = 12;
/// 单包线上长度上限。超过即丢弃，避免 IP 分片（协议草案 §8）。
pub const MAX_ON_WIRE: usize = 1200;
/// `timestamp` 字段的单位：48 kHz 采样计数（协议草案 §4）。
pub const SAMPLE_RATE: u32 = 48_000;
/// 一个语音帧的采样数（10 ms @ 48 kHz）。
///
/// 这个常数必须与 `audio-core` 的 `FRAME_SAMPLES` 一致 —— 线格式说「timestamp 是
/// 采样计数」，而发帧的是音频侧。两边由 `client-core` 的一致性测试钉住
/// （那里同时依赖两边；`audio-core` 不允许依赖 `protocol`，所以测试放在更上层）。
pub const FRAME_SAMPLES: u32 = 480;

/// flags 位定义。
pub mod flags {
    /// bit0：载荷后附带上一帧副本（RED）。
    pub const RED: u8 = 1 << 0;
    /// bit1：冗余帧是 PLC 占位（解码端知道「这是造出来的，不是真收到的」）。
    pub const RED_PLC: u8 = 1 << 1;
    /// bit2：打点包（Probe / ProbeAck）。
    pub const PROBE: u8 = 1 << 2;
    /// bit3：绑定包（Bind / BindAck）。
    pub const BIND: u8 = 1 << 3;
    /// bit4：静音帧 / DTX 边界。
    pub const SILENCE: u8 = 1 << 4;
    /// bit5-7：保留，**必须为 0**，否则丢弃（未知行为不得静默接受）。
    pub const RESERVED: u8 = 0b1110_0000;

    /// 三种互斥的「包型」位。RED 与它们正交，不在此掩码里。
    pub const KIND_MASK: u8 = PROBE | BIND | SILENCE;
}

/// 包型。网线上只有这四种；Bind/Probe 的「应答」形态由方向 + 载荷语义决定，
/// 不是独立的线格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketKind {
    /// 普通语音帧。
    Voice,
    /// 绑定（c2s 明文 / s2c 加密）。
    Bind,
    /// 打点（双向加密）。
    Probe,
    /// 静音 / DTX 边界。
    Silence,
}

/// 收包方向。决定 Bind 包是否带 tag。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// 服务器收到的方向。
    ClientToServer,
    /// 客户端收到的方向。
    ServerToClient,
}

/// 语音包格式错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceError {
    /// 短于 20 字节固定头。
    TooShort {
        /// 实际长度。
        len: usize,
    },
    /// 首字节不是 `MAGIC`。
    BadMagic {
        /// 实际值。
        got: u8,
    },
    /// 版本字节不支持。
    BadVersion {
        /// 实际值。
        got: u8,
    },
    /// 保留了必须为 0 的 flags 位。
    ReservedFlags {
        /// 实际 flags。
        flags: u8,
    },
    /// 同时置位了多个包型位。
    ConflictingKind {
        /// 实际 flags。
        flags: u8,
    },
    /// `flags.bit0` 与 `red_len` 不自洽（置位则长度必须 > 0，反之亦然）。
    RedFlagMismatch {
        /// 实际 flags。
        flags: u8,
        /// 实际 red_len。
        red_len: u8,
    },
    /// `flags.bit1`（PLC 占位）在没有冗余帧时被置位。
    PlcWithoutRed {
        /// 实际 flags。
        flags: u8,
    },
    /// 总长度与头部自述的长度不符。
    BadLength {
        /// 头部推出的应有长度。
        expected: usize,
        /// 实际长度。
        got: usize,
    },
    /// Bind 载荷长度不对（c2s 应为 16，s2c 应为 12）。
    BadBindPayload {
        /// 实际长度。
        got: usize,
        /// 该方向应有的长度。
        expected: usize,
    },
    /// Probe 载荷长度不对（应为 8）。
    BadProbePayload {
        /// 实际长度。
        got: usize,
    },
    /// Silence 包带了载荷。
    SilenceCarriesPayload {
        /// 实际长度。
        got: usize,
    },
    /// 普通语音包的载荷为空。
    EmptyVoice,
    /// 包型与载荷语义不匹配（例如 Bind 包带了 RED）。
    KindCarriesRed {
        /// 实际 flags。
        flags: u8,
    },
    /// 单包超过 `MAX_ON_WIRE`。
    TooLarge {
        /// 实际长度。
        len: usize,
    },
}

impl fmt::Display for VoiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { len } => write!(f, "包长 {len} < 头部 {HEADER_LEN}"),
            Self::BadMagic { got } => write!(f, "magic 应为 0x{MAGIC:02x}，实际 0x{got:02x}"),
            Self::BadVersion { got } => write!(f, "版本应为 {VERSION}，实际 {got}"),
            Self::ReservedFlags { flags } => write!(f, "flags 0x{flags:02x} 置位了保留位"),
            Self::ConflictingKind { flags } => write!(f, "flags 0x{flags:02x} 同时置位多个包型位"),
            Self::RedFlagMismatch { flags, red_len } => {
                write!(f, "flags 0x{flags:02x} 与 red_len {red_len} 不自洽")
            }
            Self::PlcWithoutRed { flags } => {
                write!(f, "flags 0x{flags:02x} 置了 PLC 占位位却没有冗余帧")
            }
            Self::BadLength { expected, got } => write!(f, "包长应为 {expected}，实际 {got}"),
            Self::BadBindPayload { got, expected } => {
                write!(f, "Bind 载荷应为 {expected} 字节，实际 {got}")
            }
            Self::BadProbePayload { got } => {
                write!(f, "Probe 载荷应为 {PROBE_PAYLOAD_LEN} 字节，实际 {got}")
            }
            Self::SilenceCarriesPayload { got } => write!(f, "Silence 包不该带载荷，实际 {got}"),
            Self::EmptyVoice => f.write_str("语音包载荷为空"),
            Self::KindCarriesRed { flags } => {
                write!(f, "flags 0x{flags:02x}：特殊包不得携带冗余帧")
            }
            Self::TooLarge { len } => write!(f, "包长 {len} 超过上限 {MAX_ON_WIRE}"),
        }
    }
}

impl std::error::Error for VoiceError {}

/// 固定 20 字节头。
///
/// `magic` 与 `version` 不存字段：它们在校验时被吃掉，因此这个类型
/// **不可能**表示一个 magic 或版本错误的头。少一个能构造出非法值的入口。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VoiceHeader {
    /// 见 `flags` 模块。
    pub flags: u8,
    /// 冗余帧字节数；无冗余为 0。
    pub red_len: u8,
    /// 频道成员快照版本；服务器 fan-out 前校验。
    pub channel_epoch: u16,
    /// 会话内永不复用的发送者标识。
    pub sender_id: u32,
    /// 每个 sender 每个 key_epoch 从 0 单调递增。
    pub seq: u32,
    /// 48 kHz 采样计数（回绕按 2^32 处理）。
    pub timestamp: u32,
    /// 主载荷字节数。**特殊包里它就是载荷长度**（Bind 16 / BindAck 12 / Probe 8），
    /// 不是 Opus 帧长 —— 线格式只有一个长度字段，多一个只会多一处不一致。
    pub opus_len: u16,
}

impl VoiceHeader {
    /// 编码为 20 字节，大端。
    #[must_use]
    pub const fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0] = MAGIC;
        out[1] = VERSION;
        out[2] = self.flags;
        out[3] = self.red_len;
        out[4] = (self.channel_epoch >> 8) as u8;
        out[5] = self.channel_epoch as u8;
        out[6] = (self.sender_id >> 24) as u8;
        out[7] = (self.sender_id >> 16) as u8;
        out[8] = (self.sender_id >> 8) as u8;
        out[9] = self.sender_id as u8;
        out[10] = (self.seq >> 24) as u8;
        out[11] = (self.seq >> 16) as u8;
        out[12] = (self.seq >> 8) as u8;
        out[13] = self.seq as u8;
        out[14] = (self.timestamp >> 24) as u8;
        out[15] = (self.timestamp >> 16) as u8;
        out[16] = (self.timestamp >> 8) as u8;
        out[17] = self.timestamp as u8;
        out[18] = (self.opus_len >> 8) as u8;
        out[19] = self.opus_len as u8;
        out
    }

    /// 从字节流前 20 字节解析，并校验 magic / version / flags。
    pub fn parse(bytes: &[u8]) -> Result<Self, VoiceError> {
        if bytes.len() < HEADER_LEN {
            return Err(VoiceError::TooShort { len: bytes.len() });
        }
        if bytes[0] != MAGIC {
            return Err(VoiceError::BadMagic { got: bytes[0] });
        }
        if bytes[1] != VERSION {
            return Err(VoiceError::BadVersion { got: bytes[1] });
        }
        let flags = bytes[2];
        if flags & flags::RESERVED != 0 {
            return Err(VoiceError::ReservedFlags { flags });
        }
        if (flags & flags::KIND_MASK).count_ones() > 1 {
            return Err(VoiceError::ConflictingKind { flags });
        }
        let red_len = bytes[3];
        let red_flag = flags & flags::RED != 0;
        if red_flag != (red_len > 0) {
            return Err(VoiceError::RedFlagMismatch { flags, red_len });
        }
        if flags & flags::RED_PLC != 0 && !red_flag {
            return Err(VoiceError::PlcWithoutRed { flags });
        }
        if flags & flags::KIND_MASK != 0 && red_flag {
            return Err(VoiceError::KindCarriesRed { flags });
        }
        Ok(Self {
            flags,
            red_len,
            channel_epoch: u16::from_be_bytes([bytes[4], bytes[5]]),
            sender_id: u32::from_be_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]),
            seq: u32::from_be_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]),
            timestamp: u32::from_be_bytes([bytes[14], bytes[15], bytes[16], bytes[17]]),
            opus_len: u16::from_be_bytes([bytes[18], bytes[19]]),
        })
    }

    /// 包型。
    #[must_use]
    pub const fn kind(&self) -> PacketKind {
        if self.flags & flags::BIND != 0 {
            PacketKind::Bind
        } else if self.flags & flags::PROBE != 0 {
            PacketKind::Probe
        } else if self.flags & flags::SILENCE != 0 {
            PacketKind::Silence
        } else {
            PacketKind::Voice
        }
    }

    /// 是否携带冗余帧。
    #[must_use]
    pub const fn has_red(&self) -> bool {
        self.red_len > 0
    }

    /// 主载荷 + 冗余载荷的总字节数（也就是 AEAD 明文长度）。
    #[must_use]
    pub const fn body_len(&self) -> usize {
        self.opus_len as usize + self.red_len as usize
    }
}

/// 解析后的语音包视图（零拷贝，借用原始缓冲区）。
#[derive(Debug, Clone, Copy)]
pub struct VoicePacket<'a> {
    header: VoiceHeader,
    raw: &'a [u8],
    body: &'a [u8],
    tag: Option<&'a [u8; TAG_LEN]>,
    dir: Direction,
}

impl<'a> VoicePacket<'a> {
    /// 解析一整包。
    ///
    /// `dir` 只影响一件事：c2s 的 Bind 包没有 tag（服务器在它之前不认密钥）。
    pub fn parse(bytes: &'a [u8], dir: Direction) -> Result<Self, VoiceError> {
        if bytes.len() > MAX_ON_WIRE {
            return Err(VoiceError::TooLarge { len: bytes.len() });
        }
        let header = VoiceHeader::parse(bytes)?;
        let kind = header.kind();
        let encrypted = !(dir == Direction::ClientToServer && kind == PacketKind::Bind);
        let body_len = header.body_len();
        let expected = HEADER_LEN + body_len + if encrypted { TAG_LEN } else { 0 };
        if bytes.len() != expected {
            return Err(VoiceError::BadLength {
                expected,
                got: bytes.len(),
            });
        }

        let body = &bytes[HEADER_LEN..HEADER_LEN + body_len];
        let tag = if encrypted {
            let start = HEADER_LEN + body_len;
            Some(
                <&[u8; TAG_LEN]>::try_from(&bytes[start..start + TAG_LEN])
                    .expect("上面已经校验过总长度"),
            )
        } else {
            None
        };

        match kind {
            PacketKind::Bind => {
                let want = if dir == Direction::ClientToServer {
                    VOICE_TOKEN_LEN
                } else {
                    BIND_ACK_PAYLOAD_LEN
                };
                if body_len != want {
                    return Err(VoiceError::BadBindPayload {
                        got: body_len,
                        expected: want,
                    });
                }
            }
            PacketKind::Probe => {
                if body_len != PROBE_PAYLOAD_LEN {
                    return Err(VoiceError::BadProbePayload { got: body_len });
                }
            }
            PacketKind::Silence => {
                if body_len != 0 {
                    return Err(VoiceError::SilenceCarriesPayload { got: body_len });
                }
            }
            PacketKind::Voice => {
                if header.opus_len == 0 {
                    return Err(VoiceError::EmptyVoice);
                }
            }
        }

        Ok(Self {
            header,
            raw: bytes,
            body,
            tag,
            dir,
        })
    }

    /// 头部。
    #[must_use]
    pub const fn header(&self) -> &VoiceHeader {
        &self.header
    }

    /// 收包方向。
    #[must_use]
    pub const fn direction(&self) -> Direction {
        self.dir
    }

    /// 包型。
    #[must_use]
    pub const fn kind(&self) -> PacketKind {
        self.header.kind()
    }

    /// AEAD 的 AAD —— **恰好是 20 字节头**，一个字节不多不少。
    ///
    /// 头部被任何改动（哪怕是 `seq`）都会让 tag 校验失败，
    /// 所以服务器不能在不被察觉的情况下改 `sender_id` 或 `seq`。
    #[must_use]
    pub fn aad(&self) -> &'a [u8] {
        &self.raw[..HEADER_LEN]
    }

    /// 原始整包字节（密文形态）。
    #[must_use]
    pub const fn raw(&self) -> &'a [u8] {
        self.raw
    }

    /// AEAD 明文（网线上是密文）：主载荷 ++ 冗余载荷。
    #[must_use]
    pub const fn body(&self) -> &'a [u8] {
        self.body
    }

    /// 认证标签；只有 c2s Bind 包没有。
    #[must_use]
    pub const fn tag(&self) -> Option<&'a [u8; TAG_LEN]> {
        self.tag
    }

    /// 主载荷（Opus 帧，或特殊包载荷）。
    #[must_use]
    pub fn payload(&self) -> &'a [u8] {
        &self.body[..self.header.opus_len as usize]
    }

    /// 冗余帧副本。
    #[must_use]
    pub fn red(&self) -> &'a [u8] {
        &self.body[self.header.opus_len as usize..]
    }
}

/// 一包语音在网线上的总长度。`encrypted` 为 `false` 只用于 c2s Bind。
#[must_use]
pub const fn wire_len(header: &VoiceHeader, encrypted: bool) -> usize {
    HEADER_LEN + header.body_len() + if encrypted { TAG_LEN } else { 0 }
}

/// 组装 c2s Bind 包（明文，无 tag）。返回实际写入的 36 字节长度。
///
/// 单独一个函数是因为这是**唯一**不带 AEAD 的包：把它和通用路径分开，
/// 让「忘了加密」这件事不可能发生在别的包型上。
pub fn encode_bind(voice_token: &[u8; VOICE_TOKEN_LEN], out: &mut [u8]) -> usize {
    let header = VoiceHeader {
        flags: flags::BIND,
        red_len: 0,
        channel_epoch: 0,
        sender_id: 0,
        seq: 0,
        timestamp: 0,
        opus_len: VOICE_TOKEN_LEN as u16,
    };
    let len = HEADER_LEN + VOICE_TOKEN_LEN;
    assert!(out.len() >= len, "Bind 包缓冲区不足");
    out[..HEADER_LEN].copy_from_slice(&header.encode());
    out[HEADER_LEN..len].copy_from_slice(voice_token);
    len
}

/// 取出 c2s Bind 包里的 `voice_token`。
pub fn parse_bind(packet: &VoicePacket<'_>) -> Result<[u8; VOICE_TOKEN_LEN], VoiceError> {
    if packet.kind() != PacketKind::Bind || packet.direction() != Direction::ClientToServer {
        return Err(VoiceError::BadBindPayload {
            got: packet.body().len(),
            expected: VOICE_TOKEN_LEN,
        });
    }
    Ok(<[u8; VOICE_TOKEN_LEN]>::try_from(packet.body()).expect("parse 已校验长度为 16"))
}

/// Probe / ProbeAck 载荷：`client_ts_us`，u64 大端。
#[must_use]
pub const fn probe_payload(client_ts_us: u64) -> [u8; PROBE_PAYLOAD_LEN] {
    client_ts_us.to_be_bytes()
}

/// 解析 Probe / ProbeAck 载荷。
pub fn parse_probe_payload(body: &[u8]) -> Result<u64, VoiceError> {
    let bytes = <[u8; PROBE_PAYLOAD_LEN]>::try_from(body)
        .map_err(|_| VoiceError::BadProbePayload { got: body.len() })?;
    Ok(u64::from_be_bytes(bytes))
}

/// BindAck 载荷：`server_time_ms` u64 ++ `sender_id` u32，大端。
#[must_use]
pub const fn bind_ack_payload(server_time_ms: u64, sender_id: u32) -> [u8; BIND_ACK_PAYLOAD_LEN] {
    let mut out = [0u8; BIND_ACK_PAYLOAD_LEN];
    let ts = server_time_ms.to_be_bytes();
    let mut i = 0;
    while i < 8 {
        out[i] = ts[i];
        i += 1;
    }
    let id = sender_id.to_be_bytes();
    while i < BIND_ACK_PAYLOAD_LEN {
        out[i] = id[i - 8];
        i += 1;
    }
    out
}

/// 解析 BindAck 载荷。
pub fn parse_bind_ack_payload(body: &[u8]) -> Result<(u64, u32), VoiceError> {
    let bytes =
        <[u8; BIND_ACK_PAYLOAD_LEN]>::try_from(body).map_err(|_| VoiceError::BadBindPayload {
            got: body.len(),
            expected: BIND_ACK_PAYLOAD_LEN,
        })?;
    let server_time_ms = u64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]);
    let sender_id = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    Ok((server_time_ms, sender_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个字段填一个**互不相同**的值，这样任何偏移错位都会改变字节。
    fn sample() -> VoiceHeader {
        VoiceHeader {
            flags: flags::RED,
            red_len: 0x03,
            channel_epoch: 0x1234,
            sender_id: 0x0102_0304,
            seq: 0xA1B2_C3D4,
            timestamp: 0x1122_3344,
            opus_len: 0x0005,
        }
    }

    /// 组装一包「普通语音 + 冗余 + tag」，返回 (整包, 明文载荷)。
    fn sample_packet() -> ([u8; 44], [u8; 8]) {
        let header = sample();
        let body = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x11, 0x22, 0x33];
        let mut pkt = [0u8; 44];
        pkt[..HEADER_LEN].copy_from_slice(&header.encode());
        pkt[HEADER_LEN..HEADER_LEN + 8].copy_from_slice(&body);
        pkt[28..44].copy_from_slice(&[0x5A; TAG_LEN]);
        (pkt, body)
    }

    #[test]
    fn header_bytes_are_pinned() {
        let bytes = sample().encode();
        let expected: [u8; HEADER_LEN] = [
            0x48, 0x01, // magic, version
            0x01, 0x03, // flags = RED, red_len = 3
            0x12, 0x34, // channel_epoch
            0x01, 0x02, 0x03, 0x04, // sender_id
            0xA1, 0xB2, 0xC3, 0xD4, // seq
            0x11, 0x22, 0x33, 0x44, // timestamp
            0x00, 0x05, // opus_len
        ];
        assert_eq!(bytes, expected, "20 字节头的字段偏移被改动了");
    }

    #[test]
    fn every_field_owns_its_own_bytes() {
        let base = sample();
        // (字段名, 只改这一个字段后的编码, 应当变化的字节下标)
        let cases: [(&str, VoiceHeader, &[usize]); 6] = [
            (
                "flags",
                VoiceHeader {
                    flags: flags::RED | flags::RED_PLC,
                    ..base
                },
                &[2],
            ),
            ("red_len", VoiceHeader { red_len: 4, ..base }, &[3]),
            (
                "channel_epoch",
                VoiceHeader {
                    channel_epoch: 0x5678,
                    ..base
                },
                &[4, 5],
            ),
            (
                "sender_id",
                VoiceHeader {
                    sender_id: 0x0506_0708,
                    ..base
                },
                &[6, 7, 8, 9],
            ),
            (
                "seq",
                VoiceHeader {
                    seq: 0xB2C3_D4E5,
                    ..base
                },
                &[10, 11, 12, 13],
            ),
            (
                "timestamp",
                VoiceHeader {
                    timestamp: 0x2233_4455,
                    ..base
                },
                &[14, 15, 16, 17],
            ),
        ];
        let base_bytes = base.encode();
        for (name, header, expected) in cases {
            let got = header.encode();
            let changed: Vec<usize> = (0..HEADER_LEN)
                .filter(|i| got[*i] != base_bytes[*i])
                .collect();
            assert_eq!(changed, expected.to_vec(), "字段 {name} 的字节范围不对");
        }
        // opus_len 是 u16，占最后两字节。
        let got = VoiceHeader {
            opus_len: 0x0102,
            ..base
        }
        .encode();
        let changed: Vec<usize> = (0..HEADER_LEN)
            .filter(|i| got[*i] != base_bytes[*i])
            .collect();
        assert_eq!(changed, vec![18, 19]);
    }

    #[test]
    fn header_round_trips() {
        let h = sample();
        assert_eq!(VoiceHeader::parse(&h.encode()).unwrap(), h);
    }

    #[test]
    fn magic_is_checked() {
        let mut bytes = sample().encode();
        bytes[0] = 0x47;
        assert_eq!(
            VoiceHeader::parse(&bytes),
            Err(VoiceError::BadMagic { got: 0x47 })
        );
    }

    #[test]
    fn version_is_checked() {
        let mut bytes = sample().encode();
        bytes[1] = 0x02;
        assert_eq!(
            VoiceHeader::parse(&bytes),
            Err(VoiceError::BadVersion { got: 2 })
        );
    }

    #[test]
    fn reserved_flag_bits_are_rejected() {
        for bit in 5..8u8 {
            let mut bytes = sample().encode();
            bytes[2] |= 1 << bit;
            assert!(
                matches!(
                    VoiceHeader::parse(&bytes),
                    Err(VoiceError::ReservedFlags { .. })
                ),
                "保留位 bit{bit} 被静默接受了"
            );
        }
    }

    #[test]
    fn kind_bits_are_mutually_exclusive() {
        let bytes = VoiceHeader {
            flags: flags::BIND | flags::PROBE,
            ..VoiceHeader::default()
        }
        .encode();
        assert!(matches!(
            VoiceHeader::parse(&bytes),
            Err(VoiceError::ConflictingKind { .. })
        ));
    }

    #[test]
    fn red_flag_and_red_len_must_agree() {
        // 置了 RED 但长度为 0
        let bytes = VoiceHeader {
            flags: flags::RED,
            red_len: 0,
            ..VoiceHeader::default()
        }
        .encode();
        assert!(matches!(
            VoiceHeader::parse(&bytes),
            Err(VoiceError::RedFlagMismatch { .. })
        ));
        // 有长度但没置 RED
        let bytes = VoiceHeader {
            flags: 0,
            red_len: 9,
            ..VoiceHeader::default()
        }
        .encode();
        assert!(matches!(
            VoiceHeader::parse(&bytes),
            Err(VoiceError::RedFlagMismatch { .. })
        ));
    }

    #[test]
    fn plc_bit_requires_red() {
        let bytes = VoiceHeader {
            flags: flags::RED_PLC,
            ..VoiceHeader::default()
        }
        .encode();
        assert!(matches!(
            VoiceHeader::parse(&bytes),
            Err(VoiceError::PlcWithoutRed { .. })
        ));
    }

    #[test]
    fn special_packets_must_not_carry_red() {
        let bytes = VoiceHeader {
            flags: flags::PROBE | flags::RED,
            red_len: 4,
            opus_len: PROBE_PAYLOAD_LEN as u16,
            ..VoiceHeader::default()
        }
        .encode();
        assert!(matches!(
            VoiceHeader::parse(&bytes),
            Err(VoiceError::KindCarriesRed { .. })
        ));
    }

    #[test]
    fn aad_is_exactly_the_header() {
        let (pkt, _) = sample_packet();
        let parsed = VoicePacket::parse(&pkt, Direction::ServerToClient).unwrap();
        assert_eq!(parsed.aad().len(), HEADER_LEN);
        assert_eq!(parsed.aad(), &pkt[..HEADER_LEN]);
        // AAD 不能把载荷或 tag 圈进去，否则改载荷会被当成头被篡改（或反之）。
        assert_ne!(parsed.aad().as_ptr(), parsed.body().as_ptr());
    }

    #[test]
    fn payload_and_red_are_split_by_opus_len() {
        let (pkt, body) = sample_packet();
        let parsed = VoicePacket::parse(&pkt, Direction::ServerToClient).unwrap();
        assert_eq!(parsed.payload(), &body[..5]);
        assert_eq!(parsed.red(), &body[5..]);
        assert_eq!(parsed.body(), &body[..]);
        assert_eq!(parsed.tag(), Some(&[0x5A; TAG_LEN]));
    }

    #[test]
    fn truncation_is_rejected_at_every_length() {
        let (pkt, _) = sample_packet();
        for len in 0..pkt.len() {
            assert!(
                VoicePacket::parse(&pkt[..len], Direction::ServerToClient).is_err(),
                "截断到 {len} 字节居然解析成功了"
            );
        }
        // Bind 明文包同理。
        let mut bind = [0u8; 36];
        assert_eq!(encode_bind(&[7u8; 16], &mut bind), 36);
        for len in 0..36 {
            assert!(VoicePacket::parse(&bind[..len], Direction::ClientToServer).is_err());
        }
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let (pkt, _) = sample_packet();
        let mut longer = pkt.to_vec();
        longer.push(0);
        assert!(matches!(
            VoicePacket::parse(&longer, Direction::ServerToClient),
            Err(VoiceError::BadLength { .. })
        ));
    }

    #[test]
    fn bind_is_plaintext_only_from_the_client() {
        let mut bind = [0u8; 36];
        encode_bind(&[7u8; 16], &mut bind);
        let c2s = VoicePacket::parse(&bind, Direction::ClientToServer).unwrap();
        assert_eq!(c2s.kind(), PacketKind::Bind);
        assert_eq!(c2s.tag(), None);
        assert_eq!(parse_bind(&c2s).unwrap(), [7u8; 16]);
        // 从服务器方向看，Bind 包必须带 tag 且载荷是 12 字节 → 36 字节根本凑不成一包。
        // 这正是「方向是格式的一部分」的价值：同一个 36 字节缓冲不会在两端被解读成同一件事。
        assert_eq!(
            VoicePacket::parse(&bind, Direction::ServerToClient).unwrap_err(),
            VoiceError::BadLength {
                expected: 52,
                got: 36
            }
        );
    }

    #[test]
    fn bind_ack_has_a_different_length_from_bind() {
        // 48 = 20 + 12 + 16
        let mut ack = [0u8; 48];
        let header = VoiceHeader {
            flags: flags::BIND,
            opus_len: BIND_ACK_PAYLOAD_LEN as u16,
            ..VoiceHeader::default()
        };
        ack[..HEADER_LEN].copy_from_slice(&header.encode());
        ack[20..32].copy_from_slice(&bind_ack_payload(1_234, 7));
        let parsed = VoicePacket::parse(&ack, Direction::ServerToClient).unwrap();
        assert_eq!(parse_bind_ack_payload(parsed.body()).unwrap(), (1_234, 7));
        // 同一串字节从客户端方向看长度对不上。
        assert!(matches!(
            VoicePacket::parse(&ack, Direction::ClientToServer),
            Err(VoiceError::BadLength { .. })
        ));
    }

    #[test]
    fn silence_and_bind_are_both_36_bytes_but_differ_by_flags() {
        let mut silence = [0u8; 36];
        silence[..HEADER_LEN].copy_from_slice(
            &VoiceHeader {
                flags: flags::SILENCE,
                ..VoiceHeader::default()
            }
            .encode(),
        );
        let parsed = VoicePacket::parse(&silence, Direction::ServerToClient).unwrap();
        assert_eq!(parsed.kind(), PacketKind::Silence);
        assert_eq!(parsed.body().len(), 0);
        assert_eq!(parsed.tag(), Some(&[0u8; TAG_LEN]));
    }

    #[test]
    fn silence_with_payload_is_rejected() {
        let header = VoiceHeader {
            flags: flags::SILENCE,
            opus_len: 1,
            ..VoiceHeader::default()
        };
        let mut pkt = vec![0u8; HEADER_LEN + 1 + TAG_LEN];
        pkt[..HEADER_LEN].copy_from_slice(&header.encode());
        assert!(matches!(
            VoicePacket::parse(&pkt, Direction::ServerToClient),
            Err(VoiceError::SilenceCarriesPayload { .. })
        ));
    }

    #[test]
    fn empty_voice_frame_is_rejected() {
        let header = VoiceHeader {
            opus_len: 0,
            ..VoiceHeader::default()
        };
        let mut pkt = [0u8; HEADER_LEN + TAG_LEN];
        pkt[..HEADER_LEN].copy_from_slice(&header.encode());
        assert_eq!(
            VoicePacket::parse(&pkt, Direction::ServerToClient).unwrap_err(),
            VoiceError::EmptyVoice
        );
    }

    #[test]
    fn oversize_packet_is_rejected_before_anything_else() {
        let pkt = vec![0u8; MAX_ON_WIRE + 1];
        assert_eq!(
            VoicePacket::parse(&pkt, Direction::ServerToClient).unwrap_err(),
            VoiceError::TooLarge {
                len: MAX_ON_WIRE + 1
            }
        );
    }

    #[test]
    fn probe_payload_round_trips() {
        let body = probe_payload(0x0102_0304_0506_0708);
        assert_eq!(body, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(parse_probe_payload(&body).unwrap(), 0x0102_0304_0506_0708);
        assert!(parse_probe_payload(&body[..7]).is_err());
    }

    #[test]
    fn bind_ack_payload_round_trips() {
        let body = bind_ack_payload(0x0102_0304_0506_0708, 0x090A_0B0C);
        assert_eq!(body, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(
            parse_bind_ack_payload(&body).unwrap(),
            (0x0102_0304_0506_0708, 0x090A_0B0C)
        );
        assert!(parse_bind_ack_payload(&body[..11]).is_err());
    }

    #[test]
    fn wire_len_matches_reality() {
        let header = sample();
        assert_eq!(wire_len(&header, true), 44);
        assert_eq!(wire_len(&header, false), 28);
        let (pkt, _) = sample_packet();
        assert_eq!(wire_len(&header, true), pkt.len());
    }
}
