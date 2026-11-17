//! 客户端状态机与语音收发。

use client_core::{Client, ClientAction, ClientState, Incoming, RecvError};
use protocol::control::proto as pb;
use protocol::control::proto::control_message::Kind;
use protocol::{ControlMessage, PROTOCOL_VERSION, VoiceHeader, flags};
use transport::seal;

fn envelope(kind: Kind) -> ControlMessage {
    ControlMessage {
        protocol: PROTOCOL_VERSION,
        request_id: 0,
        kind: Some(kind),
    }
}

fn server_hello(session_id: u64, sender_id: u32, token: [u8; 16]) -> ControlMessage {
    envelope(Kind::ServerHello(pb::ServerHello {
        protocol: PROTOCOL_VERSION,
        server_version: "test".to_owned(),
        session_id,
        sender_id,
        voice_host: "127.0.0.1".to_owned(),
        voice_port: 40_000,
        voice_token: token.to_vec(),
        key_epoch: 0,
        channel_epoch: 0,
        server_time_ms: 1_000,
        caps: 0,
        limits: None,
    }))
}

fn auth_ok(user_id: u32) -> ControlMessage {
    envelope(Kind::AuthResult(pb::AuthResult {
        ok: true,
        user_id,
        ..Default::default()
    }))
}

/// 走到 Ready 的客户端。
fn ready_client() -> Client {
    let mut c = Client::new();
    c.hello();
    c.on_message(server_hello(7, 42, [9u8; 16]));
    c.set_exporter(&[5u8; 64], 0);
    let actions = c.on_message(auth_ok(1));
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, ClientAction::BindVoice { .. }))
    );
    assert_eq!(c.state(), ClientState::Ready);
    c
}

#[test]
fn handshake_drives_the_state_machine() {
    let mut c = Client::new();
    assert_eq!(c.state(), ClientState::Idle);

    let ClientAction::Send(hello) = c.hello() else {
        panic!("hello 应当返回一条要发的消息");
    };
    assert_eq!(hello.kind_name(), "client_hello");
    assert_eq!(c.state(), ClientState::HelloSent);

    c.on_message(server_hello(7, 42, [9u8; 16]));
    assert_eq!(c.state(), ClientState::AwaitAuth);
    assert_eq!(c.session_id(), 7);
    assert_eq!(c.sender_id(), 42);
    assert_eq!(c.voice_endpoint(), ("127.0.0.1", 40_000));

    c.set_exporter(&[5u8; 64], 0);
    let actions = c.on_message(auth_ok(1));
    assert_eq!(c.state(), ClientState::Ready);
    let Some(ClientAction::BindVoice { host, port, token }) = actions
        .iter()
        .find(|a| matches!(a, ClientAction::BindVoice { .. }))
    else {
        panic!("认证成功后必须去绑定语音面");
    };
    assert_eq!(host, "127.0.0.1");
    assert_eq!(*port, 40_000);
    assert_eq!(token, &[9u8; 16]);
}

#[test]
fn a_fatal_error_closes_the_client() {
    let mut c = ready_client();
    let actions = c.on_message(envelope(Kind::Error(pb::Error {
        code: pb::ErrorCode::AuthFailed as u32,
        message: "nope".to_owned(),
        fatal: true,
    })));
    assert_eq!(c.state(), ClientState::Closed);
    assert!(actions.iter().any(|a| matches!(a, ClientAction::Closed)));
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, ClientAction::ServerError { fatal: true, .. }))
    );
}

#[test]
fn a_failed_auth_result_closes_the_client() {
    let mut c = Client::new();
    c.hello();
    c.on_message(server_hello(7, 42, [9u8; 16]));
    let actions = c.on_message(envelope(Kind::AuthResult(pb::AuthResult {
        ok: false,
        error_code: pb::ErrorCode::AuthFailed as u32,
        message: "bad password".to_owned(),
        ..Default::default()
    })));
    assert_eq!(c.state(), ClientState::Closed);
    assert!(actions.iter().any(|a| matches!(a, ClientAction::Closed)));
}

#[test]
fn voice_encrypts_under_the_c2s_key() {
    let mut c = ready_client();
    let mut buf = vec![0u8; 256];
    let n = c.encode_voice(b"opus", &mut buf).unwrap();
    assert_eq!(n, protocol::HEADER_LEN + 4 + protocol::TAG_LEN);
    // 服务器侧用同一把 c2s 密钥能解开。
    let packet = transport::open_packet(
        &c.keys().c2s,
        &mut buf[..n],
        protocol::Direction::ClientToServer,
    )
    .unwrap();
    assert_eq!(packet.payload(), b"opus");
    assert_eq!(packet.header().sender_id, 42);
    assert_eq!(packet.header().seq, 0);
}

#[test]
fn sequence_numbers_advance_and_never_repeat() {
    let mut c = ready_client();
    let mut buf = vec![0u8; 256];
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..64 {
        let n = c.encode_voice(b"x", &mut buf).unwrap();
        let packet = transport::open_packet(
            &c.keys().c2s,
            &mut buf[..n],
            protocol::Direction::ClientToServer,
        )
        .unwrap();
        assert!(seen.insert(packet.header().seq), "seq 重复了");
    }
}

#[test]
fn receiving_a_voice_frame_works_and_replays_are_rejected() {
    let mut c = ready_client();
    let header = VoiceHeader {
        flags: 0,
        red_len: 0,
        channel_epoch: 0,
        sender_id: 77,
        seq: 3,
        timestamp: 1440,
        opus_len: 4,
    };
    let mut buf = vec![0u8; 64];
    let n = seal::seal_packet(&c.keys().s2c, header, b"opus", &mut buf).unwrap();

    let mut one = buf[..n].to_vec();
    match c.receive(&mut one).unwrap() {
        Incoming::Voice {
            sender_id,
            seq,
            timestamp,
            payload,
        } => {
            assert_eq!(sender_id, 77);
            assert_eq!(seq, 3);
            assert_eq!(timestamp, 1440);
            assert_eq!(payload, b"opus");
        }
        other => panic!("应当是语音帧，实际 {other:?}"),
    }

    let mut again = buf[..n].to_vec();
    assert_eq!(c.receive(&mut again).unwrap_err(), RecvError::Replayed);
}

#[test]
fn each_sender_has_its_own_replay_window() {
    let mut c = ready_client();
    let mut buf = vec![0u8; 64];
    for sender in [11u32, 22] {
        let header = VoiceHeader {
            flags: 0,
            red_len: 0,
            channel_epoch: 0,
            sender_id: sender,
            seq: 1,
            timestamp: 480,
            opus_len: 1,
        };
        let n = seal::seal_packet(&c.keys().s2c, header, b"x", &mut buf).unwrap();
        let mut one = buf[..n].to_vec();
        assert!(
            c.receive(&mut one).is_ok(),
            "sender {sender} 的第一个包该收下"
        );
    }
}

#[test]
fn a_packet_encrypted_with_the_wrong_key_is_rejected() {
    let mut c = ready_client();
    let header = VoiceHeader {
        flags: 0,
        red_len: 0,
        channel_epoch: 0,
        sender_id: 77,
        seq: 1,
        timestamp: 480,
        opus_len: 1,
    };
    let mut buf = vec![0u8; 64];
    let n = seal::seal_packet(&c.keys().c2s, header, b"x", &mut buf).unwrap();
    let mut one = buf[..n].to_vec();
    assert!(matches!(c.receive(&mut one), Err(RecvError::Crypto(_))));
}

#[test]
fn silence_and_probe_are_recognised() {
    let mut c = ready_client();

    let header = VoiceHeader {
        flags: flags::SILENCE,
        red_len: 0,
        channel_epoch: 0,
        sender_id: 88,
        seq: 1,
        timestamp: 0,
        opus_len: 0,
    };
    let mut buf = vec![0u8; 64];
    let n = seal::seal_packet(&c.keys().s2c, header, &[], &mut buf).unwrap();
    let mut one = buf[..n].to_vec();
    assert!(matches!(
        c.receive(&mut one),
        Ok(Incoming::Silence { sender_id: 88 })
    ));

    let header = VoiceHeader {
        flags: flags::PROBE,
        red_len: 0,
        channel_epoch: 0,
        sender_id: 0,
        seq: 1,
        timestamp: 0,
        opus_len: protocol::PROBE_PAYLOAD_LEN as u16,
    };
    let body = protocol::voice::probe_payload(0xDEAD_BEEF);
    let n = seal::seal_packet(&c.keys().s2c, header, &body, &mut buf).unwrap();
    let mut one = buf[..n].to_vec();
    assert!(matches!(
        c.receive(&mut one),
        Ok(Incoming::ProbeAck { ts_us: 0xDEAD_BEEF })
    ));
}

#[test]
fn a_probe_carries_the_current_channel_epoch() {
    // 服务器在分流到 Probe 分支**之前**就做了 epoch 检查：填 0 的打点包会被判「过期」
    // 静静丢掉（打点没有回音，看起来像网络不通）。这条测试把那个坑钉住。
    let mut c = ready_client();
    c.on_message(envelope(Kind::ChannelSync(pb::ChannelSync {
        channels: vec![pb::ChannelState {
            channel_id: 1,
            name: "大厅".to_owned(),
            channel_epoch: 5,
            ..Default::default()
        }],
        users: vec![pb::UserState {
            user_id: 1,
            sender_id: 42,
            name: "me".to_owned(),
            channel_id: 1,
            ..Default::default()
        }],
        self_channel_id: 1,
    })));
    assert_eq!(c.channel_epoch(), 5, "前提：已经在 epoch 5 的频道里");

    let mut buf = vec![0u8; 64];
    let n = c.encode_probe(0x1234, &mut buf).expect("加密打点包");
    let header = VoiceHeader::parse(&buf[..n]).expect("解析头");
    assert_eq!(header.channel_epoch, 5, "打点包必须带当前 epoch");
    assert_eq!(header.flags, flags::PROBE);
}

#[test]
fn the_roster_follows_the_server() {
    let mut c = ready_client();
    c.on_message(envelope(Kind::ChannelSync(pb::ChannelSync {
        channels: vec![pb::ChannelState {
            channel_id: 1,
            name: "大厅".to_owned(),
            channel_epoch: 5,
            ..Default::default()
        }],
        users: vec![pb::UserState {
            user_id: 1,
            sender_id: 42,
            name: "alice".to_owned(),
            channel_id: 1,
            ..Default::default()
        }],
        self_channel_id: 1,
    })));
    assert_eq!(c.roster().my_channel(), 1);
    assert_eq!(c.channel_epoch(), 5, "头部要填的 epoch 来自镜像");
    assert_eq!(c.roster().name_of_sender(42), Some("alice"));

    c.on_message(envelope(Kind::UserLeft(pb::UserLeft { user_id: 1 })));
    assert_eq!(c.roster().my_channel(), 0);
    assert_eq!(c.channel_epoch(), 0);
}

#[test]
fn a_channel_change_makes_the_client_use_the_new_epoch() {
    let mut c = ready_client();
    c.on_message(envelope(Kind::ChannelSync(pb::ChannelSync {
        channels: vec![
            pb::ChannelState {
                channel_id: 1,
                channel_epoch: 5,
                ..Default::default()
            },
            pb::ChannelState {
                channel_id: 2,
                channel_epoch: 9,
                ..Default::default()
            },
        ],
        users: vec![pb::UserState {
            user_id: 1,
            sender_id: 42,
            channel_id: 1,
            ..Default::default()
        }],
        self_channel_id: 1,
    })));
    assert_eq!(c.channel_epoch(), 5);

    c.on_message(envelope(Kind::UserUpdated(pb::UserUpdated {
        user: Some(pb::UserState {
            user_id: 1,
            sender_id: 42,
            channel_id: 2,
            ..Default::default()
        }),
    })));
    assert_eq!(c.channel_epoch(), 9);
}

#[test]
fn control_bytes_are_framed_and_drained() {
    let mut c = Client::new();
    c.hello();
    let mut framed = Vec::new();
    protocol::encode_frame(&server_hello(7, 42, [9u8; 16]), &mut framed).unwrap();
    protocol::encode_frame(
        &envelope(Kind::ChannelSync(pb::ChannelSync {
            channels: vec![pb::ChannelState {
                channel_id: 1,
                name: "大厅".to_owned(),
                channel_epoch: 3,
                ..Default::default()
            }],
            users: Vec::new(),
            self_channel_id: 1,
        })),
        &mut framed,
    )
    .unwrap();

    // 一字节一字节地喂：分帧器必须自己攒够一整帧才吐消息。
    let mut roster_changes = 0;
    for (i, byte) in framed.iter().enumerate() {
        let actions = c.on_bytes(&[*byte]).unwrap();
        if i + 1 < framed.len() {
            assert!(actions.is_empty(), "第 {} 字节就吐出了动作", i + 1);
        }
        roster_changes += actions
            .iter()
            .filter(|a| matches!(a, ClientAction::RosterChanged))
            .count();
    }
    assert_eq!(c.state(), ClientState::AwaitAuth);
    assert_eq!(roster_changes, 1, "只有 ChannelSync 会动镜像");
    assert_eq!(c.roster().channels().len(), 1);
}

#[test]
fn wire_and_audio_agree_on_frame_size() {
    // 线格式说 timestamp 是 48 kHz 采样计数；发帧的是音频侧。
    // 这两个常数一旦不一致，抖动缓冲的时序会整体偏移，而任何单侧测试都看不出来。
    assert_eq!(protocol::FRAME_SAMPLES as usize, audio_core::FRAME_SAMPLES);
    assert_eq!(protocol::SAMPLE_RATE, audio_core::SAMPLE_RATE);
    assert_eq!(
        protocol::FRAME_SAMPLES as u64 * 1000 / protocol::SAMPLE_RATE as u64,
        audio_core::FRAME_MS as u64
    );
}
