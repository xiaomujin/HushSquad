//! 端到端联调：这个二进制自己的代码路径对着同进程里的一台真服务器跑一遍。
//!
//! 覆盖的是**用户实际会走的那几条路**：
//!
//! - 不带 --user 时按设备号匿名进入，服务器自动建号；
//! - 账号 + 密码登录，之后同一台机器凭下发的设备 token 免密；
//! - 证书默认 TOFU：第一次记住指纹，换了证书就拒绝；
//! - `--ca` 仍然可用。
//!
//! 两个客户端都不碰声卡：一个用合成正弦音当麦克风（--tone），另一个只听。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use hushsquad_client::{Config, Summary};
use hushsquad_server::SqliteAuth;
use hushsquad_server::{Shared, control, voice};
use server_core::{Server, ServerConfig};
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
    fn start(runtime: &tokio::runtime::Runtime, users: &[(&str, &str)]) -> Self {
        let generated =
            rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("生成证书");
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "hushsquad-client-e2e-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        std::fs::write(&cert, generated.cert.pem()).expect("写证书");
        std::fs::write(&key, generated.signing_key.serialize_pem()).expect("写私钥");

        let acceptor = control::load_tls(&cert, &key).expect("加载 TLS");
        let auth_db = dir.join("server.db");
        let (shared, control_addr, control_task, voice_task) =
            runtime.block_on(setup(acceptor, users, &auth_db));

        Self {
            shared,
            control_addr,
            cert,
            dir,
            _control: control_task,
            _voice: voice_task,
        }
    }

    /// 客户端配置：走真实的参数解析，只把本地库指到临时目录。
    fn config(&self, args: &[&str], db: &Path) -> Config {
        let mut argv: Vec<String> = vec![
            "--server".to_owned(),
            self.control_addr.to_string(),
            "--server-name".to_owned(),
            "localhost".to_owned(),
            "--no-mic".to_owned(),
            "--no-speaker".to_owned(),
            "--chain".to_owned(),
            "none".to_owned(),
            "--bind".to_owned(),
            "127.0.0.1:0".to_owned(),
            "--stats-interval".to_owned(),
            "0".to_owned(),
            // 打点默认 5 秒，而测试只跑 3 秒 —— 显式调密一点才测得到 RTT。
            "--probe-interval".to_owned(),
            "1".to_owned(),
            "--quiet".to_owned(),
            "--run-for".to_owned(),
            "3".to_owned(),
            "--db".to_owned(),
            db.display().to_string(),
        ];
        argv.extend(args.iter().map(|s| (*s).to_owned()));
        Config::parse(argv.into_iter()).expect("解析客户端参数")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn setup(
    acceptor: TlsAcceptor,
    users: &[(&str, &str)],
    auth_db: &std::path::Path,
) -> (Arc<Shared>, SocketAddr, JoinHandle<()>, JoinHandle<()>) {
    let (udp, voice_addr) = voice::bind("127.0.0.1:0").await.expect("绑语音面");

    let auth = SqliteAuth::open(auth_db, true, 200).expect("账号库");
    auth.with_store(|store| {
        for (user, password) in users {
            let hash = server_core::UserDb::hash_password(password).expect("哈希");
            store.add_user(user, &hash, 1).expect("建号");
        }
    });
    let mut server = Server::new(ServerConfig {
        // 用字面 IP：客户端要 lookup 它，别让 localhost 解析到 ::1 上去。
        voice_host: "127.0.0.1".to_owned(),
        voice_port: voice_addr.port(),
        ..ServerConfig::default()
    });
    server.set_authenticator(Box::new(auth));
    server.add_channel(0, "大厅");

    let shared = Arc::new(Shared {
        server: std::sync::Mutex::new(server),
        outbox: std::sync::Mutex::new(std::collections::BTreeMap::new()),
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

fn spawn_client(config: Config) -> std::thread::JoinHandle<Result<Summary, String>> {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("起客户端运行时");
        runtime.block_on(hushsquad_client::run(config))
    })
}

impl Harness {
    /// 服务器账号库里有多少个账号（用来断言「同一个设备号没被重复建号」）。
    fn account_count(&self) -> u32 {
        let store = store::users::UserStore::open(&self.dir.join("server.db")).expect("打开账号库");
        store.count().expect("数账号")
    }

    /// 匿名账号数。
    fn anonymous_count(&self) -> u32 {
        let store = store::users::UserStore::open(&self.dir.join("server.db")).expect("打开账号库");
        store.anonymous_count().expect("数匿名账号")
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("起服务器运行时")
}

#[test]
fn using_only_a_server_address_works_and_is_anonymous() {
    // 用户抱怨的就是这一步太复杂：现在只要一个 --server。
    let runtime = runtime();
    let harness = Harness::start(&runtime, &[("alice", "alice-pw")]);
    // 两个客户端 = 两台机器，各自一个设备号（同一个库会被服务器当成同一个账号）。
    let sender_db = harness.dir.join("sender.db");
    let receiver_db = harness.dir.join("receiver.db");

    let mut sender_config = harness.config(&[], &sender_db);
    sender_config.tone = Some(440.0);
    let receiver_config = harness.config(&[], &receiver_db);

    // 两个都先起来再 join：写成「spawn → join → spawn」会变成串行，
    // 先跑完的那个退出时对方还没进频道，于是谁都听不到谁。
    let sender = spawn_client(sender_config);
    let receiver = spawn_client(receiver_config);
    let sender = sender
        .join()
        .expect("线程没 panic")
        .expect("匿名客户端跑失败");
    let receiver = receiver
        .join()
        .expect("线程没 panic")
        .expect("匿名客户端跑失败");

    assert!(
        sender.tick_hz > 90.0,
        "实际 tick 只有 {:.1} Hz，10 ms 节奏没跑起来",
        sender.tick_hz
    );
    assert!(
        sender.frames_out > 250,
        "上行太少：{} 包",
        sender.frames_out
    );
    let metrics = {
        let server = harness.shared.server.lock().expect("锁没中毒");
        server.metrics().to_json()
    };
    assert!(
        receiver.frames_in > 100,
        "下行太少：{} 包；服务器：{metrics}",
        receiver.frames_in
    );
    assert_eq!(receiver.recv_errors, 0, "有包解不开");
    assert!(receiver.rtt_p50_us.is_some(), "一个 RTT 样本都没有");

    // 服务器那边：两个匿名账号，各自绑了自己那台设备。
    let server = harness.shared.server.lock().expect("锁没中毒");
    let metrics = server.metrics();
    assert_eq!(metrics.binds, 2, "两个客户端都该绑定语音面");
    assert!(metrics.fanout_copies > 0, "服务器没做过 fan-out");
    drop(server);
    assert_eq!(harness.anonymous_count(), 2, "两台机器应当各建一个匿名账号");
}

#[test]
fn the_same_machine_keeps_the_same_anonymous_identity() {
    // 「凭设备号进」的意义在于：换个进程、换个目录还是同一个人。
    let runtime = runtime();
    let harness = Harness::start(&runtime, &[]);
    let db = harness.dir.join("client.db");

    let mut first_config = harness.config(&[], &db);
    first_config.tone = Some(440.0);
    let first = spawn_client(first_config)
        .join()
        .expect("线程没 panic")
        .expect("第一次跑失败");
    let mut second_config = harness.config(&[], &db);
    second_config.tone = Some(440.0);
    let second = spawn_client(second_config)
        .join()
        .expect("线程没 panic")
        .expect("第二次跑失败");
    assert!(first.frames_out > 0 && second.frames_out > 0);

    // 关键断言：跑两次只该有**一个**匿名账号 —— 第二次是复用同一个设备号，
    // 不是又建了个新身份（那样用户会莫名其妙变成另一个人）。
    assert_eq!(harness.account_count(), 1, "同一个设备号不该建出两个账号");
}

#[test]
fn an_account_logs_in_and_then_remembers_the_device() {
    let runtime = runtime();
    let harness = Harness::start(&runtime, &[("alice", "alice-pw")]);
    let db = harness.dir.join("client.db");

    // 第一次：给密码。
    let mut with_password = harness.config(&["--user", "alice", "--password", "alice-pw"], &db);
    with_password.tone = Some(440.0);
    spawn_client(with_password)
        .join()
        .expect("线程没 panic")
        .expect("账号登录失败");

    // 第二次：不给密码 —— 上次登录时服务器下发了设备 token，客户端已经存下来了。
    let mut without_password = harness.config(&["--user", "alice"], &db);
    without_password.tone = Some(440.0);
    let summary = spawn_client(without_password)
        .join()
        .expect("线程没 panic")
        .expect("免密登录失败：设备 token 没有被存下来或没被用上");
    assert!(summary.frames_out > 0);
}

#[test]
fn a_wrong_password_is_rejected_before_the_voice_path() {
    let runtime = runtime();
    let harness = Harness::start(&runtime, &[("alice", "alice-pw")]);
    let db = harness.dir.join("client.db");

    let config = harness.config(&["--user", "alice", "--password", "wrong"], &db);
    let error = spawn_client(config)
        .join()
        .expect("线程没 panic")
        .expect_err("错密码必须失败");
    assert!(
        error.contains("拒绝") || error.contains("认证"),
        "报错要说清楚：{error}"
    );

    let server = harness.shared.server.lock().expect("锁没中毒");
    assert_eq!(server.metrics().binds, 0, "没认证成功就不该有语音绑定");
}

#[test]
fn the_certificate_is_remembered_and_a_change_is_refused() {
    let runtime = runtime();
    let harness = Harness::start(&runtime, &[("alice", "alice-pw")]);
    let db = harness.dir.join("client.db");

    // 第一次连：TOFU，记下指纹。
    spawn_client(harness.config(&["--user", "alice", "--password", "alice-pw"], &db))
        .join()
        .expect("线程没 panic")
        .expect("首次连接应当成功（TOFU）");

    // 把记住的指纹改掉，模拟「服务器换了证书」。
    {
        let store = store::client::ClientStore::open(&db).expect("打开本地库");
        let host = harness.control_addr.to_string();
        assert!(
            store.fingerprint(&host).expect("读").is_some(),
            "首次连接应当记下指纹"
        );
        store.trust(&host, &[0xAA; 32]).expect("改指纹");
    }

    let error = spawn_client(harness.config(&["--user", "alice", "--password", "alice-pw"], &db))
        .join()
        .expect("线程没 panic")
        .expect_err("指纹变了必须拒绝");
    assert!(error.contains("指纹变了"), "报错要指出是证书换了：{error}");
    assert!(error.contains("--reset-trust"), "报错要给出下一步：{error}");
}

#[test]
fn an_explicit_ca_still_works() {
    let runtime = runtime();
    let harness = Harness::start(&runtime, &[("alice", "alice-pw")]);
    let db = harness.dir.join("client.db");
    let ca = harness.cert.display().to_string();

    let mut config = harness.config(
        &["--user", "alice", "--password", "alice-pw", "--ca", &ca],
        &db,
    );
    config.tone = Some(440.0);
    let summary = spawn_client(config)
        .join()
        .expect("线程没 panic")
        .expect("--ca 路径应当可用");
    assert!(summary.frames_out > 0);
}
