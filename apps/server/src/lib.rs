//! HushSquad 服务器：TLS 1.3 控制面 + UDP 语音面（SFU）。
//!
//! 这个 crate 是 **lib + bin**：bin 只是壳，真正的逻辑在 lib 里，
//! 这样集成测试能直接起一个真服务器，而不是另写一份。
//!
//! 两个面**故意分开**：控制面要可靠有序，语音面要低延迟可丢，混在一条连接上必然互相拖累。
//! 认证完成后客户端从 ServerHello 拿到 voice_host/voice_port/voice_token，再用那个 token
//! 做 UDP 首包绑定 —— 服务器在绑定前对未认证源地址**一个字节都不回**（反放大）。
//!
//! # 跑起来
//!
//! hushsquad-server              第一次跑自动建库 + 建证书，打印证书指纹
//! hushsquad-server adduser alice  加账号（密码不回显）
//!
//! 账号存在 SQLite 里（默认 hushsquad.db），密码是 argon2id 的 PHC 串。
//! 支持三种登录：用户名+密码、用户名+设备 token（免密）、只凭设备号（匿名）。

#![deny(unsafe_code)]
// 服务器是 CLI：启动横幅、指标行、事件行都要往 stdout 写。实时线程里才禁止打印。
#![allow(clippy::print_stdout)]

pub mod auth;
pub mod cert;
pub mod config;
pub mod control;
pub mod voice;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use auth::SqliteAuth;
use config::{Command, Config};
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
/// Mutex 而不是更细的锁：一个服务器的临界区只有「处理一个包」那么长，
/// 8 人规模下这把锁从来不是瓶颈（fan-out 到 7 人只要 12.47 us）。
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
    match dispatch(config) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("错误：{message}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn dispatch(config: Config) -> Result<(), String> {
    match config.command.clone() {
        Command::Help => {
            println!("{}", config::USAGE);
            Ok(())
        }
        Command::Serve => run(config),
        Command::AddUser(name) => add_user(&config, &name),
        Command::Passwd(name) => set_password(&config, &name),
        Command::DelUser(name) => delete_user(&config, &name),
        Command::List => list_users(&config),
        Command::Cert => regenerate_cert(&config),
    }
}

/// 读密码：交互终端里不回显；非交互（管道/CI）时读一行。
fn read_password(prompt: &str, confirm: bool) -> Result<String, String> {
    use std::io::{BufRead, IsTerminal, Write};
    if std::io::stdin().is_terminal() {
        let password =
            rpassword::prompt_password(prompt).map_err(|e| format!("读密码失败：{e}"))?;
        if confirm {
            let again =
                rpassword::prompt_password("再输一遍：").map_err(|e| format!("读密码失败：{e}"))?;
            if password != again {
                return Err("两次输入的密码不一样".to_owned());
            }
        }
        if password.is_empty() {
            return Err("密码不能为空".to_owned());
        }
        return Ok(password);
    }
    // 非交互：从 stdin 读一行，方便脚本里 echo pw | hushsquad-server adduser。
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("读密码失败：{e}"))?;
    let password = line.trim_end_matches(['\r', '\n']).to_owned();
    if password.is_empty() {
        return Err("密码不能为空".to_owned());
    }
    Ok(password)
}

fn open_store(config: &Config) -> Result<SqliteAuth, String> {
    SqliteAuth::open(&config.db, config.allow_anonymous, config.max_anonymous)
}

fn add_user(config: &Config, name: &str) -> Result<(), String> {
    let auth = open_store(config)?;
    let exists = auth.with_store(|store| store.find(name).ok().flatten().is_some());
    if exists {
        return Err(format!("用户 {name} 已经存在；改密码请用 passwd {name}"));
    }
    let password = read_password(&format!("给 {name} 设密码："), true)?;
    let hash = UserDb::hash_password(&password).map_err(|e| format!("算哈希失败：{e}"))?;
    auth.with_store(|store| store.add_user(name, &hash, 1))
        .map_err(|e| e.to_string())?;
    println!("已创建账号 {name}（权限位 1：可进频道）");
    Ok(())
}

fn set_password(config: &Config, name: &str) -> Result<(), String> {
    let auth = open_store(config)?;
    let password = read_password(&format!("给 {name} 设新密码："), true)?;
    let hash = UserDb::hash_password(&password).map_err(|e| format!("算哈希失败：{e}"))?;
    let changed = auth
        .with_store(|store| store.set_password(name, &hash))
        .map_err(|e| e.to_string())?;
    if !changed {
        return Err(format!("没有 {name} 这个账号"));
    }
    println!("已改密：{name}");
    Ok(())
}

fn delete_user(config: &Config, name: &str) -> Result<(), String> {
    let auth = open_store(config)?;
    let removed = auth
        .with_store(|store| store.remove(name))
        .map_err(|e| e.to_string())?;
    if !removed {
        return Err(format!("没有 {name} 这个账号"));
    }
    println!("已删除 {name}（它的设备绑定也一起删了）");
    Ok(())
}

fn list_users(config: &Config) -> Result<(), String> {
    let auth = open_store(config)?;
    let (rows, anonymous) = auth.with_store(|store| {
        (
            store.list().unwrap_or_default(),
            store.anonymous_count().unwrap_or(0),
        )
    });
    if rows.is_empty() {
        println!("一个账号都没有。用 hushsquad-server adduser <用户名> 建一个。");
        return Ok(());
    }
    println!("{:<6} {:<24} {:<8} 说明", "id", "用户名", "类型");
    for row in &rows {
        let kind = if row.anonymous { "匿名" } else { "密码" };
        let note = if row.password_hash.is_empty() {
            "只能用绑定的设备号登录"
        } else {
            ""
        };
        println!("{:<6} {:<24} {:<8} {note}", row.user_id, row.username, kind);
    }
    println!("共 {} 个账号，其中匿名 {anonymous} 个", rows.len());
    Ok(())
}

fn regenerate_cert(config: &Config) -> Result<(), String> {
    let info = cert::generate(&config.cert, &config.key)?;
    println!("已生成 {}", config.cert.display());
    println!("证书指纹（客户端第一次连时会记住它）：");
    println!("  {}", cert::format_fingerprint(&info.fingerprint));
    println!("可用名字：{}", info.names.join(", "));
    Ok(())
}

/// 按配置跑起来，直到 Ctrl-C。
pub fn run(config: Config) -> Result<(), String> {
    let auth = open_store(&config)?;
    let info = cert::ensure(&config.cert, &config.key)?;
    let tls = control::load_tls(&config.cert, &config.key)?;

    // 语音面地址：没显式给就猜一个局域网 IP —— 猜错了客户端只会连不上语音面，
    // 启动横幅里那句「对外宣告」能让用户一眼看出该改哪里。
    let voice_host = match &config.voice_host {
        Some(host) => host.clone(),
        None => cert::local_ip().map_or_else(
            || {
                eprintln!("警告：猜不出本机局域网地址，语音面默认宣告 127.0.0.1（只有本机能连）。");
                eprintln!("      用 --voice-host <你的IP或域名> 指定。");
                "127.0.0.1".to_owned()
            },
            |ip| ip.to_string(),
        ),
    };
    let voice_port = config.voice_port();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("建 tokio runtime 失败：{e}"))?;

    runtime.block_on(async move {
        let mut server = Server::new(server_core::ServerConfig {
            voice_host: voice_host.clone(),
            voice_port,
            ..server_core::ServerConfig::default()
        });
        server.set_authenticator(Box::new(auth));
        for name in &config.channels {
            server.add_channel(0, name.clone());
        }

        let shared = Arc::new(Shared {
            server: Mutex::new(server),
            outbox: Mutex::new(std::collections::BTreeMap::new()),
            voice_host: voice_host.clone(),
            voice_port,
            metrics_interval: config.metrics_interval.unwrap_or(Duration::from_secs(3600)),
            connections: AtomicU64::new(0),
        });

        // 语音面任务一直跑到进程结束；句柄留着只为「别让它被提前 drop」。
        let _voice = voice::spawn(Arc::clone(&shared), config.voice_bind.clone()).await?;
        let control = control::spawn(Arc::clone(&shared), tls, config.control_bind.clone()).await?;

        println!("HushSquad 服务器已启动");
        println!("  控制面    {}", config.control_bind);
        println!(
            "  语音面    {}（对外宣告 {voice_host}:{voice_port}）",
            config.voice_bind
        );
        println!("  账号库    {}", config.db.display());
        println!("  频道      {}", config.channels.join("、"));
        println!(
            "  匿名登录  {}",
            if config.allow_anonymous {
                format!("允许（上限 {} 个）", config.max_anonymous)
            } else {
                "不允许".to_owned()
            }
        );
        println!("  证书      {}", config.cert.display());
        println!(
            "  指纹      {}",
            cert::format_fingerprint(&info.fingerprint)
        );
        if info.generated {
            println!("            （刚生成；客户端第一次连会自动记住它）");
        }

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
