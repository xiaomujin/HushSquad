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
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hushsquad_client::config::Config;
use hushsquad_client::host::{AccountView, Command, Host, MetersView, NetView, Phase, StateView};
use serde::Serialize;
use tauri::ipc::Channel;

/// 指令队列上限。界面上连点几下也就几条；攒到 64 条说明消费端已经卡住了，
/// 这时丢最旧的比无限增长好（静音这种指令只需要最后一条）。
const COMMAND_QUEUE: usize = 64;

/// 停会话时最多等多久。等不到就不等了 —— 卡在握手里的线程不能冻住界面。
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// 会话线程与外壳之间的观察点：**除了推给前端的事件**，还要留下「跑过哪些阶段」和
/// 「最后在哪个频道」。断线重连（B2）只靠它判断该不该重连、重连之后回哪个频道。
#[derive(Debug, Default)]
pub struct Watcher {
    /// 会话线程退出了吗。
    finished: AtomicBool,
    /// 这条会话**曾经**进过频道吗。它决定「掉线要不要重连」：
    /// 一次都没连上，说明是密码错了、证书变了、地址不通——重试只会把同一个错误
    /// 重复十遍，不如直接把原因摆给用户。
    reached: AtomicBool,
    /// 最后所在的频道 id（0 = 还不知道）。
    channel: AtomicU32,
    /// 最后一个阶段，只用于诊断。
    phase: AtomicU8,
}

impl Watcher {
    /// 会话线程退出了吗。
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    /// 这条会话曾经进过频道吗。
    #[must_use]
    pub fn reached_channel(&self) -> bool {
        self.reached.load(Ordering::Acquire)
    }

    /// 最后所在的频道 id。
    #[must_use]
    pub fn channel(&self) -> Option<u32> {
        match self.channel.load(Ordering::Acquire) {
            0 => None,
            id => Some(id),
        }
    }
}

/// 阶段编码成 `u8`：只为了存进原子量，不参与任何判断。
fn phase_code(phase: Phase) -> u8 {
    match phase {
        Phase::Disconnected => 0,
        Phase::Authenticating => 1,
        Phase::Connected => 2,
        Phase::Joining => 3,
        Phase::InChannel => 4,
        Phase::Reconnecting => 5,
        Phase::Error => 6,
    }
}

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
    /// 一条非致命提示（服务器拒绝了一次操作 / 请求超时）。
    ///
    /// 与 `phase = error` 分开：那条表示会话没了，这条表示「这件事没做成，还能继续用」。
    Notice {
        /// 给用户看的一句话。
        message: String,
    },
    /// 只有阶段、没有快照（断线重连时用）。
    ///
    /// 会话线程自己只会在「跑完 / 跑挂」时说一次话，而重连期间**没有会话**——
    /// 没有快照可推，但界面必须知道「正在重连」。所以单独一条。
    Phase {
        /// 阶段。
        phase: Phase,
        /// 只有 `error` 阶段才有。
        error: Option<String>,
    },
}

/// 会话与前端之间的桥：一个通道往外推，一个队列往里拉。
///
/// 「线程跑完了吗」不在这里，在 [`Watcher`]：重连监督线程要在**没有 Session 对象**
/// 的情况下也能看这条会话死没死。
struct Bridge {
    events: Channel<Event>,
    commands: Mutex<VecDeque<Command>>,
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
struct HostRef {
    bridge: Arc<Bridge>,
    watcher: Arc<Watcher>,
    signals: Arc<crate::supervisor::Signals>,
}

impl Host for HostRef {
    fn state(&self, state: &StateView) {
        // 观察点先更新：重连要用它，而推给前端的那份可能因为窗口关闭而失败。
        self.watcher
            .phase
            .store(phase_code(state.phase), Ordering::Release);
        if state.phase == Phase::InChannel {
            self.watcher.reached.store(true, Ordering::Release);
        }
        if state.self_channel != 0 {
            self.watcher
                .channel
                .store(state.self_channel, Ordering::Release);
        }
        // 重连期间，每一次尝试都会推一串中间状态（authenticating / error），
        // 而这些快照的成员表是**空的**——推到界面上就会把频道树和席位清空，
        // 看起来像「房间没了」。所以重连期间只放行 `in_channel` 那一条：
        // 它带着完整快照，界面一收到就恢复成真的。
        if self.signals.suppress_errors() && state.phase != Phase::InChannel {
            return;
        }
        // 已经进过频道的会话挂了：`announce` 带的是**空快照**，原样推下去会把频道树
        // 和席位清空，看起来像「房间没了」。所以只报阶段，不动房间里的东西。
        if state.phase == Phase::Error && self.watcher.reached_channel() {
            let _ = self.bridge.events.send(Event::Phase {
                phase: Phase::Error,
                error: state.error.clone(),
            });
            return;
        }
        // 前端窗口关了/正在导航时 `send` 会失败；这不是错误，丢掉就行。
        let _ = self.bridge.events.send(Event::State(state.clone()));
    }

    fn meters(&self, meters: &MetersView) {
        let _ = self.bridge.events.send(Event::Meters(meters.clone()));
    }

    fn net(&self, net: &NetView) {
        let _ = self.bridge.events.send(Event::Net(net.clone()));
    }

    fn accounts(&self, accounts: &[AccountView]) {
        let _ = self.bridge.events.send(Event::Accounts {
            accounts: accounts.to_vec(),
        });
    }

    fn notice(&self, message: &str) {
        let _ = self.bridge.events.send(Event::Notice {
            message: message.to_owned(),
        });
    }

    fn command(&self) -> Option<Command> {
        self.bridge.commands.lock().ok()?.pop_front()
    }
}

/// 一条正在跑的会话。
pub struct Session {
    bridge: Arc<Bridge>,
    watcher: Arc<Watcher>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    /// 起一条会话。立即返回：握手在后台线程上做，进度经 `events` 推出去。
    ///
    /// `app` 只用来在会话自己结束之后收尾（把全局按键注销掉）——会话可能是被服务器
    /// 关掉的，界面那边只会看到 `phase = error`，没人会记得来注销热键。
    pub fn start(
        cfg: Config,
        events: Channel<Event>,
        app: tauri::AppHandle,
        signals: Arc<crate::supervisor::Signals>,
    ) -> Self {
        let bridge = Arc::new(Bridge {
            events,
            commands: Mutex::new(VecDeque::new()),
        });
        let watcher = Arc::new(Watcher::default());
        let host = Arc::clone(&bridge);
        let watcher_for_host = Arc::clone(&watcher);
        let watcher_for_thread = Arc::clone(&watcher);
        let signals_for_host = Arc::clone(&signals);
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
                            Some(Box::new(HostRef {
                                bridge: host,
                                watcher: watcher_for_host,
                                signals: signals_for_host,
                            })),
                        ))
                    });
                if let Err(error) = outcome {
                    // 界面上已经看到 `phase = error` 了；控制台这份是给开发者的。
                    eprintln!("会话结束：{error}");
                }
                watcher_for_thread.finished.store(true, Ordering::Release);
                crate::ptt::on_session_finished(&app);
            })
            .ok();
        Self {
            bridge,
            watcher,
            thread,
        }
    }

    /// 这条会话的观察点（重连监督线程用）。
    #[must_use]
    pub fn watcher(&self) -> Arc<Watcher> {
        Arc::clone(&self.watcher)
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
            while !self.watcher.is_finished() {
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
        self.watcher.is_finished()
    }
}
