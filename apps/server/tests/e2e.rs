//! 端到端联调：真 TLS 1.3 控制面 + 真 UDP 语音面，两个客户端。
//!
//! 这一条测试把 P5 的整条链子串起来：TLS 握手 → 导出器派生语音密钥 → 认证 → 进频道 →
//! UDP 绑定 → 语音 fan-out。任何一环的密钥派生不一致（例如上下文拼错一个字节）都会
//! 让 BindAck 解不开，测试立刻红。
//!
//! 证书是**现场用 rcgen 生成**的，所以这条测试不依赖外部 openssl，也不依赖任何网络。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use hushsquad_server::{Shared, control, voice};
use protocol::control::proto as pb;
use protocol::control::proto::control_message::Kind;
use protocol::{ControlMessage, Framer, PROTOCOL_VERSION, VoiceHeader, encode_frame};
use server_core::{Server, ServerConfig, UserDb};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::ServerName;
use transport::VoiceKeys;

const TIMEOUT: Duration = Duration::from_secs(10);

/// 现场生成的一张自签证书。
struct TestCert {
    cert_pem: String,
    key_pem: String,
    der: rustls::pki_types::CertificateDer<'static>,
}

fn make_cert() -> TestCert {
    let generated =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("生成证书");
    TestCert {
        cert_pem: generated.cert.pem(),
        key_pem: generated.signing_key.serialize_pem(),
        der: generated.cert.der().clone(),
    }
}

/// 一个跑起来的测试服务器。
struct Harness {
    shared: Arc<Shared>,
    control_addr: std::net::SocketAddr,
    voice_addr: std::net::SocketAddr,
    der: rustls::pki_types::CertificateDer<'static>,
    _control: tokio::task::JoinHandle<()>,
    _voice: tokio::task::JoinHandle<()>,
    _dir: std::path::PathBuf,
}

impl Harness {
    async fn start() -> Self {
        let cert = make_cert();
        // 目录名必须**每个测试唯一**。曾经用的是 `pid + SystemTime::now().as_nanos()`，
        // 而 Windows 的墙钟粒度在没提升定时器时是 15.6 ms —— 两个测试线程落在同一个时钟
        // 刻度上就会拿到**同一个目录**，于是 A 的证书和 B 的私钥互相覆盖，服务器拿一对不匹配
        // 的密钥去握手，客户端报 `InvalidCertificate(BadSignature)`。症状是「随机一条 e2e 挂掉、
        // 换一条再跑就换成另一条挂」——机器一忙（比如同时在跑长跑基准）就更容易复现。
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "hushsquad-e2e-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        std::fs::write(&cert_path, &cert.cert_pem).expect("写证书");
        std::fs::write(&key_path, &cert.key_pem).expect("写私钥");

        let acceptor = control::load_tls(&cert_path, &key_path).expect("加载 TLS");
        let (udp, voice_addr) = voice::bind("127.0.0.1:0").await.expect("绑语音面");

        let mut db = UserDb::new();
        db.add_user(1, "alice", "alice-pw", 1).expect("建用户");
        db.add_user(2, "bob", "bob-pw", 1).expect("建用户");
        let mut server = Server::new(ServerConfig {
            voice_host: "localhost".to_owned(),
            voice_port: voice_addr.port(),
            ..ServerConfig::default()
        });
        server.set_authenticator(Box::new(db));
        server.add_channel(0, "大厅");

        let shared = Arc::new(Shared {
            server: Mutex::new(server),
            outbox: Mutex::new(BTreeMap::new()),
            voice_host: "localhost".to_owned(),
            voice_port: voice_addr.port(),
            metrics_interval: Duration::from_secs(60),
            connections: AtomicU64::new(0),
        });

        let _voice = voice::serve(udp, Arc::clone(&shared));
        let (listener, control_addr) = control::bind("127.0.0.1:0").await.expect("绑控制面");
        let _control = control::serve(listener, Arc::clone(&shared), acceptor);

        Self {
            shared,
            control_addr,
            voice_addr,
            der: cert.der,
            _control,
            _voice,
            _dir: dir,
        }
    }

    fn roots(&self) -> rustls::RootCertStore {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(self.der.clone()).expect("加根证书");
        roots
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self._dir);
    }
}

/// 一个客户端。
struct Client {
    tls: tokio_rustls::client::TlsStream<TcpStream>,
    udp: UdpSocket,
    framer: Framer,
    keys: VoiceKeys,
    session_id: u64,
    sender_id: u32,
    voice_token: [u8; 16],
    seq: u32,
    /// 当前频道的 epoch（从 ChannelUpdated 拿）。
    channel_epoch: u16,
}

impl Client {
    async fn connect(harness: &Harness, user: &str, password: &str) -> Self {
        let tcp = TcpStream::connect(harness.control_addr)
            .await
            .expect("连控制面");
        let connector = TlsConnector::from(Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(harness.roots())
                .with_no_client_auth(),
        ));
        let name = ServerName::try_from("localhost").expect("服务器名");
        let tls = connector.connect(name, tcp).await.expect("TLS 握手");

        let udp = UdpSocket::bind("127.0.0.1:0").await.expect("绑客户端 UDP");

        let mut client = Self {
            tls,
            udp,
            framer: Framer::new(),
            keys: VoiceKeys::zeroed(),
            session_id: 0,
            sender_id: 0,
            voice_token: [0u8; 16],
            seq: 0,
            channel_epoch: 0,
        };

        client.send_control(client.hello()).await;
        let hello = client
            .expect(|m| matches!(m.kind, Some(Kind::ServerHello(_))))
            .await;
        let Some(Kind::ServerHello(hello)) = hello.kind else {
            unreachable!()
        };
        client.session_id = hello.session_id;
        client.sender_id = hello.sender_id;
        client.voice_token =
            <[u8; 16]>::try_from(hello.voice_token.as_slice()).expect("token 16 B");
        assert_eq!(hello.voice_port as u16, harness.voice_addr.port());

        // 导出语音密钥：上下文必须和服务器算的一模一样。
        let context = transport::exporter_context(client.session_id, &client.voice_token);
        let mut exporter = [0u8; transport::EXPORTER_LEN];
        exporter = client
            .tls
            .get_ref()
            .1
            .export_keying_material(exporter, transport::EXPORTER_LABEL, Some(&context))
            .expect("客户端导出器");
        client.keys =
            transport::derive_voice_keys(&exporter, client.session_id, hello.key_epoch as u16);

        client.send_control(client.auth(user, password)).await;
        let result = client
            .expect(|m| matches!(m.kind, Some(Kind::AuthResult(_))))
            .await;
        let Some(Kind::AuthResult(result)) = result.kind else {
            unreachable!()
        };
        assert!(result.ok, "{user} 认证失败：{}", result.message);

        client
    }

    fn hello(&self) -> ControlMessage {
        ControlMessage {
            protocol: PROTOCOL_VERSION,
            request_id: 1,
            kind: Some(Kind::ClientHello(pb::ClientHello {
                protocol_min: 1,
                protocol_max: 1,
                client_version: "e2e".to_owned(),
                platform: "test".to_owned(),
                device_id: String::new(),
                caps: 0,
            })),
        }
    }

    fn auth(&self, user: &str, password: &str) -> ControlMessage {
        ControlMessage {
            protocol: PROTOCOL_VERSION,
            request_id: 2,
            kind: Some(Kind::Auth(pb::Auth {
                method: Some(pb::auth::Method::Password(pb::PasswordAuth {
                    username: user.to_owned(),
                    password: password.to_owned(),
                })),
                client_nonce: String::new(),
            })),
        }
    }

    async fn send_control(&mut self, msg: ControlMessage) {
        let mut frame = Vec::new();
        encode_frame(&msg, &mut frame).expect("编码");
        self.tls.write_all(&frame).await.expect("写控制面");
    }

    /// 读到第一条满足条件的消息。
    async fn expect<F>(&mut self, predicate: F) -> ControlMessage
    where
        F: Fn(&ControlMessage) -> bool,
    {
        let fut = async {
            let mut buf = vec![0u8; 8192];
            loop {
                while let Some(msg) = self.framer.poll_frame().expect("分帧") {
                    if predicate(&msg) {
                        return msg;
                    }
                }
                let n = self.tls.read(&mut buf).await.expect("读控制面");
                assert!(n > 0, "服务器关了连接");
                self.framer.push(&buf[..n]).expect("入缓冲");
            }
        };
        tokio::time::timeout(TIMEOUT, fut)
            .await
            .expect("等消息超时")
    }

    async fn join(&mut self, channel_id: u32) {
        self.send_control(ControlMessage {
            protocol: PROTOCOL_VERSION,
            request_id: 3,
            kind: Some(Kind::JoinChannel(pb::JoinChannel {
                channel_id,
                password: String::new(),
            })),
        })
        .await;
        self.expect(|m| matches!(m.kind, Some(Kind::UserJoined(_))))
            .await;
        // 成员一变 epoch 就变，服务器会把新快照推过来；发包必须用这个新的。
        let updated = self
            .expect(|m| matches!(m.kind, Some(Kind::ChannelUpdated(_))))
            .await;
        let Some(Kind::ChannelUpdated(update)) = updated.kind else {
            unreachable!()
        };
        self.channel_epoch = update.channel.expect("有频道").channel_epoch as u16;
        assert_ne!(self.channel_epoch, 0);
    }

    /// 等一条 ChannelUpdated 并把 epoch 更新到最新。
    ///
    /// 别人进/出频道会让 epoch 前进，服务器会推新快照过来 —— 发包前必须把它消费掉，
    /// 否则客户端还在用旧 epoch，服务器一律判「过期」。
    async fn sync_epoch(&mut self) {
        let updated = self
            .expect(|m| matches!(m.kind, Some(Kind::ChannelUpdated(_))))
            .await;
        let Some(Kind::ChannelUpdated(update)) = updated.kind else {
            unreachable!()
        };
        self.channel_epoch = update.channel.expect("有频道").channel_epoch as u16;
    }

    /// 绑定语音面并校验 BindAck。
    async fn bind_voice(&self, server: std::net::SocketAddr) {
        let mut buf = [0u8; 64];
        let n = protocol::voice::encode_bind(&self.voice_token, &mut buf);
        self.udp.send_to(&buf[..n], server).await.expect("发 Bind");

        let mut rbuf = [0u8; 1500];
        let (n, _) = tokio::time::timeout(TIMEOUT, self.udp.recv_from(&mut rbuf))
            .await
            .expect("等 BindAck 超时")
            .expect("收 BindAck");
        let packet = transport::open_packet(
            &self.keys.s2c,
            &mut rbuf[..n],
            protocol::Direction::ServerToClient,
        )
        .expect("BindAck 必须能用导出的 s2c 密钥解开");
        let (_, sender_id) =
            protocol::voice::parse_bind_ack_payload(packet.body()).expect("BindAck 载荷");
        assert_eq!(sender_id, self.sender_id);
    }

    async fn send_voice(&mut self, server: std::net::SocketAddr, payload: &[u8]) {
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
        let mut buf = vec![0u8; protocol::HEADER_LEN + payload.len() + protocol::TAG_LEN];
        let n = transport::seal_packet(&self.keys.c2s, header, payload, &mut buf).expect("加密");
        self.udp.send_to(&buf[..n], server).await.expect("发语音");
    }

    async fn recv_voice(&self) -> Vec<u8> {
        let mut rbuf = [0u8; 1500];
        let (n, _) = tokio::time::timeout(TIMEOUT, self.udp.recv_from(&mut rbuf))
            .await
            .expect("等语音超时")
            .expect("收语音");
        let packet = transport::open_packet(
            &self.keys.s2c,
            &mut rbuf[..n],
            protocol::Direction::ServerToClient,
        )
        .expect("语音必须能用导出的 s2c 密钥解开");
        packet.payload().to_vec()
    }
}
/// 两个人隔着真 TLS + 真 UDP 说话：bob 收到 alice 的帧，alice 收不到自己的。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_clients_talk_over_tls_and_udp() {
    let harness = Harness::start().await;

    let mut alice = Client::connect(&harness, "alice", "alice-pw").await;
    let mut bob = Client::connect(&harness, "bob", "bob-pw").await;
    assert_ne!(alice.session_id, bob.session_id);
    assert_ne!(alice.sender_id, bob.sender_id);

    // 频道 id 是服务器分配的，从 ChannelSync 里拿。
    let sync = alice
        .expect(|m| matches!(m.kind, Some(Kind::ChannelSync(_))))
        .await;
    let Some(Kind::ChannelSync(sync)) = sync.kind else {
        unreachable!()
    };
    let channel = sync.channels[0].channel_id;

    alice.join(channel).await;
    bob.join(channel).await;
    // bob 进来让 epoch 又前进了一次：alice 必须先消化这条 ChannelUpdated。
    alice.sync_epoch().await;

    alice.bind_voice(harness.voice_addr).await;
    bob.bind_voice(harness.voice_addr).await;

    alice
        .send_voice(harness.voice_addr, b"hello-from-alice")
        .await;
    assert_eq!(bob.recv_voice().await, b"hello-from-alice");

    bob.send_voice(harness.voice_addr, b"hello-from-bob").await;
    assert_eq!(alice.recv_voice().await, b"hello-from-bob");

    // 自己不该收到自己的：两个人都只收到 1 帧，再等一会也不该多。
    let mut probe = [0u8; 64];
    let quiet =
        tokio::time::timeout(Duration::from_millis(200), alice.udp.recv_from(&mut probe)).await;
    assert!(quiet.is_err(), "alice 收到了不该有的包");

    let server = harness.shared.server.lock().expect("锁没中毒");
    assert_eq!(server.metrics().binds, 2);
    assert_eq!(server.metrics().fanout_copies, 2);
    assert_eq!(server.metrics().dropped_total(), 0);
    assert_eq!(server.session_count(), 2);
}

/// 认证失败必须断开，而且**不能**给客户端任何语音密钥。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bad_password_is_rejected_over_tls() {
    let harness = Harness::start().await;

    let tcp = TcpStream::connect(harness.control_addr)
        .await
        .expect("连控制面");
    let connector = TlsConnector::from(Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(harness.roots())
            .with_no_client_auth(),
    ));
    let name = ServerName::try_from("localhost").expect("服务器名");
    let mut tls = connector.connect(name, tcp).await.expect("TLS 握手");

    let mut framer = Framer::new();
    let write = |msg: ControlMessage| {
        let mut frame = Vec::new();
        encode_frame(&msg, &mut frame).expect("编码");
        frame
    };
    let hello = write(ControlMessage {
        protocol: PROTOCOL_VERSION,
        request_id: 1,
        kind: Some(Kind::ClientHello(pb::ClientHello {
            protocol_min: 1,
            protocol_max: 1,
            ..Default::default()
        })),
    });
    tls.write_all(&hello).await.expect("写 hello");
    tls.write_all(&write(ControlMessage {
        protocol: PROTOCOL_VERSION,
        request_id: 2,
        kind: Some(Kind::Auth(pb::Auth {
            method: Some(pb::auth::Method::Password(pb::PasswordAuth {
                username: "alice".to_owned(),
                password: "wrong".to_owned(),
            })),
            client_nonce: String::new(),
        })),
    }))
    .await
    .expect("写 auth");

    let mut buf = vec![0u8; 8192];
    let mut saw_reject = false;
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while tokio::time::Instant::now() < deadline && !saw_reject {
        let Ok(n) = tls.read(&mut buf).await else {
            break;
        };
        if n == 0 {
            break;
        }
        framer.push(&buf[..n]).expect("入缓冲");
        while let Some(msg) = framer.poll_frame().expect("分帧") {
            if let Some(Kind::AuthResult(result)) = msg.kind {
                assert!(!result.ok);
                assert_eq!(result.error_code, pb::ErrorCode::AuthFailed as u32);
                saw_reject = true;
            }
        }
    }
    assert!(saw_reject, "没收到认证失败");

    let server = harness.shared.server.lock().expect("锁没中毒");
    assert_eq!(server.metrics().auth_failures, 1);
    assert_eq!(server.metrics().binds, 0, "没认证成功就不该有语音绑定");
}
