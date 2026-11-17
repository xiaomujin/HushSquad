//! 端到端联调：这个**二进制自己的**代码路径（config → control → voice → 事件循环）
//! 对着同进程里的一台真服务器跑一遍。
//!
//! 为什么要这条测试：`apps/server/tests/e2e.rs` 证明的是协议两端能通，但它自己写了一个
//! 精简客户端。这里跑的是 `hushsquad-client` 真正会走的代码 —— 握手顺序、导出器、Bind、
//! 频道选择、10 ms 循环、抖动缓冲、混音器挂轨。任何一处接错，这条测试就红。
//!
//! 两个客户端都**不碰声卡**：一个用合成正弦音当麦克风（`--tone`），另一个只听。
//! 于是这条测试能在没有音频设备的机器上跑。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use hushsquad_client::{Config, Summary};
use hushsquad_server::{Shared, control, voice};
use server_core::{Server, ServerConfig, UserDb};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

/// 一台跑起来的服务器 + 它的自签证书。
struct Harness {
    shared: Arc<Shared>,
    control_addr: SocketAddr,
    cert: PathBuf,
    dir: PathBuf,
    _control: JoinHandle<()>,
    _voice: JoinHandle<()>,
}

impl Harness {
    fn start(runtime: &tokio::runtime::Runtime) -> Self {
        let generated =
            rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("生成证书");
        // 三条测试并行跑：目录名必须唯一，否则会互相把证书/私钥删掉或截断。
        // 时间戳单独用不够（Windows 时钟粒度 100 ns，同一 tick 建两个 harness 是可能的），
        // 再加一个进程内自增序号。
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "hushsquad-client-e2e-{}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("系统时间")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        std::fs::write(&cert, generated.cert.pem()).expect("写证书");
        std::fs::write(&key, generated.signing_key.serialize_pem()).expect("写私钥");

        let acceptor = control::load_tls(&cert, &key).expect("加载 TLS");
        let (shared, control_addr, control_task, voice_task) = runtime.block_on(setup(acceptor));

        Self {
            shared,
            control_addr,
            cert,
            dir,
            _control: control_task,
            _voice: voice_task,
        }
    }

    /// 一个客户端的配置：连这台服务器、信任它的证书、不开声卡。
    fn config(&self, user: &str, password: &str) -> Config {
        Config {
            server: self.control_addr.to_string(),
            user: user.to_owned(),
            password: password.to_owned(),
            channel: "大厅".to_owned(),
            ca: Some(self.cert.clone()),
            insecure: false,
            // 证书是自签给 localhost 的；连的地址是 127.0.0.1，所以名字要显式给。
            server_name: Some("localhost".to_owned()),
            mic: None,
            speaker: None,
            no_mic: true,
            no_speaker: true,
            chain: "none".to_owned(),
            dtx: false,
            tone: None,
            jitter_min_ms: 20,
            jitter_max_ms: 60,
            bind: "127.0.0.1:0".to_owned(),
            stats_interval: None,
            probe_interval: Some(Duration::from_secs(1)),
            run_for: Some(Duration::from_secs(3)),
            list_devices: false,
            quiet: true,
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn setup(acceptor: TlsAcceptor) -> (Arc<Shared>, SocketAddr, JoinHandle<()>, JoinHandle<()>) {
    let (udp, voice_addr) = voice::bind("127.0.0.1:0").await.expect("绑语音面");

    let mut db = UserDb::new();
    db.add_user(1, "alice", "alice-pw", 1).expect("建用户");
    db.add_user(2, "bob", "bob-pw", 1).expect("建用户");
    let mut server = Server::new(ServerConfig {
        // 用字面 IP：客户端要 lookup 它，别让 localhost 解析到 ::1 上去。
        voice_host: "127.0.0.1".to_owned(),
        voice_port: voice_addr.port(),
        ..ServerConfig::default()
    });
    server.set_authenticator(Box::new(db));
    server.add_channel(0, "大厅");

    let shared = Arc::new(Shared {
        server: Mutex::new(server),
        outbox: Mutex::new(std::collections::BTreeMap::new()),
        voice_host: "127.0.0.1".to_owned(),
        voice_port: voice_addr.port(),
        metrics_interval: Duration::from_secs(3600),
        connections: AtomicU64::new(0),
    });

    let voice_task = voice::serve(udp, Arc::clone(&shared));
    let (listener, control_addr) = control::bind("127.0.0.1:0").await.expect("绑控制面");
    let control_task = control::serve(listener, Arc::clone(&shared), acceptor);
    (shared, control_addr, control_task, voice_task)
}

/// 在一个独立线程上跑一个客户端（客户端是单线程运行时，见 main.rs 的注释）。
fn spawn_client(config: Config) -> std::thread::JoinHandle<Result<Summary, String>> {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("起客户端运行时");
        runtime.block_on(hushsquad_client::run(config))
    })
}

#[test]
fn two_clients_talk_through_the_real_client_path() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("起服务器运行时");
    let harness = Harness::start(&runtime);

    // alice 用合成正弦音当麦克风；bob 只听。两个都不开声卡。
    let mut alice_config = harness.config("alice", "alice-pw");
    alice_config.tone = Some(440.0);
    let bob_config = harness.config("bob", "bob-pw");

    let alice = spawn_client(alice_config);
    let bob = spawn_client(bob_config);

    let alice = alice
        .join()
        .expect("alice 线程没 panic")
        .expect("alice 会话失败");
    let bob = bob.join().expect("bob 线程没 panic").expect("bob 会话失败");

    // ---- 上行：alice 必须真的把 100 包/秒发出去了 ----
    // 节奏是这条测试最敏感的一条：Windows 上不调定时器精度会掉到 64 Hz，
    // 上行只发得出 640 ms/秒 的音频。
    assert!(
        alice.tick_hz > 90.0,
        "实际 tick 只有 {:.1} Hz，10 ms 节奏没跑起来（定时器精度没提升？）",
        alice.tick_hz
    );
    assert!(
        alice.frames_out > 250,
        "alice 3 秒只发了 {} 个包，10 ms 节奏没跑起来",
        alice.frames_out
    );
    assert_eq!(alice.send_errors, 0, "alice 发不出去包");

    // ---- 下行：bob 必须收到并解开 ----
    assert!(
        bob.frames_in > 100,
        "bob 只收到 {} 个包：语音面没通，或者 fan-out 没生效",
        bob.frames_in
    );
    assert_eq!(
        bob.recv_errors, 0,
        "bob 有 {} 个包解不开 —— 两端的语音密钥派生不一致",
        bob.recv_errors
    );
    assert_eq!(bob.peers.len(), 1, "bob 应当恰好认识一个说话人");
    let speaker = &bob.peers[0];
    assert!(
        speaker.frames_decoded > 0,
        "抖动缓冲一帧都没解出来：Opus 载荷或序号接错了"
    );
    assert!(speaker.buffered_ms > 0, "抖动缓冲是空的，说明数据没进去");

    // ---- 打点：语音面的往返必须是通的 ----
    let rtt = alice.rtt_p50_us.expect("一个 RTT 样本都没有");
    assert!(rtt > 0, "RTT 是 0，说明打点包没真走一圈");
    assert!(
        rtt < 500_000,
        "本机环回的 RTT 居然是 {rtt} 微秒，打点的时间戳接错了"
    );

    // ---- 服务器侧：两个绑定、真的做过 fan-out ----
    let server = harness.shared.server.lock().expect("锁没中毒");
    let metrics = server.metrics();
    assert_eq!(metrics.binds, 2, "两个客户端都该绑定过语音面");
    assert!(
        metrics.fanout_copies > 0,
        "服务器一个副本都没转发：epoch 或目标集合算错了"
    );
    // 不断言 session_count：两个客户端已经跑完 3 秒并断开，会话本来就该归零。
    assert!(
        metrics.datagrams_in > 0 && metrics.datagrams_out > 0,
        "服务器没有收到/发出语音报文"
    );
}

#[test]
fn a_bad_password_stops_the_client_before_the_voice_path() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("起服务器运行时");
    let harness = Harness::start(&runtime);

    let mut config = harness.config("alice", "wrong-password");
    config.run_for = Some(Duration::from_secs(1));
    let error = spawn_client(config)
        .join()
        .expect("线程没 panic")
        .expect_err("错密码必须失败");
    // 服务器给的原文是 `bad password`，客户端把它包成「服务器拒绝了会话」。
    assert!(
        error.contains("拒绝") || error.contains("认证"),
        "报错要指明是认证失败，实际是：{error}"
    );

    // 认证没过就不该有语音绑定：服务器连 token 都不该认。
    let server = harness.shared.server.lock().expect("锁没中毒");
    assert_eq!(server.metrics().binds, 0, "没认证成功就不该有语音绑定");
}

#[test]
fn refusing_a_server_whose_certificate_is_not_trusted() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("起服务器运行时");
    let harness = Harness::start(&runtime);

    // 不给 --ca，也不给 --insecure：必须在连上之前就拒绝。
    let mut config = harness.config("alice", "alice-pw");
    config.ca = None;
    config.run_for = Some(Duration::from_secs(1));
    let error = spawn_client(config)
        .join()
        .expect("线程没 panic")
        .expect_err("没有信任链就必须失败");
    assert!(
        error.contains("--ca"),
        "报错要告诉用户怎么修，实际是：{error}"
    );
}
