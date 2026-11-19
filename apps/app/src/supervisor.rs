//! 断线重连（B2）。
//!
//! # 为什么在**外壳**里做，而不是在 `apps/client` 里
//!
//! 命令行客户端是一次性工具：跑一段、报一张表、退出。「网络断了就自己接上」对它是
//! 多余的复杂度，而且会把「这次跑挂了」这件事藏起来。界面不一样——它可能挂着八小时，
//! 中途服务器重启一次、Wi-Fi 切一次，用户不该被踢回连接页。
//!
//! 所以重连 = **重起一条会话**：音频设备、TLS、语音面全部重建，界面留在房间里。
//! 代价是重连瞬间麦克风/扬声器会被关掉再打开（约几十到几百毫秒），换来的是
//! 「设备被拔掉又插回来」这种场景也能自愈——真在原进程里复用音频流反而做不到。
//!
//! # 什么时候重连，什么时候不
//!
//! 判据只有一条：**这条会话曾经进过频道吗**（[`Watcher::reached_channel`]）。
//!
//! - 进过 → 是掉线，重连（退避 0.5/1/2/4/8/16 秒，最多 10 次）；
//! - 没进过 → 是连不上：密码错、证书变了、地址不通。重试十次只是把同一个错误重复
//!   十遍，不如把原因摆给用户（会话自己已经推了 `phase = error`）。
//!
//! # 用户随时能打断
//!
//! 「断开」把 `stopping` 置上，监督线程下一次轮询就退出；「立即重试」把 `retry_now`
//! 置上，正在退避的那一觉会立刻醒。**新会话一开就把代次加一**，旧监督线程看到代次
//! 变了就自己退场——否则用户手动重连之后会有两条线程抢着起会话。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use hushsquad_client::config::Config;
use hushsquad_client::host::Phase;
use tauri::ipc::Channel;

use crate::ipc::AppState;
use crate::session::{Event, Session, Watcher};

/// 最多重连几次。10 次 × 退避（0.5…16 秒）≈ 一分半，够熬过服务器重启与 Wi-Fi 切换。
const MAX_ATTEMPTS: u32 = 10;
/// 轮询间隔：等会话结束、等用户打断，都用它。
const POLL: Duration = Duration::from_millis(100);

/// 第 `attempt` 次重连之前等多久（从 0 开始数）。
fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500u64 << attempt.min(5))
}

/// 起一个监督线程盯着这条会话。
pub fn watch(
    state: AppState,
    cfg: Config,
    events: Channel<Event>,
    first: Arc<Watcher>,
    app: tauri::AppHandle,
) {
    let generation = state.generation();
    let _ = thread::Builder::new()
        .name("hushsquad-reconnect".to_owned())
        .spawn(move || run(state, cfg, events, first, generation, app));
}

fn run(
    state: AppState,
    cfg: Config,
    events: Channel<Event>,
    mut watcher: Arc<Watcher>,
    generation: u64,
    app: tauri::AppHandle,
) {
    let mut attempt = 0;
    // 「这条会话链上有没有**成功过**」。只有成功过才值得重连：一次都没进过频道，
    // 说明是密码错、证书变了、地址不通——重试十次只是把同一个错误重复十遍。
    // 注意它必须是**跨尝试**的：重连的第一次尝试失败之后，`watcher` 已经换成新会话的
    // 那个了，它的 `reached_channel()` 当然是 false，拿它判断会让监督线程在第一次
    // 重连失败时就悄悄退场（这个坑真踩过，症状是「界面停在连接页，服务器起来也不回来」）。
    let mut ever_reached = false;
    loop {
        // ---- 1. 等这条会话结束 ----
        while !watcher.is_finished() {
            if stale(&state, generation) {
                return;
            }
            thread::sleep(POLL);
        }
        if stale(&state, generation) || state.is_stopping() {
            return;
        }
        ever_reached |= watcher.reached_channel();
        if !ever_reached {
            return;
        }

        // ---- 2. 退避 ----
        if attempt >= MAX_ATTEMPTS {
            state.signals.set_suppress_errors(false);
            let _ = events.send(Event::Phase {
                phase: Phase::Error,
                error: Some(format!(
                    "断了之后连续重连 {MAX_ATTEMPTS} 次都没成功，先停在这里。检查一下网络再手动连接。"
                )),
            });
            return;
        }
        let delay = backoff(attempt);
        attempt += 1;
        // 重连期间**压掉会话自己推的 error**：每一次尝试失败都会推一条，界面收到就会
        // 掉回连接页——而用户要的是「在房间里等它自己接上」。
        state.signals.set_suppress_errors(true);
        let _ = events.send(Event::Phase {
            phase: Phase::Reconnecting,
            error: None,
        });
        if !sleep_interruptible(&state, generation, delay) {
            state.signals.set_suppress_errors(false);
            return;
        }

        // ---- 3. 重起一条会话，回到原来的频道 ----
        // `Config.channel` 是「名字或 id」的字符串，纯数字就是 id——所以把上次的
        // 频道 id 直接写进去，改名也不影响（id 不变）。
        let mut next = cfg.clone();
        if let Some(channel) = watcher.channel() {
            next.channel = channel.to_string();
        }
        let session = Session::start(
            next,
            events.clone(),
            app.clone(),
            Arc::clone(&state.signals),
        );
        watcher = session.watcher();
        state.replace_session(session);

        // ---- 4. 等它进频道；进了就把次数清零（下一次掉线重新从 0.5 秒开始）----
        let deadline = Instant::now() + Duration::from_secs(20);
        while !watcher.is_finished() && !watcher.reached_channel() {
            if stale(&state, generation) || state.is_stopping() {
                state.signals.set_suppress_errors(false);
                return;
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(POLL);
        }
        if watcher.reached_channel() {
            attempt = 0;
            ever_reached = true;
            state.signals.set_suppress_errors(false);
        }
    }
}

/// 代次变了（用户手动连了别的服务器 / 又点了一次连接）：这条监督线程该退场了。
fn stale(state: &AppState, generation: u64) -> bool {
    state.generation() != generation
}

/// 睡一会儿，但用户随时能打断。返回 `false` 表示该退出了。
fn sleep_interruptible(state: &AppState, generation: u64, total: Duration) -> bool {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        if stale(state, generation) || state.is_stopping() {
            return false;
        }
        if state.take_retry_now() {
            return true;
        }
        thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
    !stale(state, generation) && !state.is_stopping()
}

/// 会话/连接相关的共享标志。放在 `AppState` 里（它已经是 `Clone` 的共享句柄）。
#[derive(Debug, Default)]
pub struct Signals {
    /// 每次「起一条新会话」加一。监督线程靠它判断自己是不是过期了。
    generation: AtomicU64,
    /// 用户要断开。
    stopping: AtomicBool,
    /// 用户点了「立即重试」：别等了，现在就连。
    retry_now: AtomicBool,
    /// 重连期间压掉会话自己推的 `phase = error`（见 `run` 里的说明）。
    suppress_errors: AtomicBool,
}

impl Signals {
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn bump(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    pub fn set_stopping(&self, stopping: bool) {
        self.stopping.store(stopping, Ordering::Release);
    }

    pub fn take_retry_now(&self) -> bool {
        self.retry_now.swap(false, Ordering::AcqRel)
    }

    pub fn request_retry(&self) {
        self.retry_now.store(true, Ordering::Release);
    }

    /// 重连期间要不要把会话自己推的错误吞掉。
    pub fn suppress_errors(&self) -> bool {
        self.suppress_errors.load(Ordering::Acquire)
    }

    pub fn set_suppress_errors(&self, suppress: bool) {
        self.suppress_errors.store(suppress, Ordering::Release);
    }
}
