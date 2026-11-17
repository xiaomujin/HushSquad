//! 真实 UDP 环回联调：3 个客户端 + 1 个服务器，走真正的 socket。
//!
//! 单元测试证明的是「判定逻辑对」，这里证明的是「接上真 socket 也对」：
//! 绑定、fan-out、重绑定在真实报文流里成立。整个测试跑在本机环回上，
//! **不代表**真实网络的丢包/抖动/时延 —— 那部分要三个人隔着网络跑（P5-5 的人工部分）。

mod common;

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{Connected, connect, join_channel, server};
use protocol::{Direction, HEADER_LEN, TAG_LEN, VoiceHeader};
use server_core::{DatagramSink, Server};
use transport::{Key, seal};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// 把 {@DatagramSink@} 接到真 socket 上。
struct UdpSink(UdpSocket);

impl DatagramSink for UdpSink {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        // 环回上 send_to 不会阻塞；失败（对端已关）也不该影响别人。
        let _ = self.0.send_to(bytes, to);
    }
}

struct ServerHandle {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<Server>>,
}

impl ServerHandle {
    fn start(mut server: Server) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("绑定环回端口");
        socket
            .set_read_timeout(Some(Duration::from_millis(20)))
            .expect("设置读超时");
        let addr = socket.local_addr().expect("本地地址");
        let sender = socket.try_clone().expect("复制 socket");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let join = thread::spawn(move || {
            let mut sink = UdpSink(sender);
            let mut buf = [0u8; 1500];
            while !stop_thread.load(Ordering::Relaxed) {
                if let Ok((n, from)) = socket.recv_from(&mut buf) {
                    server.handle_datagram(from, &mut buf[..n], now_ms(), &mut sink);
                }
            }
            server
        });
        Self {
            addr,
            stop,
            join: Some(join),
        }
    }

    fn finish(mut self) -> Server {
        self.stop.store(true, Ordering::Relaxed);
        self.join
            .take()
            .expect("线程还在")
            .join()
            .expect("服务器线程没 panic")
    }
}

/// 一个走真 socket 的客户端。
struct Peer {
    sock: UdpSocket,
    c2s: Key,
    s2c: Key,
    sender_id: u32,
    token: [u8; 16],
    seq: u32,
    /// 头部要填的 `channel_epoch`（由服务器在 join 之后给出）。
    channel_epoch: u16,
}

impl Peer {
    fn new(connected: &Connected) -> Self {
        let sock = UdpSocket::bind("127.0.0.1:0").expect("绑定客户端端口");
        sock.set_read_timeout(Some(Duration::from_millis(20)))
            .expect("设置读超时");
        Self {
            sock,
            c2s: connected.c2s.clone(),
            s2c: connected.s2c.clone(),
            sender_id: connected.sender_id,
            token: connected.voice_token,
            seq: 0,
            channel_epoch: 0,
        }
    }

    fn set_epoch(&mut self, epoch: u16) {
        self.channel_epoch = epoch;
    }

    fn addr(&self) -> SocketAddr {
        self.sock.local_addr().expect("本地地址")
    }

    /// 发 Bind 并等 BindAck，返回 (服务器时间, 服务器给的 sender_id)。
    fn bind(&self, server: SocketAddr) -> (u64, u32) {
        let mut buf = [0u8; 64];
        let n = protocol::voice::encode_bind(&self.token, &mut buf);
        self.sock.send_to(&buf[..n], server).expect("发 Bind");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut rbuf = [0u8; 1500];
        while std::time::Instant::now() < deadline {
            if let Ok((n, _)) = self.sock.recv_from(&mut rbuf) {
                let packet =
                    transport::open_packet(&self.s2c, &mut rbuf[..n], Direction::ServerToClient)
                        .expect("BindAck 应当能解开");
                let (server_time, sender_id) =
                    protocol::voice::parse_bind_ack_payload(packet.body()).expect("BindAck 载荷");
                return (server_time, sender_id);
            }
        }
        panic!("等 BindAck 超时");
    }

    /// 从指定 socket 发一帧语音（重绑定测试用）。
    fn send_voice_from(&mut self, sock: &UdpSocket, server: SocketAddr, payload: &[u8]) {
        let header = VoiceHeader {
            flags: 0,
            red_len: 0,
            channel_epoch: self.channel_epoch,
            sender_id: self.sender_id,
            seq: self.seq,
            timestamp: self.seq.wrapping_mul(480),
            opus_len: payload.len() as u16,
        };
        self.seq += 1;
        let mut buf = vec![0u8; HEADER_LEN + payload.len() + TAG_LEN];
        let n = seal::seal_packet(&self.c2s, header, payload, &mut buf).expect("加密");
        sock.send_to(&buf[..n], server).expect("发语音");
    }

    fn send_voice(&mut self, server: SocketAddr, payload: &[u8]) {
        let sock = self.sock.try_clone().expect("复制 socket");
        self.send_voice_from(&sock, server, payload);
    }

    /// 收到至少 `want` 个包，返回解出来的载荷。
    fn drain(&self, want: usize) -> Vec<Vec<u8>> {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut out = Vec::new();
        let mut buf = [0u8; 1500];
        while out.len() < want && std::time::Instant::now() < deadline {
            let Ok((n, _)) = self.sock.recv_from(&mut buf) else {
                continue;
            };
            let packet =
                transport::open_packet(&self.s2c, &mut buf[..n], Direction::ServerToClient)
                    .expect("下行包应当能解开");
            out.push(packet.payload().to_vec());
        }
        out
    }

    /// 在给定时间内收不到任何包就返回空。
    fn expect_silence(&self, window: Duration) -> Vec<Vec<u8>> {
        let deadline = std::time::Instant::now() + window;
        let mut out = Vec::new();
        let mut buf = [0u8; 1500];
        while std::time::Instant::now() < deadline {
            if let Ok((n, _)) = self.sock.recv_from(&mut buf) {
                let packet =
                    transport::open_packet(&self.s2c, &mut buf[..n], Direction::ServerToClient)
                        .expect("下行包应当能解开");
                out.push(packet.payload().to_vec());
            }
        }
        out
    }
}
/// 三个人在同一个频道里互相说话：每个人都该收到另外两个人的全部帧。
#[test]
fn three_clients_talk_over_real_udp() {
    const FRAMES: usize = 10;

    let mut s = server();
    let lobby = s.add_channel(0, "大厅");
    let a = connect(&mut s, "alice", "alice-pw", 1);
    let b = connect(&mut s, "bob", "bob-pw", 2);
    let c = connect(&mut s, "carol", "carol-pw", 3);
    for who in [&a, &b, &c] {
        join_channel(&mut s, who.id, lobby);
    }
    let epoch = s.channel(lobby).expect("频道存在").epoch;

    let handle = ServerHandle::start(s);
    let server_addr = handle.addr;

    let mut pa = Peer::new(&a);
    let mut pb = Peer::new(&b);
    let mut pc = Peer::new(&c);
    for p in [&mut pa, &mut pb, &mut pc] {
        p.set_epoch(epoch);
        let (server_time, sender_id) = p.bind(server_addr);
        assert!(server_time > 0, "BindAck 必须带服务器时间");
        assert_eq!(sender_id, p.sender_id);
    }
    // 绑定后服务器看到的源地址必须就是各自的 socket。
    assert_ne!(pa.addr().port(), pb.addr().port());

    for i in 0..FRAMES {
        pa.send_voice(server_addr, format!("a{i}").as_bytes());
        pb.send_voice(server_addr, format!("b{i}").as_bytes());
        pc.send_voice(server_addr, format!("c{i}").as_bytes());
    }

    // 三人在同一频道：每个人都收到**另外两个人**的帧，且收不到自己的。
    let from_bob = pb.drain(2 * FRAMES);
    let from_carol = pc.drain(2 * FRAMES);
    let from_alice = pa.drain(2 * FRAMES);

    for (name, got, mine, others) in [
        ("alice", &from_alice, b'a', *b"bc"),
        ("bob", &from_bob, b'b', *b"ac"),
        ("carol", &from_carol, b'c', *b"ab"),
    ] {
        assert_eq!(got.len(), 2 * FRAMES, "{name} 收到的包数不对");
        let count_of = |tag: u8| got.iter().filter(|p| p.first() == Some(&tag)).count();
        assert_eq!(count_of(mine), 0, "{name} 收到了自己的包");
        for other in others {
            assert_eq!(
                count_of(other),
                FRAMES,
                "{name} 从 {} 那里应当收到 {FRAMES} 帧",
                other as char
            );
        }
    }

    // 顺序不保证（UDP），但内容必须完整：alice 从 bob 那里收到的应当是 b0..b9 的一个排列。
    let mut bob_frames: Vec<Vec<u8>> = from_alice
        .iter()
        .filter(|p| p.first() == Some(&b'b'))
        .cloned()
        .collect();
    bob_frames.sort();
    let expected: Vec<Vec<u8>> = (0..FRAMES).map(|i| format!("b{i}").into_bytes()).collect();
    assert_eq!(bob_frames, expected, "帧丢了或者串了");

    // 自己不该收到自己的。
    let leftovers = pa.expect_silence(Duration::from_millis(120));
    assert!(leftovers.is_empty(), "收到了不该有的包：{leftovers:?}");

    let server = handle.finish();
    assert_eq!(server.session_count(), 3);
    assert_eq!(server.metrics().binds, 3);
    // 每人 10 帧、每帧转发给另外两个人。
    assert_eq!(server.metrics().fanout_copies, 3 * FRAMES as u64 * 2);
    assert_eq!(server.metrics().dropped_total(), 0, "不该有任何丢弃");
}

/// 换网络：同一个 token 从新 socket 重绑，语音立刻恢复，旧 socket 失效。
#[test]
fn switching_networks_rebinds_within_one_packet() {
    let mut s = server();
    let lobby = s.add_channel(0, "大厅");
    let a = connect(&mut s, "alice", "alice-pw", 1);
    let b = connect(&mut s, "bob", "bob-pw", 2);
    join_channel(&mut s, a.id, lobby);
    join_channel(&mut s, b.id, lobby);
    let epoch = s.channel(lobby).expect("频道存在").epoch;

    let handle = ServerHandle::start(s);
    let server_addr = handle.addr;

    let mut pa = Peer::new(&a);
    let mut pb = Peer::new(&b);
    pa.set_epoch(epoch);
    pb.set_epoch(epoch);
    pa.bind(server_addr);
    pb.bind(server_addr);

    // 老网络还能用。
    pa.send_voice(server_addr, b"old-net");
    assert_eq!(pb.drain(1), vec![b"old-net".to_vec()]);

    // 换网络：新 socket，同一个 token 重绑。
    let new_sock = UdpSocket::bind("127.0.0.1:0").expect("绑定新端口");
    new_sock
        .set_read_timeout(Some(Duration::from_millis(20)))
        .expect("设置读超时");
    let mut buf = [0u8; 64];
    let n = protocol::voice::encode_bind(&pa.token, &mut buf);
    new_sock.send_to(&buf[..n], server_addr).expect("发 Bind");
    // 收 BindAck。
    let mut rbuf = [0u8; 1500];
    let (n, _) = new_sock.recv_from(&mut rbuf).expect("等 BindAck");
    assert!(n > 0);

    // 新 socket 立刻可用。
    pa.send_voice_from(&new_sock, server_addr, b"new-net");
    assert_eq!(pb.drain(1), vec![b"new-net".to_vec()]);

    // 旧 socket 已经失效：再发不会有任何东西到 bob。
    pa.send_voice(server_addr, b"ghost");
    assert!(
        pb.expect_silence(Duration::from_millis(150)).is_empty(),
        "旧地址不该还能发包"
    );

    let server = handle.finish();
    assert_eq!(server.metrics().rebinds, 1);
    assert!(server.metrics().dropped(server_core::DropReason::Unbound) >= 1);
}

/// 跨频道：同一条连接换频道后，用旧 epoch 造的包会被服务器丢掉，谁也收不到。
#[test]
fn a_stale_epoch_packet_never_reaches_the_new_channel() {
    let mut s = server();
    let lobby = s.add_channel(0, "大厅");
    let gaming = s.add_channel(0, "游戏");
    let a = connect(&mut s, "alice", "alice-pw", 1);
    let b = connect(&mut s, "bob", "bob-pw", 2);
    join_channel(&mut s, a.id, lobby);
    join_channel(&mut s, b.id, lobby);
    let old_epoch = s.channel(lobby).expect("频道存在").epoch;

    // bob 也搬到游戏频道，两个人还在一起（只是换了频道）。
    join_channel(&mut s, a.id, gaming);
    join_channel(&mut s, b.id, gaming);
    let new_epoch = s.channel(gaming).expect("频道存在").epoch;
    assert_ne!(old_epoch, new_epoch);

    let handle = ServerHandle::start(s);
    let server_addr = handle.addr;
    let mut pa = Peer::new(&a);
    let mut pb = Peer::new(&b);
    pa.set_epoch(old_epoch); // 故意用旧 epoch
    pb.set_epoch(new_epoch);
    pa.bind(server_addr);
    pb.bind(server_addr);

    pa.send_voice(server_addr, b"stale");
    assert!(
        pb.expect_silence(Duration::from_millis(150)).is_empty(),
        "旧 epoch 的包不该被转发"
    );

    // 换成正确 epoch 就通了。
    pa.set_epoch(new_epoch);
    pa.send_voice(server_addr, b"fresh");
    assert_eq!(pb.drain(1), vec![b"fresh".to_vec()]);

    let server = handle.finish();
    assert!(
        server
            .metrics()
            .dropped(server_core::DropReason::WrongEpoch)
            >= 1
    );
}
