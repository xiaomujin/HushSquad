//! 把「会分配的处理环节」挪到工作线程（P3-7）。
//!
//! # 为什么需要它
//!
//! DFN3 是唯一还违反 audio-core 实时纪律的环节：`DfTract::process` 每帧约 **185 次堆分配**
//! （P3-3 实测，1950 帧共 360 768 次，见 C-20）。它便宜（p95 0.383 ms，预算的 3.9 %），只是
//! 不能进音频回调。所以不是「换掉它」，而是「把它关进另一个线程」。
//!
//! ```text
//! 音频回调 ──input 环──▶ [工作线程: stage.process] ──output 环──▶ 音频回调
//!   （零分配）                                          （零分配）
//! ```
//!
//! # 固定占用 = 固定延迟 = 固定的容错时间
//!
//! 音频侧只在 `已送 - 已取 > depth` 时才取输出。于是：
//!
//! - **额外延迟恰好是 `depth` 帧**（10 ms × depth），不随线程调度抖动；
//! - **工作线程的容错时间也是 `depth` 帧**：第 k 帧要用第 k−depth 帧的结果，只要工作线程在
//!   `depth × 10 ms` 内算完就行。DFN3 的 p95 是 0.383 ms，depth = 2 就给了 52 倍余量。
//!
//! 这条规则是**故意不图快**的：如果允许「算完就取」，延迟会变成 0–1 帧且取决于调度，
//! 那既不可测也不可预算。延迟必须是常数，才配写进附录 A 的预算表。
//!
//! # 起播代价
//!
//! 头 `depth` 帧没有结果可给，输出静音并计入 `priming_frames`。这是队列延迟的**同一件事**
//! 的另一个说法，不是额外的 bug。
//!
//! # VAD 也是延迟的
//!
//! [`Stage::process`] 返回的是**这一帧输出**对应的 VAD，而这一帧的输入是 `depth` 帧之前进去的。
//! 所以 VAD 落后 `depth` 帧 + 环节自身延迟。用作「有没有人在说话」的指示够用（PTT 的开关不是
//! 靠它），但别拿它做逐帧门控。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use crate::{FRAME_SAMPLES, Frame, Stage};

/// 默认队列深度（帧）。2 帧 = 20 ms 额外延迟，换来 20 ms 的线程容错时间。
pub const DEFAULT_DEPTH: usize = 2;

/// 环里流动的一帧（带 VAD，省一次额外的队列）。
#[derive(Clone, Copy)]
struct Item {
    frame: Frame,
    vad: f32,
}

/// 音频侧与工作线程共享的计数（全原子，不加锁）。
#[derive(Debug, Default)]
pub struct WorkerStats {
    running: AtomicBool,
    /// 工作线程是否还活着。用 `Drop` 守卫维护，**panic 也会把它翻成 false**——
    /// 否则降噪线程死掉之后音频侧只会永远输出静音，最难查的那种静默故障。
    worker_alive: AtomicBool,
    frames_pushed: AtomicU64,
    frames_dropped: AtomicU64,
    frames_processed: AtomicU64,
    priming: AtomicU64,
    underruns: AtomicU64,
}

/// 一份可读的计数快照。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WorkerStatsSnapshot {
    /// 音频侧成功送进 input 环的帧数。
    pub frames_pushed: u64,
    /// 因为某个环满而丢掉的帧数（工作线程跟不上时才会发生）。
    pub frames_dropped: u64,
    /// 工作线程处理完的帧数。
    pub frames_processed: u64,
    /// 起播垫缓冲的帧数（应当等于 `depth`）。
    pub priming: u64,
    /// 垫完缓冲之后仍然取不到结果的帧数（工作线程真的掉队了）。
    pub underruns: u64,
}

impl WorkerStats {
    fn snapshot(&self) -> WorkerStatsSnapshot {
        WorkerStatsSnapshot {
            frames_pushed: self.frames_pushed.load(Ordering::Relaxed),
            frames_dropped: self.frames_dropped.load(Ordering::Relaxed),
            frames_processed: self.frames_processed.load(Ordering::Relaxed),
            priming: self.priming.load(Ordering::Relaxed),
            underruns: self.underruns.load(Ordering::Relaxed),
        }
    }
}

/// 音频侧：零分配地把帧交给工作线程。
pub struct WorkerStage {
    input: rtrb::Producer<Item>,
    output: rtrb::Consumer<Item>,
    shared: Arc<WorkerStats>,
    inner_latency: usize,
    depth: usize,
    pushed: u64,
    popped: u64,
}

/// 工作线程侧：真正跑那个会分配的环节。
pub struct StageWorker {
    input: rtrb::Consumer<Item>,
    output: rtrb::Producer<Item>,
    stage: Box<dyn Stage + Send>,
    shared: Arc<WorkerStats>,
}

impl WorkerStage {
    /// 构造一对（音频侧, 工作线程侧）。
    ///
    /// `depth` 同时是「额外延迟帧数」和「线程容错帧数」，见模块文档。传 0 会被夹到 1——
    /// 0 深度的队列等于没有队列，那只是把竞态藏起来。
    pub fn new(stage: Box<dyn Stage + Send>, depth: usize) -> (Self, StageWorker) {
        let depth = depth.max(1);
        let inner_latency = stage.latency_samples();
        // 两个环都留出 depth + 4：depth 是在途的，多出来的是抖动余量。
        let capacity = depth + 4;
        let (input, worker_input) = rtrb::RingBuffer::new(capacity);
        let (worker_output, output) = rtrb::RingBuffer::new(capacity);
        let shared = Arc::new(WorkerStats::default());
        shared.running.store(true, Ordering::Relaxed);
        (
            Self {
                input,
                output,
                shared: Arc::clone(&shared),
                inner_latency,
                depth,
                pushed: 0,
                popped: 0,
            },
            StageWorker {
                input: worker_input,
                output: worker_output,
                stage,
                shared,
            },
        )
    }

    /// 用默认深度构造。
    pub fn with_default_depth(stage: Box<dyn Stage + Send>) -> (Self, StageWorker) {
        Self::new(stage, DEFAULT_DEPTH)
    }

    /// 队列深度（帧）。
    #[must_use]
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// 计数快照。
    #[must_use]
    pub fn stats(&self) -> WorkerStatsSnapshot {
        self.shared.snapshot()
    }

    /// 工作线程是否还活着。
    ///
    /// 没 `spawn` 过时返回 `false`。上层应当把它接进自己的健康检查：线程死了就降级到
    /// 不降噪的档位，而不是继续喂一个没人消费的队列。
    #[must_use]
    pub fn is_worker_alive(&self) -> bool {
        self.shared.worker_alive.load(Ordering::Relaxed)
    }
}

impl Stage for WorkerStage {
    fn name(&self) -> &'static str {
        "worker"
    }

    fn latency_samples(&self) -> usize {
        // 环节自身的延迟 + 队列延迟。两者都进预算表，不能只记一个。
        self.inner_latency + self.depth * FRAME_SAMPLES
    }

    fn process(&mut self, frame: &mut Frame) -> f32 {
        // 1) 送入。环满 = 工作线程掉队，丢掉并计数——**绝不阻塞、绝不分配**。
        if self
            .input
            .push(Item {
                frame: *frame,
                vad: 1.0,
            })
            .is_ok()
        {
            self.shared.frames_pushed.fetch_add(1, Ordering::Relaxed);
        } else {
            self.shared.frames_dropped.fetch_add(1, Ordering::Relaxed);
        }
        self.pushed += 1;

        // 2) 取出。只有「已送 - 已取 > depth」时才取，延迟因此是常数。
        if self.pushed - self.popped > self.depth as u64 {
            if let Ok(item) = self.output.pop() {
                *frame = item.frame;
                self.popped += 1;
                return item.vad;
            }
            self.shared.underruns.fetch_add(1, Ordering::Relaxed);
        } else {
            self.shared.priming.fetch_add(1, Ordering::Relaxed);
        }

        // 3) 没有结果可给：静音 + 「无意见」的 VAD。
        frame.fill(0.0);
        1.0
    }
}

impl Drop for WorkerStage {
    fn drop(&mut self) {
        // 让工作线程退出。不 join：音频侧不该在回调里等一个线程收尾。
        self.shared.running.store(false, Ordering::Relaxed);
    }
}

impl StageWorker {
    /// 处理一帧（如果 input 环里有）。返回是否真的处理了。
    ///
    /// 测试直接调它，不碰线程：**行为完全确定**。
    pub fn run_once(&mut self) -> bool {
        let Ok(item) = self.input.pop() else {
            return false;
        };
        let mut frame = item.frame;
        let vad = self.stage.process(&mut frame);
        if self.output.push(Item { frame, vad }).is_ok() {
            self.shared.frames_processed.fetch_add(1, Ordering::Relaxed);
        } else {
            self.shared.frames_dropped.fetch_add(1, Ordering::Relaxed);
        }
        true
    }

    /// 把能处理的都处理掉，返回处理了几帧。
    pub fn run_pending(&mut self) -> usize {
        let mut count = 0;
        while self.run_once() {
            count += 1;
        }
        count
    }

    /// 起一个线程跑到底。音频侧被 drop 之后自动退出。
    ///
    /// 空闲时睡 1 ms：一帧 10 ms，1 ms 的轮询粒度只会让结果晚到最多 1 ms，而 `depth` 帧的
    /// 容错时间足够吸收它。忙等会白烧一个核。
    pub fn spawn(mut self) -> std::thread::JoinHandle<()> {
        self.shared.worker_alive.store(true, Ordering::Relaxed);
        std::thread::Builder::new()
            .name("hushsquad-dsp".to_owned())
            .spawn(move || {
                // 守卫在**正常退出和 panic 展开**时都会跑，所以「线程死了」一定能被看见。
                let _guard = ExitGuard(Arc::clone(&self.shared));
                while self.shared.running.load(Ordering::Relaxed) {
                    if !self.run_once() {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            })
            .expect("起 DSP 工作线程失败")
    }
}

/// 线程退出（含 panic）时把存活标记翻成 false。
struct ExitGuard(Arc<WorkerStats>);

impl Drop for ExitGuard {
    fn drop(&mut self) {
        self.0.worker_alive.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// 测试用的廉价环节：乘 2 并把 VAD 报成 0.25。不需要 DFN3 也能验证队列语义。
    pub(crate) struct Doubler;

    impl Stage for Doubler {
        fn name(&self) -> &'static str {
            "doubler"
        }

        fn latency_samples(&self) -> usize {
            96
        }

        fn process(&mut self, frame: &mut Frame) -> f32 {
            for sample in frame.iter_mut() {
                *sample *= 2.0;
            }
            0.25
        }
    }

    fn ramp(index: usize) -> Frame {
        let mut frame: Frame = [0.0; FRAME_SAMPLES];
        for (n, sample) in frame.iter_mut().enumerate() {
            *sample = (index * FRAME_SAMPLES + n) as f32 / 48_000.0;
        }
        frame
    }

    fn pair(depth: usize) -> (WorkerStage, StageWorker) {
        WorkerStage::new(Box::new(Doubler), depth)
    }

    #[test]
    fn latency_includes_the_queue_depth() {
        let (stage, _worker) = pair(3);
        assert_eq!(stage.depth(), 3);
        assert_eq!(
            stage.latency_samples(),
            96 + 3 * FRAME_SAMPLES,
            "队列延迟必须计入 latency_samples，否则预算表会少记 30 ms"
        );
    }

    #[test]
    fn zero_depth_is_clamped_up() {
        let (stage, _worker) = pair(0);
        assert_eq!(stage.depth(), 1, "0 深度的队列只是把竞态藏起来");
    }

    #[test]
    fn startup_is_priming_not_garbage() {
        let (mut stage, _worker) = pair(2);
        for index in 0..2 {
            let mut frame = ramp(index);
            let vad = stage.process(&mut frame);
            assert!(frame.iter().all(|x| *x == 0.0), "垫缓冲期间必须输出静音");
            assert_eq!(vad, 1.0, "没有结果时 VAD 报「无意见」");
        }
        let stats = stage.stats();
        assert_eq!(stats.priming, 2, "头两帧应当计入 priming");
        assert_eq!(stats.underruns, 0, "垫缓冲不算下溢");
    }

    #[test]
    fn output_is_exactly_depth_frames_behind() {
        // 手工驱动工作线程：行为完全确定，可以逐帧比对。
        let (mut stage, mut worker) = pair(2);
        let mut outputs: Vec<Frame> = Vec::new();
        for index in 0..10 {
            let mut frame = ramp(index);
            stage.process(&mut frame);
            outputs.push(frame);
            worker.run_pending();
        }
        for (index, out) in outputs.iter().enumerate().skip(2) {
            let expected = {
                let mut f = ramp(index - 2);
                for sample in f.iter_mut() {
                    *sample *= 2.0;
                }
                f
            };
            assert_eq!(
                out.as_slice(),
                expected.as_slice(),
                "第 {index} 帧的输出应当是第 {} 帧处理后的结果",
                index - 2
            );
        }
        assert_eq!(stage.stats().underruns, 0, "手工驱动不该有下溢");
    }

    #[test]
    fn vad_comes_back_with_the_frame() {
        let (mut stage, mut worker) = pair(1);
        let mut vads = Vec::new();
        for index in 0..4 {
            let mut frame = ramp(index);
            vads.push(stage.process(&mut frame));
            worker.run_pending();
        }
        assert_eq!(vads[0], 1.0, "第 0 帧还在垫缓冲");
        assert_eq!(vads[1..], [0.25, 0.25, 0.25], "之后应当拿到环节自己的 VAD");
    }

    #[test]
    fn stalled_worker_drops_frames_instead_of_blocking() {
        // 从不驱动工作线程：input 环会满，push 必须失败并计数，绝不阻塞、绝不 panic。
        let (mut stage, _worker) = pair(1);
        for index in 0..20 {
            let mut frame = ramp(index);
            stage.process(&mut frame);
        }
        let stats = stage.stats();
        assert!(stats.frames_dropped > 0, "环满却没计数");
        assert_eq!(stats.frames_processed, 0, "没驱动就不该有产出");
        assert!(stats.underruns > 0, "取不到结果必须计数");
    }

    struct Panicker;

    impl Stage for Panicker {
        fn name(&self) -> &'static str {
            "panicker"
        }

        fn latency_samples(&self) -> usize {
            0
        }

        fn process(&mut self, _frame: &mut Frame) -> f32 {
            panic!("这个环节故意炸");
        }
    }

    #[test]
    fn a_dead_worker_is_observable_instead_of_silent() {
        // 线程死掉（这里用 panic 模拟）之后，音频侧只会永远输出静音——那是最难查的静默故障。
        let (mut stage, worker) = WorkerStage::new(Box::new(Panicker), 1);
        assert!(!stage.is_worker_alive(), "没 spawn 过不该报存活");
        let handle = worker.spawn();
        assert!(stage.is_worker_alive(), "spawn 之后应当报存活");

        let mut frame = ramp(0);
        stage.process(&mut frame);
        // 等它真的炸掉（线程 panic 不会影响测试进程）。
        for _ in 0..100 {
            if !stage.is_worker_alive() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!stage.is_worker_alive(), "工作线程已经死了，却还报存活");
        assert!(handle.join().is_err(), "这个线程本来就该 panic 退出");
    }

    #[test]
    fn spawned_thread_keeps_up_with_realtime() {
        // 真的起线程：这里不追求逐位确定，只验证「工作线程跟得上」这条工程前提。
        let (mut stage, worker) = pair(2);
        let handle = worker.spawn();
        let mut produced = 0_usize;
        for index in 0..40 {
            let mut frame = ramp(index);
            stage.process(&mut frame);
            if frame.iter().any(|x| *x != 0.0) {
                produced += 1;
            }
            // 给工作线程 5 ms 的时间追上（真实场景有 20 ms）。
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(stage);
        handle.join().expect("工作线程应当正常退出");
        assert!(
            produced >= 30,
            "40 帧里只有 {produced} 帧拿到了结果，工作线程没跟上"
        );
    }
}
