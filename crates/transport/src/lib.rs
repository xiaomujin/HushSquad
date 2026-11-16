//! 传输层：加密 UDP。
//!
//! 决策依据：自建加密 UDP，不引入 ICE/STUN/TURN/SDP；服务器即中继（v1 无 P2P）。
//! 语音面用 ChaCha20-Poly1305，nonce = 0u32 ‖ sender_id ‖ seq，AAD = 语音包前 20 字节头。
//! **禁止**实现 OCB2（已被攻破，见技术设计 §7.3）。
