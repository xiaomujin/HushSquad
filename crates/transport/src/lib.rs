//! 传输层：加密 UDP 的**可测试内核**（sans-io）。
//!
//! 决策依据：自建加密 UDP，不引入 ICE/STUN/TURN/SDP；服务器即中继（v1 无 P2P）。
//! 语音面用 ChaCha20-Poly1305，nonce = `0u32 || sender_id || seq`，AAD = 语音包前 20 字节头。
//! **禁止**实现 OCB2（已被攻破，见技术设计 §7.3）。
//!
//! ## 为什么没有 socket
//!
//! 这一层只有三个纯逻辑件：密钥派生、滑动窗口、AEAD 封装。它们**不碰 I/O**，
//! 因此每条安全性质都能在单元测试里用字节数组验证，而不是「跑起来看看」。
//! 真正的 `recvfrom` / `sendto` 在 `server-core` 与 `client-core` 里，
//! 那两处只负责搬运缓冲区和决定发给谁。
//!
//! ## 与协议草案 §4 的一处偏离（有意）
//!
//! §4 写的校验顺序是「窗口去重 → AEAD」。这里反过来：**先 AEAD，再去重**。
//! 原因：`seq` 在明文头里，先更新窗口就意味着一个能伪造源地址的离线攻击者
//! 只要发一个高 `seq` 的垃圾包，就能把窗口推到前面，让合法包全被判成「太老」。
//! 代价是每个包多一次 Poly1305 校验（100 字节载荷约 1 µs），相对「窗口可被推走」
//! 这个漏洞不值一提。见 `server-core` 的 `voice` 模块与测量文档。

#![deny(unsafe_code)]

pub mod key;
pub mod replay;
pub mod seal;

pub use key::{
    EXPORTER_CONTEXT_LEN, EXPORTER_LABEL, EXPORTER_LEN, KEY_LEN, Key, VoiceKeys, derive_voice_keys,
    exporter_context, nonce,
};
pub use replay::{ReplayStats, ReplayWindow, Verdict};
pub use seal::{CryptoError, open, open_packet, seal, seal_packet};
