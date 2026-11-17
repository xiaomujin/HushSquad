//! 控制面时序与状态机。

mod common;

use common::{NOW, connect, join, join_channel, send_control, sent_messages, server};
use protocol::control::proto as pb;
use protocol::control::proto::control_message::Kind;
use server_core::{Action, Server, ServerConfig};

fn kind_of(msg: &protocol::ControlMessage) -> &'static str {
    msg.kind_name()
}

#[test]
fn handshake_returns_server_hello_then_auth_result_then_channel_sync() {
    let mut s = server();
    let id = s.open_session("alice", [1u8; 16]).expect("会话已满");

    let hello = protocol::ControlMessage {
        protocol: 1,
        request_id: 11,
        kind: Some(Kind::ClientHello(pb::ClientHello {
            protocol_min: 1,
            protocol_max: 1,
            client_version: "0.1.0".to_owned(),
            platform: "windows".to_owned(),
            device_id: "dev".to_owned(),
            caps: protocol::caps::RED | protocol::caps::WHISPER,
        })),
    };
    let actions = s.handle_control(id, hello, NOW);
    let msgs = sent_messages(&actions);
    assert_eq!(msgs.len(), 1);
    let Some(Kind::ServerHello(hello_out)) = &msgs[0].1.kind else {
        panic!("第一条必须是 ServerHello，实际 {}", kind_of(msgs[0].1));
    };
    assert_eq!(hello_out.protocol, 1);
    assert_eq!(hello_out.session_id, id);
    assert_eq!(hello_out.voice_token, vec![1u8; 16]);
    assert_eq!(hello_out.server_time_ms, NOW);
    // 能力位取交集：WHISPER 服务器不支持，RED 支持。
    assert_eq!(hello_out.caps, protocol::caps::RED);

    let auth = protocol::ControlMessage {
        protocol: 1,
        request_id: 12,
        kind: Some(Kind::Auth(pb::Auth {
            method: Some(pb::auth::Method::Password(pb::PasswordAuth {
                username: "alice".to_owned(),
                password: "alice-pw".to_owned(),
            })),
            client_nonce: String::new(),
        })),
    };
    let actions = s.handle_control(id, auth, NOW);
    let msgs = sent_messages(&actions);
    assert_eq!(msgs.len(), 2, "认证成功要先 AuthResult 再 ChannelSync");
    assert_eq!(kind_of(msgs[0].1), "auth_result");
    assert_eq!(kind_of(msgs[1].1), "channel_sync");
    let Some(Kind::AuthResult(result)) = &msgs[0].1.kind else {
        unreachable!()
    };
    assert!(result.ok);
    assert_eq!(result.user_id, 1);
    assert!(s.session(id).unwrap().is_ready());
}

#[test]
fn identity_comes_from_the_user_table_not_the_connection_order() {
    // bob 在用户表里是 id 2，但这里让他**先**连上来。若拿会话序号当身份，
    // 他会变成 user 1 —— 成员表里「名字与 id 对不上」，而客户端是按
    // `AuthResult.user_id` 在成员表里找自己的，于是会找到别人。
    // 真实症状：客户端日志「我在频道 0」然后一直等不到频道快照（10 秒超时）。
    let mut s = server();
    s.add_channel(0, "大厅");
    let bob = connect(&mut s, "bob", "bob-pw", 2);
    let alice = connect(&mut s, "alice", "alice-pw", 1);

    assert_eq!(
        s.session(bob.id).unwrap().user_id,
        2,
        "bob 的身份必须来自用户表，而不是「他第几个连上来」"
    );
    assert_eq!(s.session(alice.id).unwrap().user_id, 1);

    // 线格式上也要对：进频道时服务器推的 `UserJoined` 里，id 与名字必须自洽。
    let channel = *s.channels().keys().next().expect("有频道");
    let actions = join_channel(&mut s, alice.id, channel);
    let joined = sent_messages(&actions)
        .into_iter()
        .find_map(|(to, msg)| match &msg.kind {
            Some(Kind::UserJoined(joined)) if to == alice.id => joined.user.clone(),
            _ => None,
        })
        .expect("alice 进频道后应当收到自己的 UserJoined");
    assert_eq!(joined.user_id, 1);
    assert_eq!(joined.name, "alice");
}

#[test]
fn an_unsupported_version_closes_the_connection() {
    let mut s = server();
    let id = s.open_session("alice", [1u8; 16]).unwrap();
    let hello = protocol::ControlMessage {
        protocol: 1,
        request_id: 1,
        kind: Some(Kind::ClientHello(pb::ClientHello {
            protocol_min: 2,
            protocol_max: 3,
            ..Default::default()
        })),
    };
    let actions = s.handle_control(id, hello, NOW);
    assert!(actions.iter().any(|a| matches!(
        a,
        Action::Close { code, .. } if *code == pb::ErrorCode::UnsupportedVersion as u32
    )));
}

#[test]
fn a_bad_password_closes_the_connection() {
    let mut s = server();
    let id = s.open_session("alice", [1u8; 16]).unwrap();
    s.handle_control(
        id,
        protocol::ControlMessage {
            protocol: 1,
            request_id: 1,
            kind: Some(Kind::ClientHello(pb::ClientHello {
                protocol_min: 1,
                protocol_max: 1,
                ..Default::default()
            })),
        },
        NOW,
    );
    let actions = s.handle_control(
        id,
        protocol::ControlMessage {
            protocol: 1,
            request_id: 2,
            kind: Some(Kind::Auth(pb::Auth {
                method: Some(pb::auth::Method::Password(pb::PasswordAuth {
                    username: "alice".to_owned(),
                    password: "wrong".to_owned(),
                })),
                client_nonce: String::new(),
            })),
        },
        NOW,
    );
    let msgs = sent_messages(&actions);
    assert_eq!(kind_of(msgs[0].1), "auth_result");
    let Some(Kind::AuthResult(result)) = &msgs[0].1.kind else {
        unreachable!()
    };
    assert!(!result.ok);
    assert_eq!(result.error_code, pb::ErrorCode::AuthFailed as u32);
    assert!(actions.iter().any(|a| matches!(a, Action::Close { .. })));
    assert_eq!(s.metrics().auth_failures, 1);
}

#[test]
fn joining_a_channel_notifies_both_sides() {
    let mut s = server();
    let lobby = s.add_channel(0, "大厅");
    let a = join(&mut s, "alice", "alice-pw", 1);
    let b = join(&mut s, "bob", "bob-pw", 2);

    let actions = join_channel(&mut s, a.id, lobby);
    let joined: Vec<_> = sent_messages(&actions)
        .into_iter()
        .filter(|(_, m)| kind_of(m) == "user_joined")
        .collect();
    assert_eq!(joined.len(), 1, "第一个人进频道只通知他自己");
    assert_eq!(joined[0].0, a.id);

    let actions = join_channel(&mut s, b.id, lobby);
    let msgs = sent_messages(&actions);
    let targets: Vec<_> = msgs
        .iter()
        .filter(|(_, m)| kind_of(m) == "user_joined")
        .map(|(to, _)| *to)
        .collect();
    assert_eq!(targets.len(), 2, "bob 进频道要通知 alice 和 bob 自己");
    assert!(targets.contains(&a.id));
    assert!(targets.contains(&b.id));
    // 成员变了 epoch 也变了：新快照必须推给频道里的人。
    let updated: Vec<_> = msgs
        .iter()
        .filter(|(_, m)| kind_of(m) == "channel_updated")
        .collect();
    assert_eq!(updated.len(), 2, "频道快照要推给两个成员");
    for (_, msg) in updated {
        let Some(Kind::ChannelUpdated(update)) = &msg.kind else {
            unreachable!()
        };
        assert_eq!(
            update.channel.as_ref().unwrap().channel_epoch as u16,
            s.channel(lobby).unwrap().epoch
        );
    }
}

#[test]
fn leaving_a_channel_notifies_the_former_peers() {
    let mut s = server();
    let lobby = s.add_channel(0, "大厅");
    let a = join(&mut s, "alice", "alice-pw", 1);
    let b = join(&mut s, "bob", "bob-pw", 2);
    join_channel(&mut s, a.id, lobby);
    join_channel(&mut s, b.id, lobby);

    let actions = send_control(&mut s, a.id, Kind::LeaveChannel(pb::LeaveChannel {}), 5);
    let msgs = sent_messages(&actions);
    let targets: Vec<_> = msgs
        .iter()
        .filter(|(_, m)| kind_of(m) == "user_left")
        .map(|(to, _)| *to)
        .collect();
    assert!(targets.contains(&a.id));
    assert!(targets.contains(&b.id));
    assert!(s.session(a.id).unwrap().channel.is_none());
    assert_eq!(s.channel(lobby).unwrap().members.len(), 1);
}

#[test]
fn ping_is_answered_with_pong_echoing_the_timestamp() {
    let mut s = server();
    let a = join(&mut s, "alice", "alice-pw", 1);
    let actions = send_control(&mut s, a.id, Kind::Ping(pb::Ping { ts_us: 987_654 }), 9);
    let msgs = sent_messages(&actions);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].1.request_id, 9);
    let Some(Kind::Pong(pong)) = &msgs[0].1.kind else {
        panic!("必须是 Pong");
    };
    assert_eq!(pong.ts_us, 987_654);
}

#[test]
fn control_rate_limit_does_not_drop_the_connection() {
    let mut s = server();
    let a = join(&mut s, "alice", "alice-pw", 1);
    let limit = s.config().max_control_msgs_per_sec;
    // 已经用掉 2 条（hello + auth）。
    for _ in 0..limit {
        send_control(&mut s, a.id, Kind::Ping(pb::Ping { ts_us: 1 }), 0);
    }
    let actions = send_control(&mut s, a.id, Kind::Ping(pb::Ping { ts_us: 1 }), 0);
    let msgs = sent_messages(&actions);
    assert_eq!(kind_of(msgs[0].1), "error");
    let Some(Kind::Error(err)) = &msgs[0].1.kind else {
        unreachable!()
    };
    assert_eq!(err.code, pb::ErrorCode::RateLimited as u32);
    assert!(!err.fatal, "超速不该断线");
    assert!(s.metrics().control_rate_limited >= 1);
    // 下一秒恢复。
    let actions = s.handle_control(
        a.id,
        protocol::ControlMessage {
            protocol: 1,
            request_id: 0,
            kind: Some(Kind::Ping(pb::Ping { ts_us: 2 })),
        },
        NOW + 1000,
    );
    assert_eq!(kind_of(sent_messages(&actions)[0].1), "pong");
}

#[test]
fn channel_full_is_refused() {
    let mut s = server();
    let small = s.add_channel(0, "小房间");
    s.channel_mut(small).unwrap().max_users = 1;
    let a = join(&mut s, "alice", "alice-pw", 1);
    let b = join(&mut s, "bob", "bob-pw", 2);
    join_channel(&mut s, a.id, small);
    let actions = join_channel(&mut s, b.id, small);
    let msgs = sent_messages(&actions);
    assert_eq!(kind_of(msgs[0].1), "error");
    let Some(Kind::Error(err)) = &msgs[0].1.kind else {
        unreachable!()
    };
    assert_eq!(err.code, pb::ErrorCode::ChannelFull as u32);
    assert!(s.session(b.id).unwrap().channel.is_none());
}

#[test]
fn joining_a_missing_channel_is_refused() {
    let mut s = server();
    let a = join(&mut s, "alice", "alice-pw", 1);
    let actions = join_channel(&mut s, a.id, 999);
    let msgs = sent_messages(&actions);
    let Some(Kind::Error(err)) = &msgs[0].1.kind else {
        panic!("必须是 Error");
    };
    assert_eq!(err.code, pb::ErrorCode::ChannelNotFound as u32);
}

#[test]
fn self_mute_is_broadcast_to_the_channel() {
    let mut s = server();
    let lobby = s.add_channel(0, "大厅");
    let a = join(&mut s, "alice", "alice-pw", 1);
    let b = join(&mut s, "bob", "bob-pw", 2);
    join_channel(&mut s, a.id, lobby);
    join_channel(&mut s, b.id, lobby);

    let actions = send_control(
        &mut s,
        a.id,
        Kind::SetSelfState(pb::SetSelfState {
            self_mute: true,
            self_deaf: false,
        }),
        7,
    );
    let msgs = sent_messages(&actions);
    assert_eq!(msgs.len(), 2, "自己和同频道的人都要收到");
    for (_, msg) in &msgs {
        let Some(Kind::UserUpdated(update)) = &msg.kind else {
            panic!("必须是 UserUpdated");
        };
        assert!(update.user.as_ref().unwrap().self_mute);
    }
}

#[test]
fn unimplemented_messages_are_ignored_without_a_reply() {
    let mut s = server();
    let a = join(&mut s, "alice", "alice-pw", 1);
    let actions = send_control(
        &mut s,
        a.id,
        Kind::Whisper(pb::Whisper {
            target_user_ids: vec![2],
            channel_id: 0,
            stop: false,
        }),
        0,
    );
    assert!(actions.is_empty(), "未实现的分支必须静默忽略");
    assert_eq!(s.metrics().control_ignored, 1);
}

#[test]
fn a_closed_session_produces_no_actions() {
    let mut s = server();
    let a = join(&mut s, "alice", "alice-pw", 1);
    s.close_session(a.id);
    let actions = send_control(&mut s, a.id, Kind::Ping(pb::Ping { ts_us: 1 }), 0);
    assert!(actions.is_empty());
}

#[test]
fn every_channel_gets_a_distinct_epoch() {
    let mut s = Server::new(ServerConfig::default());
    let a = s.add_channel(0, "A");
    let b = s.add_channel(0, "B");
    assert_ne!(s.channel(a).unwrap().epoch, s.channel(b).unwrap().epoch);
}

#[test]
fn epochs_only_move_forward_on_membership_changes() {
    let mut s = server();
    let lobby = s.add_channel(0, "大厅");
    let a = join(&mut s, "alice", "alice-pw", 1);
    let before = s.channel(lobby).unwrap().epoch;
    join_channel(&mut s, a.id, lobby);
    let after = s.channel(lobby).unwrap().epoch;
    assert_ne!(before, after);
    // 只是发消息不动 epoch。
    send_control(&mut s, a.id, Kind::Ping(pb::Ping { ts_us: 1 }), 0);
    assert_eq!(s.channel(lobby).unwrap().epoch, after);
}
