//! 语音面安全性质：跨频道、旧 epoch、错 tag、超速 —— 四条必须机器可证。

mod common;

use common::{NOW, decrypt, deliver, epoch_of, join, join_channel, server};
use server_core::{DropReason, Outcome};
use std::net::SocketAddr;

fn unbound_addr() -> SocketAddr {
    "127.0.0.1:59999".parse().unwrap()
}

#[test]
fn a_packet_reaches_every_other_member_of_the_channel() {
    let mut s = server();
    let lobby = s.add_channel(0, "大厅");
    let a = join(&mut s, "alice", "alice-pw", 1);
    let b = join(&mut s, "bob", "bob-pw", 2);
    let c = join(&mut s, "carol", "carol-pw", 3);
    join_channel(&mut s, a.id, lobby);
    join_channel(&mut s, b.id, lobby);
    join_channel(&mut s, c.id, lobby);

    let epoch = epoch_of(&s, lobby);
    let pkt = a.voice(1, epoch, b"opus-frame");
    let (outcome, sink) = deliver(&mut s, a.addr, &pkt, NOW);
    assert_eq!(outcome, Outcome::Forwarded { targets: 2 });
    assert_eq!(sink.sent.len(), 2);

    // 每个收包人拿到的都是同一帧明文，但用的是自己的 s2c 密钥。
    let mut payloads: Vec<Vec<u8>> = Vec::new();
    for (to, bytes) in &sink.sent {
        let key = if *to == b.addr { &b.s2c } else { &c.s2c };
        payloads.push(decrypt(key, bytes));
    }
    assert_eq!(
        payloads,
        vec![b"opus-frame".to_vec(), b"opus-frame".to_vec()]
    );

    // 两个收包人拿到的密文不同（不同密钥），但头里 sender_id 与 seq 原样保留。
    assert_ne!(sink.sent[0].1[20..], sink.sent[1].1[20..]);
    assert_eq!(sink.sent[0].1[6..10], a.sender_id.to_be_bytes());
    assert_eq!(sink.sent[0].1[10..14], 1u32.to_be_bytes());
}

#[test]
fn the_sender_does_not_receive_its_own_packet() {
    let mut s = server();
    let lobby = s.add_channel(0, "大厅");
    let a = join(&mut s, "alice", "alice-pw", 1);
    join_channel(&mut s, a.id, lobby);
    let pkt = a.voice(1, epoch_of(&s, lobby), b"x");
    let (outcome, sink) = deliver(&mut s, a.addr, &pkt, NOW);
    // 频道里只有自己：没有目标，不算错误，但要计数。
    assert_eq!(outcome, Outcome::Dropped(DropReason::NoTargets));
    assert!(sink.sent.is_empty());
}

#[test]
fn a_deaf_member_receives_nothing() {
    let mut s = server();
    let lobby = s.add_channel(0, "大厅");
    let a = join(&mut s, "alice", "alice-pw", 1);
    let b = join(&mut s, "bob", "bob-pw", 2);
    join_channel(&mut s, a.id, lobby);
    join_channel(&mut s, b.id, lobby);
    s.session_mut(b.id).unwrap().self_deaf = true;

    let pkt = a.voice(1, epoch_of(&s, lobby), b"x");
    let (outcome, sink) = deliver(&mut s, a.addr, &pkt, NOW);
    assert_eq!(outcome, Outcome::Dropped(DropReason::NoTargets));
    assert!(sink.sent.is_empty(), "闭麦的人不该收到任何语音");
}

#[test]
fn cross_channel_packets_are_rejected() {
    let mut s = server();
    let lobby = s.add_channel(0, "大厅");
    let gaming = s.add_channel(0, "游戏");
    let a = join(&mut s, "alice", "alice-pw", 1);
    let b = join(&mut s, "bob", "bob-pw", 2);
    join_channel(&mut s, a.id, lobby);
    join_channel(&mut s, b.id, lobby);

    let lobby_epoch = epoch_of(&s, lobby);
    let pkt = a.voice(1, lobby_epoch, b"in-lobby");

    // alice 换到另一个频道；在途的「大厅」包必须失效。
    join_channel(&mut s, a.id, gaming);
    assert_ne!(epoch_of(&s, gaming), lobby_epoch, "全局 epoch 必须单调");

    let (outcome, sink) = deliver(&mut s, a.addr, &pkt, NOW);
    assert_eq!(outcome, Outcome::Dropped(DropReason::WrongEpoch));
    assert!(sink.sent.is_empty());
    assert_eq!(s.metrics().dropped(DropReason::WrongEpoch), 1);
}

#[test]
fn stale_epoch_packets_are_rejected() {
    let mut s = server();
    let lobby = s.add_channel(0, "大厅");
    let a = join(&mut s, "alice", "alice-pw", 1);
    let b = join(&mut s, "bob", "bob-pw", 2);
    join_channel(&mut s, a.id, lobby);
    join_channel(&mut s, b.id, lobby);

    // 先按当前 epoch 造好包，但先不发。
    let pkt = a.voice(1, epoch_of(&s, lobby), b"stale");

    // 有人进出 → epoch 前进 → 这个包过期了。
    let c = join(&mut s, "carol", "carol-pw", 3);
    join_channel(&mut s, c.id, lobby);

    let (outcome, _) = deliver(&mut s, a.addr, &pkt, NOW);
    assert_eq!(outcome, Outcome::Dropped(DropReason::WrongEpoch));
}

#[test]
fn a_forged_tag_is_rejected() {
    let mut s = server();
    let lobby = s.add_channel(0, "大厅");
    let a = join(&mut s, "alice", "alice-pw", 1);
    let b = join(&mut s, "bob", "bob-pw", 2);
    join_channel(&mut s, a.id, lobby);
    join_channel(&mut s, b.id, lobby);

    let mut pkt = a.voice(1, epoch_of(&s, lobby), b"payload");
    let n = pkt.len();
    pkt[n - 1] ^= 0x01;
    let (outcome, sink) = deliver(&mut s, a.addr, &pkt, NOW);
    assert_eq!(outcome, Outcome::Dropped(DropReason::BadTag));
    assert!(sink.sent.is_empty());
}

/// 拿别人的 c2s 密钥签自己的包 —— tag 对不上。
#[test]
fn a_packet_signed_with_the_wrong_key_is_rejected() {
    let mut s = server();
    let lobby = s.add_channel(0, "大厅");
    let a = join(&mut s, "alice", "alice-pw", 1);
    let b = join(&mut s, "bob", "bob-pw", 2);
    join_channel(&mut s, a.id, lobby);
    join_channel(&mut s, b.id, lobby);

    // bob 用 alice 的密钥签，但从 bob 的地址发（服务器按地址找会话 → 用 bob 的密钥验）。
    let header = protocol::VoiceHeader {
        flags: 0,
        red_len: 0,
        channel_epoch: epoch_of(&s, lobby),
        sender_id: b.sender_id,
        seq: 1,
        timestamp: 480,
        opus_len: 1,
    };
    let mut buf = vec![0u8; protocol::HEADER_LEN + 1 + protocol::TAG_LEN];
    transport::seal_packet(&a.c2s, header, b"x", &mut buf).unwrap();
    let (outcome, _) = deliver(&mut s, b.addr, &buf, NOW);
    assert_eq!(outcome, Outcome::Dropped(DropReason::BadTag));
}

/// 用**自己的**密钥签，但把 sender_id 填成别人 —— 这是冒充，必须拦。
///
/// 这条是 P5-4 实装时发现的真漏洞：服务器原本只按源地址找会话，
/// 不校验头里的 `sender_id`，于是任何成员都能让服务器替自己转发一条
/// 「看起来是 alice 说的」语音。修法是在 AEAD 之后加一道 sender_id 比对。
#[test]
fn a_member_cannot_impersonate_another_sender_id() {
    let mut s = server();
    let lobby = s.add_channel(0, "大厅");
    let a = join(&mut s, "alice", "alice-pw", 1);
    let b = join(&mut s, "bob", "bob-pw", 2);
    join_channel(&mut s, a.id, lobby);
    join_channel(&mut s, b.id, lobby);

    let header = protocol::VoiceHeader {
        flags: 0,
        red_len: 0,
        channel_epoch: epoch_of(&s, lobby),
        sender_id: a.sender_id,
        seq: 1,
        timestamp: 480,
        opus_len: 1,
    };
    let mut buf = vec![0u8; protocol::HEADER_LEN + 1 + protocol::TAG_LEN];
    // bob 用自己的密钥签 —— tag 是合法的，只有 sender_id 是假的。
    transport::seal_packet(&b.c2s, header, b"x", &mut buf).unwrap();
    let (outcome, sink) = deliver(&mut s, b.addr, &buf, NOW);
    assert_eq!(outcome, Outcome::Dropped(DropReason::SenderMismatch));
    assert!(sink.sent.is_empty(), "冒充包不得转发");
    assert_eq!(s.metrics().dropped(DropReason::SenderMismatch), 1);
}

#[test]
fn repeated_bad_tags_kill_the_session() {
    let mut s = server();
    let lobby = s.add_channel(0, "大厅");
    let a = join(&mut s, "alice", "alice-pw", 1);
    join_channel(&mut s, a.id, lobby);

    let limit = s.config().auth_failure_limit;
    for seq in 0..limit {
        let mut pkt = a.voice(seq, epoch_of(&s, lobby), b"x");
        let n = pkt.len();
        pkt[n - 1] ^= 0xFF;
        let (outcome, _) = deliver(&mut s, a.addr, &pkt, NOW);
        assert_eq!(outcome, Outcome::Dropped(DropReason::BadTag), "第 {seq} 次");
    }
    assert!(s.session(a.id).is_none(), "连续伪造应当在 {limit} 次后断开");
    assert_eq!(s.metrics().sessions_killed_bad_tag, 1);
    // 会话没了，后续包一律按未绑定处理。
    let pkt = a.voice(999, epoch_of(&s, lobby), b"x");
    let (outcome, _) = deliver(&mut s, a.addr, &pkt, NOW);
    assert_eq!(outcome, Outcome::Dropped(DropReason::Unbound));
}

#[test]
fn a_good_packet_resets_the_failure_counter() {
    let mut s = server();
    let lobby = s.add_channel(0, "大厅");
    let a = join(&mut s, "alice", "alice-pw", 1);
    let b = join(&mut s, "bob", "bob-pw", 2);
    join_channel(&mut s, a.id, lobby);
    join_channel(&mut s, b.id, lobby);
    let epoch = epoch_of(&s, lobby);

    let limit = s.config().auth_failure_limit;
    for seq in 0..(limit - 1) {
        let mut pkt = a.voice(seq, epoch, b"x");
        let n = pkt.len();
        pkt[n - 1] ^= 0xFF;
        deliver(&mut s, a.addr, &pkt, NOW);
    }
    // 一个合法包把计数清零。
    let (outcome, _) = deliver(&mut s, a.addr, &a.voice(limit - 1, epoch, b"ok"), NOW);
    assert_eq!(outcome, Outcome::Forwarded { targets: 1 });
    // 再错 limit-1 次也不该被踢。
    for seq in 0..(limit - 1) {
        let mut pkt = a.voice(1000 + seq, epoch, b"x");
        let n = pkt.len();
        pkt[n - 1] ^= 0xFF;
        deliver(&mut s, a.addr, &pkt, NOW);
    }
    assert!(s.session(a.id).is_some(), "中途成功过就不该被断开");
}

#[test]
fn a_replayed_packet_is_dropped() {
    let mut s = server();
    let lobby = s.add_channel(0, "大厅");
    let a = join(&mut s, "alice", "alice-pw", 1);
    let b = join(&mut s, "bob", "bob-pw", 2);
    join_channel(&mut s, a.id, lobby);
    join_channel(&mut s, b.id, lobby);
    let epoch = epoch_of(&s, lobby);

    let pkt = a.voice(7, epoch, b"once");
    assert_eq!(
        deliver(&mut s, a.addr, &pkt, NOW).0,
        Outcome::Forwarded { targets: 1 }
    );
    let (outcome, sink) = deliver(&mut s, a.addr, &pkt, NOW);
    assert_eq!(outcome, Outcome::Dropped(DropReason::Replay));
    assert!(sink.sent.is_empty(), "重放包不得再转发一次");
}

#[test]
fn overspeed_packets_are_dropped_and_counted() {
    let mut s = server();
    let lobby = s.add_channel(0, "大厅");
    let a = join(&mut s, "alice", "alice-pw", 1);
    let b = join(&mut s, "bob", "bob-pw", 2);
    join_channel(&mut s, a.id, lobby);
    join_channel(&mut s, b.id, lobby);
    let epoch = epoch_of(&s, lobby);

    let limit = s.config().max_voice_pps;
    for seq in 0..limit {
        let (outcome, _) = deliver(&mut s, a.addr, &a.voice(seq, epoch, b"x"), NOW);
        assert_eq!(outcome, Outcome::Forwarded { targets: 1 }, "第 {seq} 个包");
    }
    let (outcome, sink) = deliver(&mut s, a.addr, &a.voice(limit, epoch, b"x"), NOW);
    assert_eq!(outcome, Outcome::Dropped(DropReason::RateLimited));
    assert!(sink.sent.is_empty());
    assert_eq!(s.metrics().dropped(DropReason::RateLimited), 1);

    // 下一秒恢复。
    let (outcome, _) = deliver(&mut s, a.addr, &a.voice(limit + 1, epoch, b"x"), NOW + 1000);
    assert_eq!(outcome, Outcome::Forwarded { targets: 1 });
}

#[test]
fn an_unbound_source_is_dropped() {
    let mut s = server();
    let lobby = s.add_channel(0, "大厅");
    let a = join(&mut s, "alice", "alice-pw", 1);
    join_channel(&mut s, a.id, lobby);

    let pkt = a.voice(1, epoch_of(&s, lobby), b"x");
    let (outcome, sink) = deliver(&mut s, unbound_addr(), &pkt, NOW);
    assert_eq!(outcome, Outcome::Dropped(DropReason::Unbound));
    assert!(sink.sent.is_empty());
}

#[test]
fn bind_moves_the_address_on_a_network_switch() {
    let mut s = server();
    let lobby = s.add_channel(0, "大厅");
    let a = join(&mut s, "alice", "alice-pw", 1);
    let b = join(&mut s, "bob", "bob-pw", 2);
    join_channel(&mut s, a.id, lobby);
    join_channel(&mut s, b.id, lobby);

    // 换网络：同一个 token 从新地址重绑。
    let new_addr: SocketAddr = "127.0.0.1:41001".parse().unwrap();
    let mut buf = [0u8; 64];
    let n = protocol::voice::encode_bind(&a.voice_token, &mut buf);
    let (outcome, sink) = deliver(&mut s, new_addr, &buf[..n], NOW);
    assert_eq!(outcome, Outcome::Replied);
    assert_eq!(sink.sent.len(), 1);
    assert_eq!(s.metrics().rebinds, 1);
    assert_eq!(s.session(a.id).unwrap().addr, Some(new_addr));

    // 老地址立刻失效。
    let epoch = epoch_of(&s, lobby);
    let stale = a.voice(1, epoch, b"x");
    let (outcome, _) = deliver(&mut s, a.addr, &stale, NOW);
    assert_eq!(outcome, Outcome::Dropped(DropReason::Unbound));
    // 新地址可用。
    let fresh = a.voice(2, epoch, b"x");
    let (outcome, _) = deliver(&mut s, new_addr, &fresh, NOW);
    assert_eq!(outcome, Outcome::Forwarded { targets: 1 });
}

#[test]
fn an_unknown_bind_token_gets_no_reply() {
    let mut s = server();
    let _ = join(&mut s, "alice", "alice-pw", 1);

    let mut buf = [0u8; 64];
    let n = protocol::voice::encode_bind(&[0xEE; 16], &mut buf);
    let (outcome, sink) = deliver(&mut s, unbound_addr(), &buf[..n], NOW);
    assert_eq!(outcome, Outcome::Dropped(DropReason::Unbound));
    assert!(
        sink.sent.is_empty(),
        "对未知 token 回应就等于把自己变成放大器"
    );
}

#[test]
fn probe_is_echoed_without_entering_the_jitter_buffer() {
    let mut s = server();
    let lobby = s.add_channel(0, "大厅");
    let a = join(&mut s, "alice", "alice-pw", 1);
    join_channel(&mut s, a.id, lobby);

    let pkt = a.probe(1, epoch_of(&s, lobby), 0x0102_0304_0506_0708);
    let (outcome, sink) = deliver(&mut s, a.addr, &pkt, NOW);
    assert_eq!(outcome, Outcome::Replied);
    assert_eq!(sink.sent.len(), 1);
    let body = decrypt(&a.s2c, &sink.sent[0].1);
    assert_eq!(
        protocol::voice::parse_probe_payload(&body).unwrap(),
        0x0102_0304_0506_0708
    );
    assert_eq!(s.metrics().probes, 1);
}

#[test]
fn server_generated_packets_never_reuse_a_nonce() {
    // 重绑定两次 → 两条 BindAck。它们的 sender_id 都是 0，所以 seq 必须不同，
    // 否则同一个 (key, nonce) 会加密两条不同的消息 —— ChaCha20-Poly1305 下是灾难。
    let mut s = server();
    let a = join(&mut s, "alice", "alice-pw", 1);

    let mut seqs = Vec::new();
    for port in [41010u16, 41011] {
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut buf = [0u8; 64];
        let n = protocol::voice::encode_bind(&a.voice_token, &mut buf);
        let (_, sink) = deliver(&mut s, addr, &buf[..n], NOW);
        assert_eq!(sink.sent.len(), 1);
        let bytes = &sink.sent[0].1;
        assert_eq!(
            &bytes[6..10],
            &0u32.to_be_bytes(),
            "服务器包的 sender_id 必须是 0"
        );
        seqs.push(u32::from_be_bytes([
            bytes[10], bytes[11], bytes[12], bytes[13],
        ]));
    }
    assert_ne!(seqs[0], seqs[1], "两条 BindAck 的 seq 撞了 → nonce 复用");
}

#[test]
fn bind_ack_carries_the_sender_id_and_server_time() {
    let mut s = server();
    let a = join(&mut s, "alice", "alice-pw", 1);
    let mut buf = [0u8; 64];
    let n = protocol::voice::encode_bind(&a.voice_token, &mut buf);
    let (_, sink) = deliver(&mut s, a.addr, &buf[..n], NOW);
    let body = decrypt(&a.s2c, &sink.sent[0].1);
    let (server_time, sender_id) = protocol::voice::parse_bind_ack_payload(&body).unwrap();
    assert_eq!(server_time, NOW);
    assert_eq!(sender_id, a.sender_id);
}

#[test]
fn a_malformed_packet_is_dropped_before_anything_expensive() {
    let mut s = server();
    let a = join(&mut s, "alice", "alice-pw", 1);
    let (outcome, _) = deliver(&mut s, a.addr, b"\x48\x01", NOW);
    assert_eq!(outcome, Outcome::Dropped(DropReason::Malformed));
    // 只有 3 个字节的头。
    let (outcome, _) = deliver(&mut s, a.addr, &[0x48, 0x01, 0x00], NOW);
    assert_eq!(outcome, Outcome::Dropped(DropReason::Malformed));
    assert_eq!(s.metrics().dropped(DropReason::Malformed), 2);
}

#[test]
fn a_server_muted_member_cannot_be_heard() {
    let mut s = server();
    let lobby = s.add_channel(0, "大厅");
    let a = join(&mut s, "alice", "alice-pw", 1);
    let b = join(&mut s, "bob", "bob-pw", 2);
    join_channel(&mut s, a.id, lobby);
    join_channel(&mut s, b.id, lobby);
    s.session_mut(a.id).unwrap().server_mute = true;

    let pkt = a.voice(1, epoch_of(&s, lobby), b"x");
    let (outcome, sink) = deliver(&mut s, a.addr, &pkt, NOW);
    assert_eq!(outcome, Outcome::Dropped(DropReason::ServerMuted));
    assert!(sink.sent.is_empty());
}
