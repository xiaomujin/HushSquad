//! P5 传输面的单包开销（`testkit p5-bench`）。
//!
//! 这里量的是**每包固定开销**，不是网络时延：AEAD 加解密、fan-out 拷贝、窗口判定。
//! 抖动缓冲与漂移补偿的耗时已经在 §2 里量过，不重复。
//!
//! 数字只在 `--release` 下有参考价值，报告里会标 profile。

use std::net::SocketAddr;
use std::time::Instant;

use protocol::control::proto as pb;
use protocol::control::proto::control_message::Kind;
use protocol::{ControlMessage, HEADER_LEN, PROTOCOL_VERSION, TAG_LEN, VoiceHeader};
use serde::Serialize;
use server_core::{Server, ServerConfig, UserDb, VecSink};
use transport::{ReplayWindow, VoiceKeys, derive_voice_keys, seal};

/// 一份报告。
#[derive(Debug, Serialize)]
pub struct Report {
    /// 构建配置。
    pub profile: &'static str,
    /// 每项迭代次数。
    pub iters: u32,
    /// 每项取几轮的中位数。
    pub rounds: u32,
    /// 语音包载荷字节数。
    pub payload_bytes: usize,
    /// 一个方向密钥（32 B）的 HKDF 派生耗时（ns）。
    pub hkdf_ns: f64,
    /// 单包 AEAD 封装（ns）。
    pub seal_ns: f64,
    /// 单包 AEAD 解封 + 校验（ns）。
    pub open_ns: f64,
    /// 单包 20 B 头解析 + 长度自洽校验（ns）。
    pub header_parse_ns: f64,
    /// 滑动窗口判定一次（ns）。
    pub replay_ns: f64,
    /// 服务器 fan-out 到 N 个听众的单包总耗时（µs）。
    pub fanout_us: f64,
    /// fan-out 的听众数。
    pub fanout_peers: usize,
    /// 每个包实际产生的 fan-out 副本数。**这是自检**：应当等于 `fanout_peers`；
    /// 明显更小说明基准量的是丢弃路径而不是转发路径。
    pub fanout_copies_per_packet: f64,
}

fn now_ns(start: Instant, iters: u32) -> f64 {
    start.elapsed().as_nanos() as f64 / f64::from(iters)
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).expect("无 NaN"));
    values[values.len() / 2]
}

fn keys() -> VoiceKeys {
    let mut exporter = [0u8; 64];
    for (i, b) in exporter.iter_mut().enumerate() {
        *b = i as u8;
    }
    derive_voice_keys(&exporter, 1, 0)
}

fn header(payload_len: usize) -> VoiceHeader {
    VoiceHeader {
        flags: 0,
        red_len: 0,
        channel_epoch: 1,
        sender_id: 42,
        seq: 7,
        timestamp: 3360,
        opus_len: payload_len as u16,
    }
}

/// 造一个「1 个说话人 + N 个听众」的服务器，全部走真握手。
fn fanout_server(peers: usize) -> (Server, SocketAddr, VoiceKeys, u32) {
    let mut db = UserDb::new();
    for i in 0..=peers {
        db.add_user((i + 1) as u32, &format!("u{i}"), "pw", 1)
            .expect("建用户");
    }
    let mut server = Server::new(ServerConfig {
        max_sessions: peers + 4,
        ..ServerConfig::default()
    });
    server.set_authenticator(Box::new(db));
    let channel = server.add_channel(0, "bench");

    let mut speaker_keys = VoiceKeys::zeroed();
    let mut speaker_id = 0u32;
    for i in 0..=peers {
        let seed = (i + 1) as u8;
        let exporter = [seed; 64];
        let token = [seed; 16];
        let id = server
            .open_session(format!("u{i}"), &exporter, token)
            .expect("会话已满");
        let hello = ControlMessage {
            protocol: PROTOCOL_VERSION,
            request_id: 1,
            kind: Some(Kind::ClientHello(pb::ClientHello {
                protocol_min: 1,
                protocol_max: 1,
                ..Default::default()
            })),
        };
        server.handle_control(id, hello, 0);
        let auth = ControlMessage {
            protocol: PROTOCOL_VERSION,
            request_id: 2,
            kind: Some(Kind::Auth(pb::Auth {
                method: Some(pb::auth::Method::Password(pb::PasswordAuth {
                    username: format!("u{i}"),
                    password: "pw".to_owned(),
                })),
                client_nonce: String::new(),
            })),
        };
        server.handle_control(id, auth, 0);
        let join = ControlMessage {
            protocol: PROTOCOL_VERSION,
            request_id: 3,
            kind: Some(Kind::JoinChannel(pb::JoinChannel {
                channel_id: channel,
                password: String::new(),
            })),
        };
        server.handle_control(id, join, 0);

        let addr: SocketAddr = format!("127.0.0.1:{}", 41_000 + i as u16)
            .parse()
            .expect("地址");
        let mut bind = [0u8; 64];
        let n = protocol::voice::encode_bind(&token, &mut bind);
        let mut sink = VecSink::new();
        server.handle_datagram(addr, &mut bind[..n], 0, &mut sink);

        if i == 0 {
            let session = server.session(id).expect("会话存在");
            speaker_keys = session.keys.clone();
            speaker_id = session.sender_id;
        }
    }
    let speaker: SocketAddr = "127.0.0.1:41000".parse().expect("地址");
    (server, speaker, speaker_keys, speaker_id)
}

/// 跑一遍基准，返回报告。
pub fn run(iters: u32, rounds: u32, payload: usize, peers: usize) -> Report {
    let keys = keys();
    let hdr = header(payload);
    let total = HEADER_LEN + payload + TAG_LEN;
    let body = vec![0x5Au8; payload];

    let mut hkdf = Vec::new();
    for _ in 0..rounds {
        let start = Instant::now();
        for _ in 0..iters {
            let _ = derive_voice_keys(&[7u8; 64], 1, 0);
        }
        hkdf.push(now_ns(start, iters));
    }

    let mut seal_ns = Vec::new();
    let mut open_ns = Vec::new();
    let mut buf = vec![0u8; total];
    for _ in 0..rounds {
        let start = Instant::now();
        for _ in 0..iters {
            seal::seal_packet(&keys.c2s, hdr, &body, &mut buf).expect("封装");
        }
        seal_ns.push(now_ns(start, iters));

        let start = Instant::now();
        for _ in 0..iters {
            seal::seal_packet(&keys.c2s, hdr, &body, &mut buf).expect("封装");
            seal::open(&keys.c2s, &mut buf).expect("解封");
        }
        // 这一轮包含一次封装，减掉它得到纯解封耗时。
        open_ns.push(now_ns(start, iters) - seal_ns[seal_ns.len() - 1]);
    }

    let mut parse_ns = Vec::new();
    for _ in 0..rounds {
        let start = Instant::now();
        for _ in 0..iters {
            let h = VoiceHeader::parse(&buf).expect("解析");
            assert_eq!(h.body_len(), payload);
        }
        parse_ns.push(now_ns(start, iters));
    }

    let mut replay_ns = Vec::new();
    for _ in 0..rounds {
        let mut window = ReplayWindow::new();
        let start = Instant::now();
        for i in 0..iters {
            window.accept(i);
        }
        replay_ns.push(now_ns(start, iters));
    }

    let (mut server, speaker, speaker_keys, speaker_id) = fanout_server(peers);
    // 头里的 sender_id 必须与会话的一致，否则服务器按「冒充」丢弃（这正是 P5-4 修的漏洞）。
    let hdr = VoiceHeader {
        sender_id: speaker_id,
        ..hdr
    };
    let epoch = server.channels().values().next().expect("有频道").epoch;
    let mut out = VecSink::new();
    let mut packet = vec![0u8; total];

    // 先确认这确实会 fan-out（而不是被限速器拦下）—— 否则量到的是丢弃路径。
    let probe = VoiceHeader {
        channel_epoch: epoch,
        seq: 0,
        ..hdr
    };
    seal::seal_packet(&speaker_keys.c2s, probe, &body, &mut packet).expect("封装");
    let outcome = server.handle_datagram(speaker, &mut packet, 0, &mut out);
    assert_eq!(
        outcome,
        server_core::Outcome::Forwarded { targets: peers },
        "fan-out 基准没走到转发路径"
    );
    assert_eq!(out.sent.len(), peers);
    out.clear();

    let copies_before = server.metrics().fanout_copies;
    let mut fanout_us = Vec::new();
    // 虚拟时钟必须**跨轮单调**。每轮都从 0 开始的话，第二轮的 now 会落到上一轮
    // 限速窗口的起点之前，`saturating_sub` 得到 0，窗口永不重置，于是除第一轮外
    // 全部被判超速 —— 量到的就不是 fan-out，而是丢弃路径。
    let mut clock_ms = 0u64;
    let mut seq = 1u32;
    for _ in 0..rounds {
        let start = Instant::now();
        for _ in 0..iters {
            let h = VoiceHeader {
                channel_epoch: epoch,
                seq,
                ..hdr
            };
            seq = seq.wrapping_add(1);
            seal::seal_packet(&speaker_keys.c2s, h, &body, &mut packet).expect("封装");
            // 每个包推进 7 ms 虚拟时间：1000/7 ≈ 142 pps < 150 pps 的限速，
            // 这样量到的是 fan-out，而不是「超速丢弃」。
            clock_ms += 7;
            server.handle_datagram(speaker, &mut packet, clock_ms, &mut out);
            out.clear();
        }
        fanout_us.push(start.elapsed().as_nanos() as f64 / f64::from(iters) / 1000.0);
    }
    let copies = server.metrics().fanout_copies - copies_before;
    let fanout_copies_per_packet = copies as f64 / f64::from(iters.saturating_mul(rounds));

    Report {
        profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        iters,
        rounds,
        payload_bytes: payload,
        hkdf_ns: median(hkdf),
        seal_ns: median(seal_ns),
        open_ns: median(open_ns),
        header_parse_ns: median(parse_ns),
        replay_ns: median(replay_ns),
        fanout_us: median(fanout_us),
        fanout_peers: peers,
        fanout_copies_per_packet,
    }
}

/// CLI 入口。
pub fn main(args: &[String]) -> Result<(), String> {
    let mut iters = 20_000u32;
    let mut rounds = 7u32;
    let mut payload = 100usize;
    let mut peers = 7usize;
    let mut out_path: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--iters" => {
                iters = args
                    .get(i + 1)
                    .ok_or("--iters 缺少值")?
                    .parse()
                    .map_err(|_| "--iters 需要正整数")?;
                i += 2;
            }
            "--rounds" => {
                rounds = args
                    .get(i + 1)
                    .ok_or("--rounds 缺少值")?
                    .parse()
                    .map_err(|_| "--rounds 需要正整数")?;
                i += 2;
            }
            "--payload" => {
                payload = args
                    .get(i + 1)
                    .ok_or("--payload 缺少值")?
                    .parse()
                    .map_err(|_| "--payload 需要正整数")?;
                i += 2;
            }
            "--peers" => {
                peers = args
                    .get(i + 1)
                    .ok_or("--peers 缺少值")?
                    .parse()
                    .map_err(|_| "--peers 需要正整数")?;
                i += 2;
            }
            "--out" => {
                out_path = Some(args.get(i + 1).ok_or("--out 缺少值")?.clone());
                i += 2;
            }
            other => return Err(format!("未知参数 {other:?}")),
        }
    }

    let report = run(iters, rounds, payload, peers);
    let json = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    if let Some(path) = out_path {
        std::fs::write(&path, format!("{json}\n")).map_err(|e| e.to_string())?;
        println!("已写入 {path}");
    } else {
        println!("{json}");
    }
    if report.profile == "debug" {
        println!("注意：debug 构建，数字不可用于结论。用 --release 重跑。");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_produces_plausible_numbers() {
        let report = run(200, 3, 100, 3);
        assert_eq!(report.fanout_peers, 3);
        assert_eq!(report.payload_bytes, 100);
        assert!(report.seal_ns > 0.0);
        assert!(report.open_ns > 0.0);
        assert!(report.fanout_us > 0.0);
        // 自检：每个包都真的 fan-out 给了 3 个人。
        assert!(
            (report.fanout_copies_per_packet - 3.0).abs() < 0.01,
            "fan-out 副本数不对：{}",
            report.fanout_copies_per_packet
        );
        // fan-out 到 3 个人不可能比单包封装还便宜。
        assert!(report.fanout_us * 1000.0 > report.seal_ns);
    }
}
