//! 控制面：protobuf 消息 + **u32 大端长度前缀**分帧。
//!
//! 帧格式（协议草案 §0、`.proto` 头注释）：`u32 BE 长度 + ControlMessage 字节`，
//! 单帧上限 64 KiB。控制面走 TLS 1.3 over TCP，是字节流，所以分帧必须有状态 ——
//! `Framer` 就是那个状态机，它只做「攒够一帧就吐出来」，不解释语义。

use prost::Message;

/// 生成的 protobuf 类型（`docs/proto/hushsquad-control-v1.proto`）。
///
/// 生成代码不受我们控制，因此整体放行 lint：`-D warnings` 是给**我们的**代码用的，
/// 不是用来跟 protoc 的输出较劲的。
#[allow(clippy::all, clippy::pedantic, clippy::restriction, missing_docs)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/hushsquad.v1.rs"));
}

pub use proto::*;

/// 当前控制面主版本。
pub const PROTOCOL_VERSION: u32 = 1;
/// 单帧上限：64 KiB（协议草案 §8）。
pub const MAX_CONTROL_FRAME: usize = 64 * 1024;
/// 长度前缀本身。
pub const LENGTH_PREFIX_LEN: usize = 4;

/// 能力位（`ClientHello.caps` / `ServerHello.caps`）。双方都置位才启用。
pub mod caps {
    /// 语音包级冗余。
    pub const RED: u64 = 1;
    /// 静音帧与 DTX 边界。
    pub const DTX: u64 = 2;
    /// 多频道监听。
    pub const LISTEN_MULTI: u64 = 4;
    /// 定向语音。
    pub const WHISPER: u64 = 8;
    /// 服务器 1 Hz 统计。
    pub const STATS: u64 = 16;
}

/// 控制面错误。
#[derive(Debug)]
pub enum ControlError {
    /// 长度前缀或实际缓冲超过单帧上限。
    FrameTooLarge {
        /// 自述或被拒绝的长度。
        len: usize,
    },
    /// protobuf 解码失败。
    Decode(prost::DecodeError),
    /// protobuf 编码失败（缓冲区不足）。
    Encode(prost::EncodeError),
}

impl core::fmt::Display for ControlError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::FrameTooLarge { len } => {
                write!(f, "控制帧 {len} 字节超过上限 {MAX_CONTROL_FRAME}")
            }
            Self::Decode(e) => write!(f, "控制帧解码失败：{e}"),
            Self::Encode(e) => write!(f, "控制帧编码失败：{e}"),
        }
    }
}

impl std::error::Error for ControlError {}

impl From<prost::DecodeError> for ControlError {
    fn from(e: prost::DecodeError) -> Self {
        Self::Decode(e)
    }
}

impl From<prost::EncodeError> for ControlError {
    fn from(e: prost::EncodeError) -> Self {
        Self::Encode(e)
    }
}

impl ControlMessage {
    /// 生效的协议版本：`0`（未填）视为 1，见 `.proto` 的字段注释。
    #[must_use]
    pub const fn effective_protocol(&self) -> u32 {
        if self.protocol == 0 {
            PROTOCOL_VERSION
        } else {
            self.protocol
        }
    }

    /// 消息类型名，用于日志与指标（P5-6 只记事件，不记内容）。
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            None => "none",
            Some(proto::control_message::Kind::ClientHello(_)) => "client_hello",
            Some(proto::control_message::Kind::ServerHello(_)) => "server_hello",
            Some(proto::control_message::Kind::Auth(_)) => "auth",
            Some(proto::control_message::Kind::AuthResult(_)) => "auth_result",
            Some(proto::control_message::Kind::ChannelSync(_)) => "channel_sync",
            Some(proto::control_message::Kind::ChannelAdded(_)) => "channel_added",
            Some(proto::control_message::Kind::ChannelRemoved(_)) => "channel_removed",
            Some(proto::control_message::Kind::ChannelUpdated(_)) => "channel_updated",
            Some(proto::control_message::Kind::JoinChannel(_)) => "join_channel",
            Some(proto::control_message::Kind::LeaveChannel(_)) => "leave_channel",
            Some(proto::control_message::Kind::UserJoined(_)) => "user_joined",
            Some(proto::control_message::Kind::UserLeft(_)) => "user_left",
            Some(proto::control_message::Kind::UserUpdated(_)) => "user_updated",
            Some(proto::control_message::Kind::SetSelfState(_)) => "set_self_state",
            Some(proto::control_message::Kind::SetListening(_)) => "set_listening",
            Some(proto::control_message::Kind::Whisper(_)) => "whisper",
            Some(proto::control_message::Kind::RekeyRequest(_)) => "rekey_request",
            Some(proto::control_message::Kind::RekeyResponse(_)) => "rekey_response",
            Some(proto::control_message::Kind::Ping(_)) => "ping",
            Some(proto::control_message::Kind::Pong(_)) => "pong",
            Some(proto::control_message::Kind::Stats(_)) => "stats",
            Some(proto::control_message::Kind::Error(_)) => "error",
        }
    }
}

/// 把一条消息编码成**带长度前缀**的完整帧，追加到 `out`。
pub fn encode_frame(msg: &ControlMessage, out: &mut Vec<u8>) -> Result<(), ControlError> {
    let len = msg.encoded_len();
    if len > MAX_CONTROL_FRAME {
        return Err(ControlError::FrameTooLarge { len });
    }
    out.reserve(LENGTH_PREFIX_LEN + len);
    out.extend_from_slice(&(len as u32).to_be_bytes());
    msg.encode(out)?;
    Ok(())
}

/// 把一条消息编码成裸 body（不含长度前缀），主要给测试与抓包工具用。
pub fn encode_body(msg: &ControlMessage) -> Result<Vec<u8>, ControlError> {
    let mut out = Vec::with_capacity(msg.encoded_len());
    msg.encode(&mut out)?;
    Ok(out)
}

/// 解码裸 body（不含长度前缀）。
pub fn decode_body(bytes: &[u8]) -> Result<ControlMessage, ControlError> {
    Ok(ControlMessage::decode(bytes)?)
}

/// TCP 字节流 → 控制帧的状态机。
#[derive(Debug, Default)]
pub struct Framer {
    buf: Vec<u8>,
}

impl Framer {
    /// 新建。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入刚读到的字节。
    ///
    /// 这里只做**粗粒度**上限检查（缓冲不会超过「一帧上限 + 前缀」），
    /// 真正的长度校验在 `next` 里按长度前缀做。
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), ControlError> {
        if self.buf.len() + bytes.len() > MAX_CONTROL_FRAME + LENGTH_PREFIX_LEN {
            return Err(ControlError::FrameTooLarge {
                len: self.buf.len() + bytes.len(),
            });
        }
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    /// 取出下一帧；不够一帧时返回 `Ok(None)`。
    ///
    /// 不叫 `next` 是因为它和 `Iterator::next` 语义不同（会失败、且不消耗「迭代器」）。
    pub fn poll_frame(&mut self) -> Result<Option<ControlMessage>, ControlError> {
        if self.buf.len() < LENGTH_PREFIX_LEN {
            return Ok(None);
        }
        let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if len > MAX_CONTROL_FRAME {
            return Err(ControlError::FrameTooLarge { len });
        }
        if self.buf.len() < LENGTH_PREFIX_LEN + len {
            return Ok(None);
        }
        let msg = ControlMessage::decode(&self.buf[LENGTH_PREFIX_LEN..LENGTH_PREFIX_LEN + len])?;
        self.buf.drain(..LENGTH_PREFIX_LEN + len);
        Ok(Some(msg))
    }

    /// 当前积压的字节数。
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::control_message::Kind;

    fn hello() -> ControlMessage {
        ControlMessage {
            protocol: PROTOCOL_VERSION,
            request_id: 0,
            kind: Some(Kind::ClientHello(ClientHello {
                protocol_min: 1,
                protocol_max: 1,
                client_version: "0.1.0".into(),
                platform: "windows".into(),
                device_id: "abc".into(),
                caps: caps::RED | caps::DTX,
            })),
        }
    }

    #[test]
    fn frame_round_trips() {
        let msg = hello();
        let mut framed = Vec::new();
        encode_frame(&msg, &mut framed).unwrap();
        // 长度前缀必须是大端，且等于 body 长度。
        let declared = u32::from_be_bytes([framed[0], framed[1], framed[2], framed[3]]) as usize;
        assert_eq!(declared, framed.len() - LENGTH_PREFIX_LEN);

        let mut framer = Framer::new();
        framer.push(&framed).unwrap();
        assert_eq!(framer.poll_frame().unwrap().unwrap(), msg);
        assert!(framer.poll_frame().unwrap().is_none());
        assert_eq!(framer.buffered(), 0);
    }

    #[test]
    fn framer_survives_one_byte_at_a_time() {
        let msg = hello();
        let mut framed = Vec::new();
        encode_frame(&msg, &mut framed).unwrap();
        let mut framer = Framer::new();
        for (i, byte) in framed.iter().enumerate() {
            framer.push(&[*byte]).unwrap();
            let got = framer.poll_frame().unwrap();
            if i + 1 == framed.len() {
                assert_eq!(got.unwrap(), msg);
            } else {
                assert!(got.is_none(), "第 {} 字节就吐出了帧", i + 1);
            }
        }
    }

    #[test]
    fn framer_splits_back_to_back_frames() {
        let a = hello();
        let b = ControlMessage {
            protocol: 1,
            request_id: 7,
            kind: Some(Kind::Ping(Ping { ts_us: 123 })),
        };
        let mut framed = Vec::new();
        encode_frame(&a, &mut framed).unwrap();
        encode_frame(&b, &mut framed).unwrap();
        let mut framer = Framer::new();
        framer.push(&framed).unwrap();
        assert_eq!(framer.poll_frame().unwrap().unwrap(), a);
        assert_eq!(framer.poll_frame().unwrap().unwrap(), b);
        assert!(framer.poll_frame().unwrap().is_none());
    }

    #[test]
    fn oversize_length_prefix_is_rejected_without_buffering_it() {
        let mut framer = Framer::new();
        let mut hdr = ((MAX_CONTROL_FRAME as u32) + 1).to_be_bytes().to_vec();
        hdr.extend_from_slice(&[0u8; 8]);
        framer.push(&hdr).unwrap();
        assert!(matches!(
            framer.poll_frame(),
            Err(ControlError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn unbounded_buffer_growth_is_refused() {
        let mut framer = Framer::new();
        let chunk = vec![0u8; 40 * 1024];
        framer.push(&chunk).unwrap();
        assert!(framer.push(&chunk).is_err());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        // proto3 规则：未知字段必须忽略，不得报错（协议草案 §9）。
        let msg = ControlMessage {
            protocol: 1,
            request_id: 0,
            kind: Some(Kind::Ping(Ping { ts_us: 1 })),
        };
        let mut body = encode_body(&msg).unwrap();
        // 追加一个未知字段：字段号 15，varint，值 42。
        body.extend_from_slice(&[0x78, 0x2A]);
        assert_eq!(decode_body(&body).unwrap(), msg);
    }

    #[test]
    fn missing_protocol_means_v1() {
        let msg = ControlMessage::default();
        assert_eq!(msg.protocol, 0);
        assert_eq!(msg.effective_protocol(), PROTOCOL_VERSION);
        assert_eq!(msg.kind_name(), "none");
    }

    #[test]
    fn kind_names_cover_the_handshake() {
        assert_eq!(hello().kind_name(), "client_hello");
    }
}
