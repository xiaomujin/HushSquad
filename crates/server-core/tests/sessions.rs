//! 僵尸会话回收（B7）。
//!
//! 这一条守的是**半开连接**：TCP 的 EOF 只覆盖「对端正常关」和「RST 送到」两种情况。
//! 进程被强杀而 RST 丢在路上的、网线被拔的、NAT 表被清的，连接会一直是 ESTABLISHED，
//! 服务器什么都不做就会一直留着那条会话 —— 同一台设备重连时撞上它，症状是「握手成功、
//! 进频道成功，然后一个字节都收不到」（C-54）。

mod common;

use common::{NOW, join};
use protocol::ControlMessage;
use protocol::control::proto as pb;
use protocol::control::proto::control_message::Kind;
use server_core::{ServerConfig, VecSink};

/// 下发给客户端的超时就是服务器自己执行的阈值 —— 承诺与执行必须是同一个数。
fn timeout_ms() -> u64 {
    u64::from(ServerConfig::default().limits.ping_timeout_ms)
}

#[test]
fn a_silent_session_is_reaped_just_after_the_ping_timeout() {
    let mut server = common::server();
    let joined = join(&mut server, "alice", "alice-pw", 1);

    // 刚好到点还不算超时：判据是「超过」，不是「达到」。
    assert_eq!(server.sweep_idle(NOW + timeout_ms()), 0, "到点不该收");
    assert_eq!(server.metrics().sessions_closed, 0);

    assert_eq!(
        server.sweep_idle(NOW + timeout_ms() + 1),
        1,
        "过 1 ms 就该收"
    );
    assert_eq!(server.metrics().sessions_closed, 1);
    assert_eq!(server.metrics().sessions_reaped, 1);
    assert_eq!(
        server.sweep_idle(NOW + timeout_ms() + 2_000),
        0,
        "收过之后不该再数一次"
    );
    let _ = joined;
}

#[test]
fn a_control_message_keeps_the_session_alive() {
    let mut server = common::server();
    let joined = join(&mut server, "alice", "alice-pw", 1);

    // 快到点时发一条心跳：回收计时重新开始。
    let late = NOW + timeout_ms();
    let msg = ControlMessage {
        protocol: 1,
        request_id: 1,
        kind: Some(Kind::Ping(pb::Ping { ts_us: 0 })),
    };
    server.handle_control(joined.id, msg, late);
    assert_eq!(server.sweep_idle(late + 1), 0, "刚说过话，不该收");
    assert_eq!(
        server.sweep_idle(late + timeout_ms() + 1),
        1,
        "又静了，该收"
    );
}

#[test]
fn a_voice_packet_keeps_the_session_alive() {
    // 只听不说的人也会发包（保活标记 / 打点），但反过来：只发语音、不发控制消息的人
    // 不能被当成僵尸。这条钉住「语音面也算活着」。
    let mut server = common::server();
    let joined = join(&mut server, "alice", "alice-pw", 1);
    let late = NOW + timeout_ms();

    let mut sink = VecSink::new();
    let mut packet = joined.voice(1, 1, &[0u8; 20]);
    server.handle_datagram(joined.addr, &mut packet, late, &mut sink);

    assert_eq!(server.sweep_idle(late + 1), 0, "刚发过语音，不该收");
    assert_eq!(server.sweep_idle(late + timeout_ms() + 1), 1, "静了，该收");
}

#[test]
fn reaping_one_session_leaves_the_others_alone() {
    let mut server = common::server();
    let alice = join(&mut server, "alice", "alice-pw", 1);
    let _bob = join(&mut server, "bob", "bob-pw", 2);

    // 只有 alice 说话：bob 从 NOW 之后就再没动静。
    let late = NOW + timeout_ms();
    let msg = ControlMessage {
        protocol: 1,
        request_id: 1,
        kind: Some(Kind::Ping(pb::Ping { ts_us: 0 })),
    };
    server.handle_control(alice.id, msg, late);
    assert_eq!(server.sweep_idle(late + 1), 1, "只该收掉 bob");
    assert_eq!(server.metrics().sessions_closed, 1);
}
