//! ChaCha20-Poly1305 原地封装 / 解封。
//!
//! 约定（协议草案 §4）：**AAD 恰好是 20 字节头**，密文覆盖 `opus_data ++ red_data`，
//! tag 追加在末尾。因此：
//!
//! - 服务器不能在不被察觉的情况下改 `sender_id` / `seq` / `channel_epoch`
//!   （它们都在 AAD 里，而且 `sender_id` / `seq` 还决定 nonce）；
//! - nonce 直接取自头里的 `sender_id` 与 `seq`，所以「nonce 和头不一致」
//!   这种 bug 在这个 API 下不可能发生 —— 没有第二个地方能设 nonce。
//!
//! 全程原地、零分配：网络线程每 10 ms 要处理 8 个这样的包。

use chacha20poly1305::{ChaCha20Poly1305, KeyInit, aead::AeadInOut};
use protocol::{Direction, HEADER_LEN, TAG_LEN, VoiceError, VoiceHeader, VoicePacket};

use crate::key::{Key, nonce};

/// 加解密错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoError {
    /// 头部本身不合法（magic/version/flags/长度）。
    Malformed(VoiceError),
    /// 认证失败：密钥不对、AAD 被改、载荷被改，或者这是个伪造包。
    AuthFailed,
    /// 缓冲区长度与头部自述的长度不符。
    BadLength {
        /// 头部推出的应有长度。
        expected: usize,
        /// 实际长度。
        got: usize,
    },
}

impl core::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Malformed(e) => write!(f, "头部非法：{e}"),
            Self::AuthFailed => f.write_str("AEAD 认证失败"),
            Self::BadLength { expected, got } => write!(f, "包长应为 {expected}，实际 {got}"),
        }
    }
}

impl std::error::Error for CryptoError {}

impl From<VoiceError> for CryptoError {
    fn from(e: VoiceError) -> Self {
        Self::Malformed(e)
    }
}

/// 把一包**明文**原地加密成密文包（头不动，载荷加密，末尾写 tag）。
///
/// `packet` 必须是完整一包：`20 + body_len + 16` 字节，其中
/// `[20 .. 20+body_len]` 是明文，最后 16 字节留空。
pub fn seal(key: &Key, packet: &mut [u8]) -> Result<(), CryptoError> {
    let header = VoiceHeader::parse(packet)?;
    let body_len = header.body_len();
    let expected = HEADER_LEN + body_len + TAG_LEN;
    if packet.len() != expected {
        return Err(CryptoError::BadLength {
            expected,
            got: packet.len(),
        });
    }
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let nonce = nonce(header.sender_id, header.seq);

    let (aad, rest) = packet.split_at_mut(HEADER_LEN);
    let (body, tag_slot) = rest.split_at_mut(body_len);
    let tag = cipher
        .encrypt_inout_detached(&nonce.into(), aad, body.into())
        .map_err(|_| CryptoError::AuthFailed)?;
    tag_slot.copy_from_slice(&tag);
    Ok(())
}

/// 校验并原地解密。成功之后 `packet` 的载荷段是明文。
pub fn open(key: &Key, packet: &mut [u8]) -> Result<(), CryptoError> {
    let header = VoiceHeader::parse(packet)?;
    let body_len = header.body_len();
    let expected = HEADER_LEN + body_len + TAG_LEN;
    if packet.len() != expected {
        return Err(CryptoError::BadLength {
            expected,
            got: packet.len(),
        });
    }
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let nonce = nonce(header.sender_id, header.seq);

    let (aad, rest) = packet.split_at_mut(HEADER_LEN);
    let (body, tag_slot) = rest.split_at_mut(body_len);
    let tag = <&[u8; TAG_LEN]>::try_from(&tag_slot[..]).expect("上面已校验长度");
    cipher
        .decrypt_inout_detached(&nonce.into(), aad, body.into(), tag.into())
        .map_err(|_| CryptoError::AuthFailed)?;
    Ok(())
}

/// 发送侧一把梭：写头、拷载荷、加密。返回写入 `out` 的总长度。
///
/// `out` 至少要 `HEADER_LEN + body.len() + TAG_LEN` 字节。
pub fn seal_packet(
    key: &Key,
    header: VoiceHeader,
    body: &[u8],
    out: &mut [u8],
) -> Result<usize, CryptoError> {
    let total = HEADER_LEN + body.len() + TAG_LEN;
    if out.len() < total {
        return Err(CryptoError::BadLength {
            expected: total,
            got: out.len(),
        });
    }
    out[..HEADER_LEN].copy_from_slice(&header.encode());
    out[HEADER_LEN..HEADER_LEN + body.len()].copy_from_slice(body);
    seal(key, &mut out[..total])?;
    Ok(total)
}

/// 接收侧一把梭：校验 + 解密 + 解析成视图。
///
/// 返回的 `VoicePacket` 借用 `buf`，此时载荷已经是明文。
pub fn open_packet<'a>(
    key: &Key,
    buf: &'a mut [u8],
    dir: Direction,
) -> Result<VoicePacket<'a>, CryptoError> {
    open(key, buf)?;
    Ok(VoicePacket::parse(buf, dir)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::{VoiceKeys, derive_voice_keys};
    use protocol::{PacketKind, flags, wire_len};

    fn keys() -> VoiceKeys {
        let mut e = [0u8; 64];
        for (i, b) in e.iter_mut().enumerate() {
            *b = i as u8;
        }
        derive_voice_keys(&e, 7, 0)
    }

    fn header(seq: u32) -> VoiceHeader {
        VoiceHeader {
            flags: 0,
            red_len: 0,
            channel_epoch: 3,
            sender_id: 42,
            seq,
            timestamp: seq * 480,
            opus_len: 12,
        }
    }

    /// 头里的 `opus_len` 按载荷长度补齐（绝大多数测试用）。
    fn build(mut header: VoiceHeader, body: &[u8]) -> Vec<u8> {
        header.opus_len = body.len() as u16;
        seal_with(header, body)
    }

    /// 头原样使用，不补 `opus_len`（测 RED 这种「载荷长度 ≠ opus_len」的布局）。
    fn seal_with(header: VoiceHeader, body: &[u8]) -> Vec<u8> {
        let mut buf = vec![0u8; HEADER_LEN + body.len() + TAG_LEN];
        let len = seal_packet(&keys().c2s, header, body, &mut buf).unwrap();
        assert_eq!(len, buf.len());
        buf
    }

    #[test]
    fn round_trip_restores_the_plaintext() {
        let k = keys();
        let body = b"opus-payload";
        let mut pkt = build(header(1), body);
        // 网线上不该看见明文。
        assert!(!pkt.windows(body.len()).any(|w| w == body));
        open(&k.c2s, &mut pkt).unwrap();
        assert_eq!(&pkt[HEADER_LEN..HEADER_LEN + body.len()], body);
    }

    #[test]
    fn the_wrong_key_fails() {
        let k = keys();
        let mut pkt = build(header(1), b"x");
        assert_eq!(open(&k.s2c, &mut pkt), Err(CryptoError::AuthFailed));
    }

    #[test]
    fn every_header_byte_is_authenticated() {
        // AAD = 整个 20 字节头。逐字节翻一位，全部必须认证失败。
        let k = keys();
        for i in 0..HEADER_LEN {
            let mut pkt = build(header(5), b"payload!");
            pkt[i] ^= 0x01;
            let got = open(&k.c2s, &mut pkt);
            assert!(got.is_err(), "第 {i} 字节被改了却还能解出来：{got:?}");
        }
    }

    #[test]
    fn the_payload_is_authenticated() {
        let k = keys();
        let mut pkt = build(header(5), b"payload!");
        let last = HEADER_LEN;
        pkt[last] ^= 0x80;
        assert_eq!(open(&k.c2s, &mut pkt), Err(CryptoError::AuthFailed));
    }

    #[test]
    fn the_tag_is_authenticated() {
        let k = keys();
        let mut pkt = build(header(5), b"payload!");
        let n = pkt.len();
        pkt[n - 1] ^= 0x80;
        assert_eq!(open(&k.c2s, &mut pkt), Err(CryptoError::AuthFailed));
    }

    #[test]
    fn a_cut_tag_is_rejected_before_decrypting() {
        let k = keys();
        let mut pkt = build(header(5), b"payload!");
        pkt.pop();
        assert!(matches!(
            open(&k.c2s, &mut pkt),
            Err(CryptoError::BadLength { .. })
        ));
    }

    #[test]
    fn the_same_plaintext_produces_different_ciphertext_per_seq() {
        // seq 参与 nonce，所以同一帧内容在不同 seq 下密文不同。
        let a = build(header(1), b"same-frame");
        let b = build(header(2), b"same-frame");
        assert_ne!(a[HEADER_LEN..], b[HEADER_LEN..]);
    }

    #[test]
    fn the_same_seq_with_a_different_key_is_different() {
        let k = keys();
        let body = b"same-frame";
        let mut h = header(1);
        h.opus_len = body.len() as u16;
        let mut a = vec![0u8; HEADER_LEN + body.len() + TAG_LEN];
        seal_packet(&k.c2s, h, body, &mut a).unwrap();
        let mut b = vec![0u8; HEADER_LEN + body.len() + TAG_LEN];
        seal_packet(&k.s2c, h, body, &mut b).unwrap();
        assert_ne!(a[HEADER_LEN..], b[HEADER_LEN..]);
    }

    #[test]
    fn open_packet_parses_after_decrypting() {
        let k = keys();
        let mut pkt = build(header(9), b"hello-opus");
        let parsed = open_packet(&k.c2s, &mut pkt, Direction::ClientToServer).unwrap();
        assert_eq!(parsed.kind(), PacketKind::Voice);
        assert_eq!(parsed.payload(), b"hello-opus");
        assert_eq!(parsed.header().seq, 9);
        assert_eq!(parsed.aad(), parsed.raw().get(..HEADER_LEN).unwrap());
    }

    #[test]
    fn red_and_opus_are_both_covered() {
        let k = keys();
        let mut h = header(1);
        h.flags = flags::RED;
        h.red_len = 4;
        h.opus_len = 3;
        let body = [1u8, 2, 3, 0xAA, 0xBB, 0xCC, 0xDD];
        let mut pkt = seal_with(h, &body);
        let parsed = open_packet(&k.c2s, &mut pkt, Direction::ClientToServer).unwrap();
        assert_eq!(parsed.payload(), &[1, 2, 3]);
        assert_eq!(parsed.red(), &[0xAA, 0xBB, 0xCC, 0xDD]);
    }

    #[test]
    fn wire_len_agrees_with_seal_packet() {
        let mut h = header(1);
        h.opus_len = 12;
        let mut buf = vec![0u8; 64];
        let n = seal_packet(&keys().c2s, h, b"123456789012", &mut buf).unwrap();
        assert_eq!(n, wire_len(&h, true));
    }

    #[test]
    fn an_undersized_buffer_is_refused() {
        let mut buf = [0u8; 8];
        let mut h = header(1);
        h.opus_len = 12;
        assert!(matches!(
            seal_packet(&keys().c2s, h, b"123456789012", &mut buf),
            Err(CryptoError::BadLength { .. })
        ));
    }
}
