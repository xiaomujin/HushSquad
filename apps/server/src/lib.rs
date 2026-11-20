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

pub use auth::{SharedAuth, SqliteAuth};
use config::{Command, Config};
use server_core::{Server, UserDb};

/// 僵尸会话回收的节拍（见 `Server::sweep_idle`）。
const SWEEP_INTERVAL: Duration = Duration::from_secs(5);

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
        Command::Promote(name) => promote(&config, &name),
        Command::List => list_users(&config),
        Command::Channels => list_channels(&config),
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

fn open_store(config: &Config) -> Result<SharedAuth, String> {
    SqliteAuth::open(&config.db, config.allow_anonymous, config.max_anonymous).map(SharedAuth::new)
}

/// 保证这台服务器有一个服主。
///
/// 库里一个带 `OWNER` 位的账号都没有时，把**最早建的非匿名账号**提上去，并打印一行说明。
/// 覆盖两种场景：
///
/// - 全新部署：`adduser` 建的第一个账号就是服主，不需要用户去理解「权限位」；
/// - 老库升级：以前没有服主这个概念，升级后自动指定一个，而不是让服务器变成没人能管。
///
/// 返回被提上去的用户名（本来就有服主时返回 `None`）。
fn ensure_owner(auth: &SharedAuth) -> Result<Option<String>, String> {
    use server_core::perms;
    let store = auth.inner();
    let has_owner = store
        .with_store(|store| store.any_with_perms(perms::OWNER))
        .map_err(|e| e.to_string())?;
    if has_owner {
        return Ok(None);
    }
    let Some(row) = store
        .with_store(|store| store.first_real_user())
        .map_err(|e| e.to_string())?
    else {
        // 一个非匿名账号都没有（只有游客）。等第一个 adduser。
        return Ok(None);
    };
    store
        .with_store(|store| store.set_perms(row.user_id, perms::OWNER_ROLE))
        .map_err(|e| e.to_string())?;
    Ok(Some(row.username))
}

fn add_user(config: &Config, name: &str) -> Result<(), String> {
    let auth = open_store(config)?;
    let exists = auth
        .inner()
        .with_store(|store| store.find(name).ok().flatten().is_some());
    if exists {
        return Err(format!("用户 {name} 已经存在；改密码请用 passwd {name}"));
    }
    let password = read_password(&format!("给 {name} 设密码："), true)?;
    let hash = UserDb::hash_password(&password).map_err(|e| format!("算哈希失败：{e}"))?;
    auth.inner()
        .with_store(|store| store.add_user(name, &hash, server_core::perms::MEMBER))
        .map_err(|e| e.to_string())?;
    println!("已创建账号 {name}（成员：可以进频道）");
    if let Some(owner) = ensure_owner(&auth)? {
        println!("{owner} 是这个服务器的服主（可以给其他人分配权限）");
    }
    Ok(())
}

/// 把服主位给某个账号（灾难恢复：服主账号丢了、要换人）。
fn promote(config: &Config, name: &str) -> Result<(), String> {
    use server_core::perms;
    let auth = open_store(config)?;
    let store = auth.inner();
    let Some(target) = store
        .with_store(|store| store.find(name))
        .map_err(|e| e.to_string())?
    else {
        return Err(format!("没有 {name} 这个账号"));
    };
    if target.anonymous {
        return Err(format!(
            "{name} 是匿名账号（只能用设备号登录），服主位不能给它"
        ));
    }
    // 先把旧的服主位清掉：服主唯一。
    for row in store
        .with_store(|store| store.list())
        .map_err(|e| e.to_string())?
    {
        if row.user_id != target.user_id && row.perms & perms::OWNER != 0 {
            store
                .with_store(|store| store.set_perms(row.user_id, row.perms & !perms::OWNER))
                .map_err(|e| e.to_string())?;
            println!("{} 不再是服主（降为管理员）", row.username);
        }
    }
    store
        .with_store(|store| store.set_perms(target.user_id, perms::OWNER_ROLE))
        .map_err(|e| e.to_string())?;
    println!("{name} 现在是这个服务器的服主");
    Ok(())
}

fn set_password(config: &Config, name: &str) -> Result<(), String> {
    let auth = open_store(config)?;
    let password = read_password(&format!("给 {name} 设新密码："), true)?;
    let hash = UserDb::hash_password(&password).map_err(|e| format!("算哈希失败：{e}"))?;
    let changed = auth
        .inner()
        .with_store(|store| store.set_password(name, &hash))
        .map_err(|e| e.to_string())?;
    if !changed {
        return Err(format!("没有 {name} 这个账号"));
    }
    println!("已改密：{name}");
    Ok(())
}

fn delete_user(config: &Config, name: &str) -> Result<(), String> {
    use server_core::perms;
    let auth = open_store(config)?;
    let store = auth.inner();
    // 删掉服主会让服务器变成没人能管的状态。要删就先换人。
    if let Some(row) = store
        .with_store(|store| store.find(name))
        .map_err(|e| e.to_string())?
        && row.perms & perms::OWNER != 0
    {
        return Err(format!(
            "{name} 是服主，不能直接删。先用 promote <用户名> 把服主位给别人才行"
        ));
    }
    let removed = store
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
    let store = auth.inner();
    let (rows, anonymous) = store.with_store(|store| {
        (
            store.list().unwrap_or_default(),
            store.anonymous_count().unwrap_or(0),
        )
    });
    if rows.is_empty() {
        println!("一个账号都没有。用 hushsquad-server adduser <用户名> 建一个。");
        return Ok(());
    }
    println!("{:<6} {:<24} {:<8} 权限", "id", "用户名", "类型");
    for row in &rows {
        let kind = if row.anonymous { "匿名" } else { "密码" };
        println!(
            "{:<6} {:<24} {:<8} {}",
            row.user_id,
            row.username,
            kind,
            server_core::perms::describe(row.perms)
        );
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

/// 打印库里那棵频道树（命令行版；图形客户端里能直接改）。
fn list_channels(config: &Config) -> Result<(), String> {
    let auth = open_store(config)?;
    let rows = auth
        .inner()
        .with_store(|store| store.channels().load().map_err(|e| e.to_string()))?;
    if rows.is_empty() {
        println!("库里还没有频道。服务器下次启动时会按 --channel 建一批。");
        return Ok(());
    }
    let mut by_parent: std::collections::BTreeMap<u32, Vec<&store::channels::ChannelRow>> =
        std::collections::BTreeMap::new();
    for row in &rows {
        by_parent.entry(row.parent_id).or_default().push(row);
    }
    for list in by_parent.values_mut() {
        list.sort_by_key(|row| (row.position, row.channel_id));
    }
    // 递归打印。深度上限 3 层，但这里不假设数据是好的：走够 rows.len() 层就停。
    fn walk(
        by_parent: &std::collections::BTreeMap<u32, Vec<&store::channels::ChannelRow>>,
        parent: u32,
        depth: usize,
        budget: usize,
    ) {
        if budget == 0 {
            return;
        }
        for row in by_parent.get(&parent).into_iter().flatten() {
            let indent = "  ".repeat(depth);
            let kind = if row.flags & server_core::channel::FLAG_CONTAINER != 0 {
                "［分组］"
            } else {
                ""
            };
            let cap = if row.max_users == 0 {
                String::new()
            } else {
                format!("（上限 {} 人）", row.max_users)
            };
            println!("{indent}{} {kind}{}{cap}", row.position, row.name);
            walk(by_parent, row.channel_id, depth + 1, budget - 1);
        }
    }
    walk(&by_parent, 0, 0, rows.len() + 1);
    println!("共 {} 个频道", rows.len());
    Ok(())
}

/// 按配置跑起来，直到 Ctrl-C。
pub fn run(config: Config) -> Result<(), String> {
    let auth = open_store(&config)?;
    let info = cert::ensure(&config.cert, &config.key)?;
    let tls = control::load_tls(&config.cert, &config.key)?;
    // 老库升级：以前没有「服主」这个概念，这里补一个，否则服务器没人能管。
    let promoted = ensure_owner(&auth)?;

    // 语音面地址：默认**不宣告**。空串的含义是「沿用客户端连控制面用的地址」，
    // 于是云主机（网卡上只有内网 IP、公网靠 NAT）和局域网都自动正确 ——
    // 以前这里猜一个本机 IP，云主机上猜出来是 10.x，客户端照着连就永远通不了。
    // 只有语音面真的在另一台机器 / 另一个对外 IP 时，才需要 --voice-host 显式指定。
    let voice_host = config.voice_host.clone().unwrap_or_default();
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
        // 认证器与账号管理必须是**同一个**实例：改完权限，认证读到的得是新库。
        server.set_authenticator(Box::new(auth.clone()));
        server.set_user_admin(Box::new(auth.clone()));
        // 频道树也走同一个库。先读，库里是空的才按 --channel 建一批（并立刻落盘）。
        server.set_channel_store(Box::new(auth.clone()));
        let restored = server.restore_channels()?;
        let channel_note = if restored > 0 {
            format!("{restored} 个（从库里读出来的）")
        } else {
            for name in &config.channels {
                server.add_channel(0, name.clone());
            }
            server.persist_channels()?;
            format!("{}（首次建库，已写进库）", config.channels.join("、"))
        };

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
        let announced = if voice_host.is_empty() {
            // 空 = 客户端沿用自己连控制面用的地址，这里没有具体主机可打印。
            format!("跟随客户端（端口 {voice_port}）")
        } else {
            format!("{voice_host}:{voice_port}")
        };
        println!("  语音面    {}（对外宣告 {announced}）", config.voice_bind);
        println!("  账号库    {}", config.db.display());
        println!("  频道      {channel_note}");
        if restored > 0 && config.channels != vec!["大厅".to_owned()] {
            println!("            （--channel 只在首次建库时生效，库里已经有频道了，本次忽略）");
        }
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
        if let Some(owner) = &promoted {
            println!("  服主      {owner}（库里原本没有服主，自动提的；用 promote 换人）");
        }

        metrics_loop(Arc::clone(&shared), control).await;
        Ok::<(), String>(())
    })
}

/// 周期性打印指标与事件，直到收到 Ctrl-C。
async fn metrics_loop(shared: Arc<Shared>, control: tokio::task::JoinHandle<()>) {
    let mut ticker = tokio::time::interval(shared.metrics_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // 僵尸会话回收的节拍。5 秒一次：阈值是 45 秒，误差 5 秒无所谓，
    // 但比 45 秒还粗就会让「刚断线的人重连」撞上还没被收掉的自己。
    let mut sweeper = tokio::time::interval(SWEEP_INTERVAL);
    sweeper.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("收到 Ctrl-C，退出");
                break;
            }
            _ = sweeper.tick() => {
                let reaped = {
                    let mut server = shared.server.lock().expect("锁没中毒");
                    server.sweep_idle(now_ms())
                };
                if reaped > 0 {
                    println!("回收了 {reaped} 条僵尸会话（超过 ping_timeout 没有任何数据）");
                }
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
