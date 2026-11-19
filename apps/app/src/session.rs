//! 会话线程：把 `hushsquad_client::run_with` 放在后台线程上跑，状态经
//! Tauri 的 [`Channel`] 推给前端，指令从同一份状态里拉。
//!
//! # 为什么是独立线程 + current_thread 运行时
//!
//! 客户端只有一条事件循环，网络与音频都不需要第二个线程（音频回调由 cpal 自己起），
//! 顺带免了把 opus 句柄跨线程搬 —— 这与 `apps/client/src/main.rs` 的取舍完全一致。
//! Tauri 的主线程要跑窗口消息循环，所以这里必须是**另一个**线程，而不是 `spawn` 到
//! 某个已有的运行时上。
//!
//! # 三条流共用一个 `Channel<T>`
//!
//! 前端只订阅一次，按 `kind` 分派（`state` / `meters` / `net`）。用三条通道的话，
//! 前端要管三份生命周期，而它们的生命周期其实完全一样。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hushsquad_client::config::Config;
use hushsquad_client::host::{AccountView, Command, Host, MetersView, NetView, StateView};
use serde::Serialize;
use tauri::ipc::Channel;

/// 指令队列上限。界面上连点几下也就几条；攒到 64 条说明消费端已经卡住了，
/// 这时丢最旧的比无限增长好（静音这种指令只需要最后一条）。
const COMMAND_QUEUE: usize = 64;

/// 停会话时最多等多久。等不到就不等了 —— 卡在握手里的线程不能冻住界面。
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// 推给前端的一条消息。前端按 `kind` 分派。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// 低频状态（阶段 / 频道 / 成员 / 自己的静音闭听）。
    State(StateView),
    /// 20 Hz 电平。
    Meters(MetersView),
    /// 5 s 网络数字。
    Net(NetView),
    /// 账号列表（管理面板请求之后来一次）。
    ///
    /// **必须是结构体变体**：`#[serde(tag = "kind")]` 的内部标记无法序列化
    /// 「新类型变体里装一个序列」——那种写法会在发送时返回 Err，而调用方是
    /// `let _ = channel.send(..)`，于是消息**静默消失**（这个坑踩过一次）。
    Accounts {
        /// 全部账号（含离线的）。
        accounts: Vec<AccountView>,
    },
}

/// 会话与前端之间的桥：一个通道往外推，一个队列往里拉。
struct Bridge {
    events: Channel<Event>,
    commands: Mutex<VecDeque<Command>>,
    finished: AtomicBool,
}

impl Bridge {
    fn push(&self, command: Command) {
        let Ok(mut queue) = self.commands.lock() else {
            return;
        };
        if queue.len() >= COMMAND_QUEUE {
            queue.pop_front();
        }
        queue.push_back(command);
    }
}

/// 新类型：`Host` 是外部 trait、`Arc` 是外部类型，直接 `impl Host for Arc<Bridge>`
/// 会撞孤儿规则。包一层本地类型最省事，也不影响别处共享 `Arc`。
struct HostRef(Arc<Bridge>);

impl Host for HostRef {
    fn state(&self, state: &StateView) {
        // 前端窗口关了/正在导航时 `send` 会失败；这不是错误，丢掉就行。
        let _ = self.0.events.send(Event::State(state.clone()));
    }

    fn meters(&self, meters: &MetersView) {
        let _ = self.0.events.send(Event::Meters(meters.clone()));
    }

    fn net(&self, net: &NetView) {
        let _ = self.0.events.send(Event::Net(net.clone()));
    }

    fn accounts(&self, accounts: &[AccountView]) {
        let _ = self.0.events.send(Event::Accounts {
            accounts: accounts.to_vec(),
        });
    }

    fn command(&self) -> Option<Command> {
        self.0.commands.lock().ok()?.pop_front()
    }
}

/// 一条正在跑的会话。
pub struct Session {
    bridge: Arc<Bridge>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    /// 起一条会话。立即返回：握手在后台线程上做，进度经 `events` 推出去。
    pub fn start(cfg: Config, events: Channel<Event>) -> Self {
        let bridge = Arc::new(Bridge {
            events,
            commands: Mutex::new(VecDeque::new()),
            finished: AtomicBool::new(false),
        });
        let host = Arc::clone(&bridge);
        let watcher = Arc::clone(&bridge);
        let thread = thread::Builder::new()
            .name("hushsquad-session".to_owned())
            .spawn(move || {
                let outcome = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| format!("起运行时失败：{error}"))
                    .and_then(|runtime| {
                        runtime.block_on(hushsquad_client::run_with(
                            cfg,
                            Some(Box::new(HostRef(host))),
                        ))
                    });
                if let Err(error) = outcome {
                    // 界面上已经看到 `phase = error` 了；控制台这份是给开发者的。
                    eprintln!("会话结束：{error}");
                }
                watcher.finished.store(true, Ordering::Release);
            })
            .ok();
        Self { bridge, thread }
    }

    /// 给会话发一条指令。会话每个 tick（10 ms）取一次。
    pub fn send(&self, command: Command) {
        self.bridge.push(command);
    }

    /// 让会话退出，最多等 [`STOP_TIMEOUT`]。
    ///
    /// 等不到就返回 —— 一个卡在 TLS 握手里的线程不应该冻住界面；它自己会在超时后退出。
    pub fn stop(mut self) {
        self.bridge.push(Command::Disconnect);
        if let Some(thread) = self.thread.take() {
            let deadline = Instant::now() + STOP_TIMEOUT;
            while !self.bridge.finished.load(Ordering::Acquire) {
                if Instant::now() >= deadline {
                    // 不等了。线程会在自己的超时（握手 10 s）后收尾。
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
            let _ = thread.join();
        }
    }

    /// 会话线程是否已经退出。
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.bridge.finished.load(Ordering::Acquire)
    }
}
