//! HushSquad 服务器：TLS 1.3 控制面 + UDP 语音面（SFU）。
//!
//! 这个 crate 是 **lib + bin**：bin 只是壳，真正的逻辑在 lib 里，
//! 这样集成测试（`tests/e2e.rs`）能直接起一个真服务器，而不是另写一份。
//!
//! ```text
//! 客户端 ──TLS/TCP──> 控制面（握手、认证、频道）
//!        ──UDP────> 语音面（SFU fan-out）
//! ```
//!
//! 两个面**故意分开**：控制面要可靠有序，语音面要低延迟可丢，混在一条连接上必然互相拖累。
//! 认证完成后客户端从 ServerHello 拿到 `voice_host/voice_port/voice_token`，再用那个 token
//! 做 UDP 首包绑定 —— 服务器在绑定前对未认证源地址**一个字节都不回**（反放大）。
//!
//! ## 跑起来
//!
//! ```text
//! openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
//!   -keyout key.pem -out cert.pem -days 365 -subj "/CN=localhost"
//! hushsquad-server --users users.txt --cert cert.pem --key key.pem
//! ```

#![deny(unsafe_code)]
// 服务器是 CLI：启动横幅、指标行、事件行都要往 stdout 写。实时线程里才禁止打印。
#![allow(clippy::print_stdout)]

pub mod config;
pub mod control;
pub mod voice;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use config::Config;
use server_core::{Server, UserDb};

/// 毫秒时钟。全服务器共用一个，方便把日志和指标对上。
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// 进程内共享状态。
///
/// `Mutex` 而不是更细的锁：一个服务器的临界区只有「处理一个包」那么长，
/// 8 人规模下这把锁从来不是瓶颈（fan-out 到 7 人只要 12.47 µs，见 measurements §5.1）。
/// 真到瓶颈时再按频道分片 —— 现在分片是过早优化。
pub struct Shared {
    /// 服务器核心。
    pub server: Mutex<Server>,
    /// 每个在线会话的控制面出站队列。连接线程从这里取要推给它的消息。
    pub outbox: Mutex<
        std::collections::BTreeMap<
            server_core::SessionId,
            tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
        >,
    >,
    /// 语音面地址，写进 ServerHello。
    pub voice_host: String,
    /// 语音面端口。
    pub voice_port: u16,
    /// 指标轮询间隔。
    pub metrics_interval: Duration,
    /// 已经服务过的连接数（只用于日志）。
    pub connections: AtomicU64,
}

/// 命令行入口。
pub fn run_cli() -> std::process::ExitCode {
    let config = match Config::parse(std::env::args().skip(1)) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("错误：{message}");
            eprintln!();
            eprintln!("{}", config::USAGE);
            return std::process::ExitCode::FAILURE;
        }
    };
    match run(config) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("错误：{message}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// 按配置跑起来，直到 Ctrl-C。
pub fn run(config: Config) -> Result<(), String> {
    let users = std::fs::read_to_string(&config.users)
        .map_err(|e| format!("读用户文件 {} 失败：{e}", config.users.display()))?;
    let db = UserDb::parse(&users).map_err(|e| format!("用户文件解析失败：{e}"))?;
    if db.is_empty() {
        return Err("用户文件里一个用户都没有".to_owned());
    }

    let tls = control::load_tls(&config.cert, &config.key)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("建 tokio runtime 失败：{e}"))?;

    runtime.block_on(async move {
        let mut server = Server::new(server_core::ServerConfig {
            voice_host: config.voice_host.clone(),
            voice_port: config.voice_port,
            ..server_core::ServerConfig::default()
        });
        server.set_authenticator(Box::new(db));
        for name in &config.channels {
            server.add_channel(0, name.clone());
        }

        let shared = Arc::new(Shared {
            server: Mutex::new(server),
            outbox: Mutex::new(std::collections::BTreeMap::new()),
            voice_host: config.voice_host.clone(),
            voice_port: config.voice_port,
            metrics_interval: config.metrics_interval,
            connections: AtomicU64::new(0),
        });

        // 语音面任务一直跑到进程结束；句柄留着只为「别让它被提前 drop」。
        let _voice = voice::spawn(Arc::clone(&shared), config.voice_bind.clone()).await?;
        let control = control::spawn(Arc::clone(&shared), tls, config.control_bind.clone()).await?;

        println!(
            "hushsquad-server 已启动：控制面 {}，语音面 {}（对外宣告 {}:{}）",
            config.control_bind, config.voice_bind, config.voice_host, config.voice_port
        );

        metrics_loop(Arc::clone(&shared), control).await;
        Ok::<(), String>(())
    })
}

/// 周期性打印指标与事件，直到收到 Ctrl-C。
async fn metrics_loop(shared: Arc<Shared>, control: tokio::task::JoinHandle<()>) {
    let mut ticker = tokio::time::interval(shared.metrics_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("收到 Ctrl-C，退出");
                break;
            }
            _ = ticker.tick() => {
                let (metrics, events) = {
                    let mut server = shared.server.lock().expect("锁没中毒");
                    (server.metrics().to_json(), server.drain_events())
                };
                // 指标一行，事件一行一条：日志采集按行解析。
                println!("metrics {metrics}");
                for event in events {
                    println!("event {}", event.to_json());
                }
            }
        }
    }
    control.abort();
}
