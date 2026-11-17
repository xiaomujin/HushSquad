//! 协议层：控制面消息（protobuf）与语音包线格式（手写二进制）。
//!
//! 依据 `docs/协议草案-v0.1.md` 与 `docs/proto/hushsquad-control-v1.proto`。
//! 语音包头部固定 20 字节、大端；字段偏移改变即为破坏性改动，必须提升主版本。
//!
//! 命名空间约定：`sender_id` 会话内**永不复用**（它参与 AEAD nonce 构造）。
//!
//! 这一层是**纯格式**：没有密钥、没有时钟、没有 I/O、没有分配（除控制面编码外）。
//! 认证在 `transport`，状态机在 `client-core` / `server-core`。

#![deny(unsafe_code)]

pub mod control;
pub mod voice;

pub use control::{
    ControlError, ControlMessage, Framer, MAX_CONTROL_FRAME, PROTOCOL_VERSION, caps, decode_body,
    encode_body, encode_frame,
};
pub use voice::{
    BIND_ACK_PAYLOAD_LEN, Direction, FRAME_SAMPLES, HEADER_LEN, MAGIC, MAX_ON_WIRE,
    PROBE_PAYLOAD_LEN, PacketKind, SAMPLE_RATE, TAG_LEN, VERSION, VOICE_TOKEN_LEN, VoiceError,
    VoiceHeader, VoicePacket, flags, wire_len,
};
