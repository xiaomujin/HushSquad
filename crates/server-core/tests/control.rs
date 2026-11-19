//! 控制面时序与状态机。

mod common;

use common::{
    NOW, connect, deliver, epoch_of, join, join_channel, roles, send_control, sent_messages, server,
};
use protocol::control::proto as pb;
use protocol::control::proto::control_message::Kind;
use server_core::{Action, Server, ServerConfig};

fn kind_of(msg: &protocol::ControlMessage) -> &'static str {
    msg.kind_name()
}

#[test]
fn handshake_returns_server_hello_then_auth_result_then_channel_sync() {
    let mut s = server();
    let id = s
        .open_session("alice", [1u8; 16], common::NOW)
        .expect("会话已满");

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
    let id = s.open_session("alice", [1u8; 16], common::NOW).unwrap();
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
    let id = s.open_session("alice", [1u8; 16], common::NOW).unwrap();
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
#[test]
fn joining_a_group_container_is_refused() {
    // 分组（flags bit 3）只用来聚合子频道，不能加入。
    let mut s = server();
    let group = s.add_channel(0, "常用");
    s.channel_mut(group).unwrap().flags |= server_core::channel::FLAG_CONTAINER;
    let a = join(&mut s, "alice", "alice-pw", 1);

    let actions = join_channel(&mut s, a.id, group);
    let msgs = sent_messages(&actions);
    assert_eq!(kind_of(msgs[0].1), "error");
    let Some(Kind::Error(err)) = &msgs[0].1.kind else {
        unreachable!()
    };
    assert_eq!(err.code, pb::ErrorCode::PermissionDenied as u32);
    assert!(
        s.session(a.id).unwrap().channel.is_none(),
        "被拒绝之后不该留在任何频道里"
    );
}

#[test]
fn default_channel_cap_matches_the_mixer() {
    // 每频道默认 32 人，与 audio-core::mix::MAX_TRACKS 对齐：
    // 满员时每个客户端最多混 31 路远端，仍落在 32 个槽位以内。
    // 这条数字是跨 crate 的约定，改一边就必须改另一边。
    assert_eq!(ServerConfig::default().max_channel_users, 32);
}

// ---------------------------------------------------------------- 权限（ADR-0016）

/// 只取错误消息。
fn errors(actions: &[Action]) -> Vec<(u64, &pb::Error)> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::Send { msg, .. } => match &msg.kind {
                Some(Kind::Error(error)) => Some((msg.request_id, error)),
                _ => None,
            },
            Action::Close { .. } => None,
        })
        .collect()
}

/// 关掉了哪些会话。
fn closed(actions: &[Action]) -> Vec<(server_core::SessionId, u32)> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::Close { session, code, .. } => Some((*session, *code)),
            Action::Send { .. } => None,
        })
        .collect()
}

#[test]
fn a_member_cannot_kick_anyone_and_the_error_echoes_the_request_id() {
    let (mut s, _db) = roles();
    let bob = connect(&mut s, "bob", "bob-pw", 2);
    let carol = connect(&mut s, "carol", "carol-pw", 3);

    let actions = send_control(
        &mut s,
        carol.id,
        Kind::KickUser(pb::KickUser {
            user_id: 2,
            reason: String::new(),
        }),
        77,
    );

    assert!(closed(&actions).is_empty(), "成员不能关掉任何会话");
    let errs = errors(&actions);
    assert_eq!(errs.len(), 1, "只该回一条错误");
    assert_eq!(errs[0].0, 77, "错误必须带回同一个 request_id");
    assert_eq!(errs[0].1.code, pb::ErrorCode::PermissionDenied as u32);
    assert_eq!(s.metrics().permission_denied, 1, "越权要计数");
    assert!(s.session(bob.id).unwrap().is_ready(), "bob 不该被动过");
}

#[test]
fn a_moderator_can_kick_a_member_and_the_target_gets_a_fatal_error() {
    let (mut s, _db) = roles();
    let bob = connect(&mut s, "bob", "bob-pw", 2);
    let carol = connect(&mut s, "carol", "carol-pw", 3);

    let actions = send_control(
        &mut s,
        bob.id,
        Kind::KickUser(pb::KickUser {
            user_id: 3,
            reason: "太吵了".to_owned(),
        }),
        9,
    );

    assert_eq!(
        closed(&actions),
        vec![(carol.id, pb::ErrorCode::Kicked as u32)],
        "被踢的会话要被关掉"
    );
    let (_, msg) = sent_messages(&actions)
        .into_iter()
        .find(|(to, _)| *to == carol.id)
        .expect("被踢的人要收到一条消息");
    let Some(Kind::Error(error)) = &msg.kind else {
        panic!("被踢的人应当收到 Error");
    };
    assert!(error.fatal, "踢人必须是 fatal");
    assert!(error.message.contains("太吵了"), "原因要传给被踢的人");
    assert_eq!(s.metrics().kicks, 1);
    // 日志里**不**记原因：原因可能带着别人的隐私。
    let events = s.drain_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            server_core::Event::SessionKilled {
                reason: server_core::KillReason::Kicked,
                ..
            }
        )),
        "踢人要在事件里留一条"
    );
}

#[test]
fn an_admin_cannot_kick_the_owner() {
    let (mut s, _db) = roles();
    let alice = connect(&mut s, "alice", "alice-pw", 1);
    let bob = connect(&mut s, "bob", "bob-pw", 2);

    let actions = send_control(
        &mut s,
        bob.id,
        Kind::KickUser(pb::KickUser {
            user_id: 1,
            reason: String::new(),
        }),
        1,
    );

    assert!(closed(&actions).is_empty(), "管理员踢不了服主");
    assert_eq!(
        errors(&actions)[0].1.code,
        pb::ErrorCode::PermissionDenied as u32
    );
    assert!(s.session(alice.id).unwrap().is_ready());
}

#[test]
fn nobody_can_grant_permissions_they_do_not_have() {
    let (mut s, db) = roles();
    let bob = connect(&mut s, "bob", "bob-pw", 2);

    // 管理员想把 carol 提成服主：授予 ⊄ 自己 → 拒绝。
    let actions = send_control(
        &mut s,
        bob.id,
        Kind::SetUserPerms(pb::SetUserPerms {
            user_id: 3,
            perms: server_core::perms::OWNER_ROLE,
        }),
        5,
    );

    assert_eq!(
        errors(&actions)[0].1.code,
        pb::ErrorCode::PermissionDenied as u32
    );
    assert_eq!(
        db.perms_of(3),
        server_core::perms::MEMBER,
        "被拒的请求不能改库"
    );
}

#[test]
fn the_owner_bit_moves_atomically() {
    let (mut s, db) = roles();
    let alice = connect(&mut s, "alice", "alice-pw", 1);
    let bob = connect(&mut s, "bob", "bob-pw", 2);

    let actions = send_control(
        &mut s,
        alice.id,
        Kind::SetUserPerms(pb::SetUserPerms {
            user_id: 2,
            perms: server_core::perms::OWNER_ROLE,
        }),
        11,
    );

    assert!(errors(&actions).is_empty(), "服主转移不该报错");
    assert_eq!(
        db.perms_of(2),
        server_core::perms::OWNER_ROLE,
        "bob 应当成为服主"
    );
    assert_eq!(
        db.perms_of(1),
        server_core::perms::ADMIN_ROLE,
        "alice 应当降成管理员（服主位被清掉，其余位不动）"
    );
    // 两条 PermsChanged：一条给新服主，一条给旧服主。两人都还在线，所以都收到。
    let changed: Vec<u32> = sent_messages(&actions)
        .into_iter()
        .filter_map(|(_, msg)| match &msg.kind {
            Some(Kind::PermsChanged(p)) => Some(p.user_id),
            _ => None,
        })
        .collect();
    assert!(changed.contains(&2), "新服主要收到 PermsChanged");
    assert!(changed.contains(&1), "旧服主也要收到（他的权限变了）");
    // 在线会话的权限要**立刻**变：bob 不用重连就已经是服主了。
    assert_eq!(
        s.session(bob.id).unwrap().perms,
        server_core::perms::OWNER_ROLE,
        "在线的会话也要跟着改"
    );
    assert_eq!(
        s.session(alice.id).unwrap().perms,
        server_core::perms::ADMIN_ROLE,
        "旧服主的会话要降成管理员"
    );
}

#[test]
fn the_last_admin_cannot_be_demoted() {
    let (mut s, db) = roles();
    let alice = connect(&mut s, "alice", "alice-pw", 1);

    // 把唯一的管理员（自己，且是服主）降成成员 → 服务器会没人能管。
    let actions = send_control(
        &mut s,
        alice.id,
        Kind::SetUserPerms(pb::SetUserPerms {
            user_id: 1,
            perms: server_core::perms::MEMBER,
        }),
        12,
    );

    assert_eq!(errors(&actions)[0].1.code, pb::ErrorCode::Conflict as u32);
    assert_eq!(
        db.perms_of(1),
        server_core::perms::OWNER_ROLE,
        "被拒之后库不能变"
    );
}

#[test]
fn the_owner_cannot_be_demoted_without_handing_it_over() {
    let (mut s, db) = roles();
    let alice = connect(&mut s, "alice", "alice-pw", 1);

    let actions = send_control(
        &mut s,
        alice.id,
        Kind::SetUserPerms(pb::SetUserPerms {
            user_id: 1,
            perms: server_core::perms::ADMIN_ROLE,
        }),
        13,
    );

    assert_eq!(
        errors(&actions)[0].1.code,
        pb::ErrorCode::Conflict as u32,
        "把服主位去掉要走转移，不能直接清"
    );
    assert_eq!(db.perms_of(1), server_core::perms::OWNER_ROLE);
}

#[test]
fn list_users_needs_admin_and_marks_who_is_online() {
    let (mut s, _db) = roles();
    let alice = connect(&mut s, "alice", "alice-pw", 1);
    let carol = connect(&mut s, "carol", "carol-pw", 3);

    // 成员看不到账号列表。
    let denied = send_control(&mut s, carol.id, Kind::ListUsers(pb::ListUsers {}), 21);
    assert_eq!(
        errors(&denied)[0].1.code,
        pb::ErrorCode::PermissionDenied as u32
    );

    let actions = send_control(&mut s, alice.id, Kind::ListUsers(pb::ListUsers {}), 22);
    let (_, msg) = sent_messages(&actions)
        .into_iter()
        .find(|(to, _)| *to == alice.id)
        .expect("服主要收到账号列表");
    let Some(Kind::UserList(list)) = &msg.kind else {
        panic!("应当是 UserList");
    };
    assert_eq!(list.users.len(), 3, "三个账号都在（含离线的 bob）");
    let online = |user_id: u32| {
        list.users
            .iter()
            .find(|entry| entry.user_id == user_id)
            .map(|entry| entry.online)
    };
    assert_eq!(online(1), Some(true));
    assert_eq!(online(2), Some(false), "bob 没连上来");
    assert_eq!(online(3), Some(true));
}

#[test]
fn server_mute_from_an_admin_actually_stops_the_voice_plane() {
    // 这条是「权限不只是界面」的证明：管理员按下静音之后，被静音的人发的语音包
    // 在**语音面**被丢掉，而不是靠客户端自觉。
    let (mut s, _db) = roles();
    s.add_channel(0, "大厅");
    let alice = join(&mut s, "alice", "alice-pw", 1);
    let carol = join(&mut s, "carol", "carol-pw", 3);
    // 两个人都进同一个频道：语音面只转发给**同频道**的其他人，没人听就没有 targets。
    join_channel(&mut s, alice.id, 1);
    join_channel(&mut s, carol.id, 1);

    let epoch = epoch_of(&s, 1);
    let packet = carol.voice(1, epoch, &[1, 2, 3]);
    let (outcome, sink) = deliver(&mut s, carol.addr, &packet, NOW);
    assert!(
        matches!(outcome, server_core::Outcome::Forwarded { .. }),
        "静音之前应当转发，实际 {outcome:?}"
    );
    assert!(!sink.sent.is_empty());

    // 管理员（这里是服主）静音 carol。
    let actions = send_control(
        &mut s,
        alice.id,
        Kind::SetUserState(pb::SetUserState {
            user_id: 3,
            server_mute: Some(true),
            server_deaf: None,
        }),
        31,
    );
    assert!(errors(&actions).is_empty(), "服主可以静音成员");
    assert!(s.session(carol.id).unwrap().server_mute);

    let packet = carol.voice(2, epoch, &[4, 5, 6]);
    let (outcome, _) = deliver(&mut s, carol.addr, &packet, NOW);
    assert_eq!(
        outcome,
        server_core::Outcome::Dropped(server_core::DropReason::ServerMuted),
        "被静音的人发的语音包必须在服务器侧丢掉"
    );
}

#[test]
fn an_admin_cannot_mute_the_owner() {
    let (mut s, _db) = roles();
    s.add_channel(0, "大厅");
    let alice = join(&mut s, "alice", "alice-pw", 1);
    let bob = join(&mut s, "bob", "bob-pw", 2);
    join_channel(&mut s, alice.id, 1);
    join_channel(&mut s, bob.id, 1);

    let actions = send_control(
        &mut s,
        bob.id,
        Kind::SetUserState(pb::SetUserState {
            user_id: 1,
            server_mute: Some(true),
            server_deaf: None,
        }),
        32,
    );
    assert_eq!(
        errors(&actions)[0].1.code,
        pb::ErrorCode::PermissionDenied as u32
    );
    assert!(!s.session(alice.id).unwrap().server_mute, "服主不该被静音");
}

#[test]
fn moving_a_member_needs_move_anyone_and_broadcasts_the_move() {
    let (mut s, _db) = roles();
    s.add_channel(0, "大厅");
    s.add_channel(0, "小房间");
    let alice = join(&mut s, "alice", "alice-pw", 1);
    let carol = join(&mut s, "carol", "carol-pw", 3);
    join_channel(&mut s, carol.id, 1);

    // 成员挪不动别人。
    let denied = send_control(
        &mut s,
        carol.id,
        Kind::MoveUser(pb::MoveUser {
            user_id: 1,
            channel_id: 2,
        }),
        41,
    );
    assert_eq!(
        errors(&denied)[0].1.code,
        pb::ErrorCode::PermissionDenied as u32
    );

    // 服主可以把 carol 挪到 2 号频道。
    let actions = send_control(
        &mut s,
        alice.id,
        Kind::MoveUser(pb::MoveUser {
            user_id: 3,
            channel_id: 2,
        }),
        42,
    );
    assert!(errors(&actions).is_empty(), "服主可以移动成员");
    assert_eq!(
        s.session(carol.id).unwrap().channel,
        Some(2),
        "carol 应当已经在 2 号频道"
    );
    // 被移动的人自己也会收到 UserJoined（他的频道变了）。
    assert!(
        sent_messages(&actions)
            .into_iter()
            .any(|(to, msg)| to == carol.id && matches!(msg.kind, Some(Kind::UserJoined(_)))),
        "被移动的人要收到自己的 UserJoined"
    );
}

#[test]
fn moving_into_a_group_container_is_refused() {
    use server_core::channel::FLAG_CONTAINER;
    let (mut s, _db) = roles();
    s.add_channel(0, "大厅");
    let group = s.add_channel(0, "分组");
    s.channel_mut(group).unwrap().flags = FLAG_CONTAINER;
    let alice = join(&mut s, "alice", "alice-pw", 1);
    let carol = join(&mut s, "carol", "carol-pw", 3);
    join_channel(&mut s, carol.id, 1);

    let actions = send_control(
        &mut s,
        alice.id,
        Kind::MoveUser(pb::MoveUser {
            user_id: 3,
            channel_id: group,
        }),
        43,
    );
    assert_eq!(
        errors(&actions)[0].1.code,
        pb::ErrorCode::PermissionDenied as u32
    );
    assert_eq!(s.session(carol.id).unwrap().channel, Some(1));
}
