//! 语音密钥派生（协议草案 §3）。
//!
//! ```text
//! base = TLS1.3_Exporter(label = "EXPORTER-hushsquad-voice-v1",
//!                        context = session_id(8) || user_id(4) || voice_token(16),
//!                        length = 64)
//! c2s_key(epoch) = HKDF-SHA256(base[0..32],  salt = session_id, info = "c2s" || key_epoch)
//! s2c_key(epoch) = HKDF-SHA256(base[32..64], salt = session_id, info = "s2c" || key_epoch)
//! ```
//!
//! TLS 导出器（RFC 8446 §7.5）由 `rustls` 提供：它保证两条不同 TLS 连接拿到的
//! `base` 不同，即使 `session_id` 撞了也一样。这里只做「给定 64 字节 base 之后」的部分，
//! 因此可以脱机测试 —— 真正的 exporter 调用在 P5-3 的 TLS 层。

use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// TLS 导出器标签。改它就是破坏性改动：老客户端会解不开新服务器的包。
pub const EXPORTER_LABEL: &[u8] = b"EXPORTER-hushsquad-voice-v1";
/// 导出器输出长度：两个方向各 32 字节。
pub const EXPORTER_LEN: usize = 64;
/// 单个方向的密钥长度（ChaCha20-Poly1305）。
pub const KEY_LEN: usize = 32;
/// 导出器上下文长度：8 + 16。
///
/// **协议草案 §3 原本写的是 `session_id(8) || user_id(4) || voice_token(16)`（28 字节），
/// 实装时发现它有个循环依赖**：客户端要算这个上下文，就得先知道 `user_id`，而 `user_id`
/// 要等认证结果；可服务器那边如果也要等认证才派生，就不能在 ServerHello 之前把密钥准备好。
/// 把 `user_id` 去掉即可 —— 上下文的唯一职责是域分离，而 `session_id` 已经是一次连接
/// 唯一，`voice_token` 是 16 字节随机量。见协议草案 §12.7。
pub const EXPORTER_CONTEXT_LEN: usize = 24;

/// 一个方向的 32 字节密钥。
///
/// `Debug` 被刻意做成脱敏的 —— 密钥进日志是安全事故，而 `derive(Debug)` 会让它悄悄发生。
#[derive(Clone, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct Key([u8; KEY_LEN]);

impl Key {
    /// 从原始字节构造。
    #[must_use]
    pub const fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// 全部置零的密钥，仅用于「还没协商出密钥」的占位。
    #[must_use]
    pub const fn zeroed() -> Self {
        Self([0u8; KEY_LEN])
    }

    /// 原始字节。调用点应当立刻用完，不要存起来。
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    /// 是不是还没协商出来的全零密钥。
    #[must_use]
    pub fn is_zeroed(&self) -> bool {
        self.0 == [0u8; KEY_LEN]
    }
}

impl core::fmt::Debug for Key {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Key(<redacted>)")
    }
}

/// 一对方向密钥。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceKeys {
    /// 客户端 → 服务器。
    pub c2s: Key,
    /// 服务器 → 客户端。
    pub s2c: Key,
}

impl VoiceKeys {
    /// 两个方向都是全零密钥（尚未协商）。
    #[must_use]
    pub fn zeroed() -> Self {
        Self {
            c2s: Key::zeroed(),
            s2c: Key::zeroed(),
        }
    }
}

/// 导出器上下文：`session_id` u64 大端 ++ `voice_token` 16 字节。
#[must_use]
pub fn exporter_context(
    session_id: u64,
    voice_token: &[u8; protocol::VOICE_TOKEN_LEN],
) -> [u8; EXPORTER_CONTEXT_LEN] {
    let mut out = [0u8; EXPORTER_CONTEXT_LEN];
    out[..8].copy_from_slice(&session_id.to_be_bytes());
    out[8..].copy_from_slice(voice_token);
    out
}

/// HKDF 的 `info` 字段：方向名 ++ `key_epoch` u16 大端。
fn hkdf_info(direction: &[u8; 3], key_epoch: u16) -> [u8; 5] {
    let mut info = [0u8; 5];
    info[..3].copy_from_slice(direction);
    info[3..].copy_from_slice(&key_epoch.to_be_bytes());
    info
}

fn expand(ikm: &[u8], salt: &[u8], info: &[u8]) -> Key {
    let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut okm = [0u8; KEY_LEN];
    hk.expand(info, &mut okm)
        .expect("32 字节远小于 HKDF-SHA256 的输出上限 8160 字节");
    let key = Key::from_bytes(okm);
    okm.zeroize();
    key
}

/// 从 64 字节导出器输出派生某一 `key_epoch` 的两个方向密钥。
///
/// `salt` 用 `session_id` 的大端 8 字节：同一对端在同一 TLS 连接内换 epoch 时，
/// 只有 `info` 变；不同会话之间连 salt 都不同。
#[must_use]
pub fn derive_voice_keys(
    exporter: &[u8; EXPORTER_LEN],
    session_id: u64,
    key_epoch: u16,
) -> VoiceKeys {
    let salt = session_id.to_be_bytes();
    VoiceKeys {
        c2s: expand(&exporter[..32], &salt, &hkdf_info(b"c2s", key_epoch)),
        s2c: expand(&exporter[32..], &salt, &hkdf_info(b"s2c", key_epoch)),
    }
}

/// AEAD nonce：`0u32` ++ `sender_id` u32 大端 ++ `seq` u32 大端（协议草案 §3）。
///
/// 前 4 字节固定为 0 是**故意的**：留给将来的 64 位序号，同时让
/// 「sender_id 复用」这件事在 nonce 上表现为灾难性重复 —— 所以协议规定
/// `sender_id` 会话内永不复用。
#[must_use]
pub fn nonce(sender_id: u32, seq: u32) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[4..8].copy_from_slice(&sender_id.to_be_bytes());
    out[8..].copy_from_slice(&seq.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exporter() -> [u8; EXPORTER_LEN] {
        let mut e = [0u8; EXPORTER_LEN];
        for (i, b) in e.iter_mut().enumerate() {
            *b = i as u8;
        }
        e
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn exporter_context_is_24_bytes_big_endian() {
        let token = [0xAAu8; 16];
        let ctx = exporter_context(0x0102_0304_0506_0708, &token);
        assert_eq!(ctx.len(), EXPORTER_CONTEXT_LEN);
        assert_eq!(&ctx[..8], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&ctx[8..], &[0xAA; 16]);
    }

    #[test]
    fn nonce_layout_is_pinned() {
        assert_eq!(
            nonce(0x0102_0304, 0x0506_0708),
            [0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8]
        );
    }

    #[test]
    fn nonce_changes_with_both_fields() {
        assert_ne!(nonce(1, 2), nonce(2, 1));
        assert_ne!(nonce(0, 0), nonce(0, 1));
        assert_ne!(nonce(0, 0), nonce(1, 0));
    }

    #[test]
    fn hkdf_matches_rfc5869_test_case_1() {
        // 证明 HKDF 这层是接对了的：RFC 5869 A.1 的前 32 字节。
        let ikm = [0x0bu8; 22];
        let salt = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
        ];
        let info = [0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9];
        let key = expand(&ikm, &salt, &info);
        assert_eq!(
            hex(key.as_bytes()),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf"
        );
    }

    #[test]
    fn directions_get_different_keys() {
        let keys = derive_voice_keys(&exporter(), 1, 0);
        assert_ne!(keys.c2s.as_bytes(), keys.s2c.as_bytes());
        assert!(!keys.c2s.is_zeroed());
    }

    #[test]
    fn epochs_get_different_keys() {
        let a = derive_voice_keys(&exporter(), 1, 0);
        let b = derive_voice_keys(&exporter(), 1, 1);
        assert_ne!(a.c2s.as_bytes(), b.c2s.as_bytes());
        assert_ne!(a.s2c.as_bytes(), b.s2c.as_bytes());
    }

    #[test]
    fn sessions_get_different_keys() {
        let a = derive_voice_keys(&exporter(), 1, 0);
        let b = derive_voice_keys(&exporter(), 2, 0);
        assert_ne!(a.c2s.as_bytes(), b.c2s.as_bytes());
    }

    #[test]
    fn derivation_is_deterministic() {
        assert_eq!(
            derive_voice_keys(&exporter(), 42, 7).c2s,
            derive_voice_keys(&exporter(), 42, 7).c2s
        );
    }

    #[test]
    fn keys_are_pinned() {
        // 固定向量：改动标签、盐、info 或 HKDF 参数都会让这串字节变，
        // 于是「跨版本互通」在 CI 上立刻暴露。这不是正确性证明，是「没被无意改动」的证明。
        let keys = derive_voice_keys(&exporter(), 0, 0);
        let both = format!("{}|{}", hex(keys.c2s.as_bytes()), hex(keys.s2c.as_bytes()));
        assert_eq!(both, VECTOR);
    }

    const VECTOR: &str = "4f9186aa46cb28772a69c37d18d9a66c961257d55a7ecf4d3030aa0c7d08154a|465ca0fcfae43119c3af53912c9060e688ce0d0c2fcaa10ae2dc6a6576fbd548";

    #[test]
    fn debug_does_not_leak_key_bytes() {
        let keys = derive_voice_keys(&exporter(), 1, 0);
        let text = format!("{keys:?}");
        assert!(
            !text.contains(&hex(keys.c2s.as_bytes())),
            "Debug 把密钥打出来了：{text}"
        );
        assert!(text.contains("redacted"));
    }
}
