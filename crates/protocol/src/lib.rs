//! 协议层：控制面消息（protobuf）与语音包线格式（手写二进制）。
//!
//! 依据 `docs/协议草案-v0.1.md` 与 `docs/proto/hushsquad-control-v1.proto`。
//! 语音包头部固定 20 字节、大端；字段偏移改变即为破坏性改动，必须提升主版本。
//!
//! 命名空间约定：`sender_id` 会话内**永不复用**（它参与 AEAD nonce 构造）。

#![deny(unsafe_code)]
