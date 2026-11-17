//! 降噪档热切换：20 ms 交叉淡入（P3-4）。
//!
//! 目标是「换降噪器不爆音、不断音」。听上去只是两路加权平均，实际有三个坑，这个模块就是
//! 为它们存在的：
//!
//! 1. **两路的延迟不同**（RNNoise 10 ms、DFN3 30 ms、关掉降噪 0 ms）。直接把两路按权重相加，
//!    等于把同一句话的两个时间错开的版本混在一起——轻则梳状滤波（发闷），重则听见回声。
//!    所以先把**延迟短的那一路推迟到与长的一路对齐**（`target`），再混合。
//! 2. **新环节的流水线是空的**。DFN3 要喂满 30 ms 才开始出有意义的输出；切换瞬间直接接上去，
//!    前 30 ms 是静音 → 一次断音。所以切换时先用**输入历史预热**新环节（喂它过去 30 ms 的音频，
//!    输出直接丢弃并写入历史缓冲），这样它接手的第一个样本就是对齐的。
//! 3. **降延迟方向会丢音频**。从 DFN3 切回 RNNoise，如果立刻把 20 ms 的对齐延迟收掉，就得丢掉
//!    20 ms 音频（或做变速率滑移）。v1 选择**不回收**：对齐目标单调不减，宁可多背 20 ms 也不断音。
//!    回收留给 P4（需要变速率滑移，能在 200 ms 内悄悄吃掉 20 ms）。
//!
//! 交叉淡入用**线性**权重而不是等功率：两路信号高度相关（同一段音频，一路经过降噪），
//! 等功率会让中间点响 +3 dB。
//!
//! 这不是一个 [`crate::StageKind`]：它描述的是**运行期操作**，不是链路里的一档。

use crate::{FRAME_SAMPLES, Frame, Stage};

/// 交叉淡入长度：2 帧 = 20 ms（技术设计 D10 的验收条件）。
pub const CROSSFADE_FRAMES: usize = 2;

/// 对齐缓冲能覆盖的最大延迟差：DFN3 的 30 ms（1440 样本）是目前最长的一档。
pub const MAX_ALIGN_SAMPLES: usize = 1_440;

/// 输出历史的环形缓冲。容量比最大对齐再多留一帧，保证 `offset` 最大时也能读到有效样本。
#[derive(Debug)]
struct History {
    buf: Vec<f32>,
    pos: usize,
}

impl History {
    fn new() -> Self {
        Self {
            buf: vec![0.0; MAX_ALIGN_SAMPLES + FRAME_SAMPLES],
            pos: 0,
        }
    }

    /// 写入一个样本。
    fn push(&mut self, x: f32) {
        let capacity = self.buf.len();
        self.buf[self.pos] = x;
        self.pos = (self.pos + 1) % capacity;
    }

    /// 读 `offset` 个样本之前的写入值（0 = 刚写入的那个）。
    fn get(&self, offset: usize) -> f32 {
        let capacity = self.buf.len();
        let offset = offset.min(capacity - 1);
        self.buf[(self.pos + capacity - 1 - offset) % capacity]
    }
}

/// 正在淡入的下一档。
struct Incoming {
    stage: Box<dyn Stage>,
    frames_done: usize,
}

/// 两档之间的交叉淡入。
pub struct Crossfade {
    current: Box<dyn Stage>,
    incoming: Option<Incoming>,
    /// 对齐目标（样本）：单调不减，见模块文档第 3 条。
    target: usize,
    /// 进入本环节的输入历史（预热新环节用）。
    hist_input: History,
    /// 两路输出历史（对齐读取用）。
    hist_current: History,
    hist_incoming: History,
}

impl Crossfade {
    /// 从一档开始。
    #[must_use]
    pub fn new(current: Box<dyn Stage>) -> Self {
        let target = current.latency_samples();
        Self {
            current,
            incoming: None,
            target,
            hist_input: History::new(),
            hist_current: History::new(),
            hist_incoming: History::new(),
        }
    }

    /// 当前档的名字（报告与日志用）。
    #[must_use]
    pub fn current_name(&self) -> &'static str {
        self.current.name()
    }

    /// 是否正在淡入。
    #[must_use]
    pub fn is_switching(&self) -> bool {
        self.incoming.is_some()
    }

    /// 开始切换到 `next`。
    ///
    /// 这里会做两件事，都在调用线程上同步完成（**不要在音频回调里调用**）：
    /// 1. 抬高对齐目标到两路的较大延迟；
    /// 2. 用输入历史预热 `next`，把它的输出灌进历史缓冲，使它在第一帧就能对齐输出。
    pub fn begin(&mut self, mut next: Box<dyn Stage>) {
        if self.incoming.is_some() {
            // 上一轮还没切完：直接顶掉，用当前这一档做基准（不叠加两层淡入）。
            self.incoming = None;
        }
        let lat_current = self.current.latency_samples();
        let lat_next = next.latency_samples();
        self.target = self.target.max(lat_current).max(lat_next);

        // 预热：喂 `ceil(target / 480)` 帧历史输入，把输出写进 hist_incoming。
        let prime_frames = self.target.div_ceil(FRAME_SAMPLES);
        self.hist_incoming = History::new();
        for frame_index in (0..prime_frames).rev() {
            let mut frame: Frame = [0.0; FRAME_SAMPLES];
            for (i, sample) in frame.iter_mut().enumerate() {
                *sample = self
                    .hist_input
                    .get((frame_index + 1) * FRAME_SAMPLES - 1 - i);
            }
            let _ = next.process(&mut frame);
            for &sample in &frame {
                self.hist_incoming.push(sample);
            }
        }

        self.incoming = Some(Incoming {
            stage: next,
            frames_done: 0,
        });
    }
}

impl Stage for Crossfade {
    fn name(&self) -> &'static str {
        "crossfade"
    }

    fn latency_samples(&self) -> usize {
        self.target
    }

    fn process(&mut self, frame: &mut Frame) -> f32 {
        // 输入要先留一份：历史（`begin` 预热要用）和「新环节处理同一帧输入」都需要它。
        let input: Frame = *frame;
        for &sample in input.iter() {
            self.hist_input.push(sample);
        }

        let offset_current = self.target.saturating_sub(self.current.latency_samples());

        let vad_current = self.current.process(frame);
        for &sample in frame.iter() {
            self.hist_current.push(sample);
        }

        // 按对齐偏移读当前这一路：刚推进去的这一帧是「现在」，偏移 d 就是 d 个样本之前。
        let mut mixed: Frame = [0.0; FRAME_SAMPLES];
        for (i, sample) in mixed.iter_mut().enumerate() {
            *sample = self
                .hist_current
                .get(offset_current + FRAME_SAMPLES - 1 - i);
        }

        if self.incoming.is_none() {
            *frame = mixed;
            return vad_current;
        }

        let finished = {
            let incoming = self.incoming.as_mut().expect("刚判断过存在");
            let offset_incoming = self.target.saturating_sub(incoming.stage.latency_samples());

            let mut incoming_frame = input;
            let _ = incoming.stage.process(&mut incoming_frame);
            for &sample in incoming_frame.iter() {
                self.hist_incoming.push(sample);
            }

            // 逐样本线性淡入，整段窗口 2 帧 = 960 样本。
            let total = CROSSFADE_FRAMES * FRAME_SAMPLES;
            let start = incoming.frames_done * FRAME_SAMPLES;
            for (i, sample) in frame.iter_mut().enumerate() {
                let weight = ((start + i + 1) as f32 / total as f32).min(1.0);
                let to = self
                    .hist_incoming
                    .get(offset_incoming + FRAME_SAMPLES - 1 - i);
                *sample = mixed[i] * (1.0 - weight) + to * weight;
            }

            incoming.frames_done += 1;
            incoming.frames_done >= CROSSFADE_FRAMES
        };

        if finished {
            // 淡入结束：新档转正，历史缓冲互换（新档的输出历史已经在 hist_incoming 里）。
            let incoming = self.incoming.take().expect("刚判断过存在");
            self.current = incoming.stage;
            std::mem::swap(&mut self.hist_current, &mut self.hist_incoming);
        }

        // VAD 取当前（淡入期间仍占主体的）那一档。
        vad_current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 直通。
    struct Pass;

    impl Stage for Pass {
        fn name(&self) -> &'static str {
            "pass"
        }
        fn latency_samples(&self) -> usize {
            0
        }
        fn process(&mut self, _frame: &mut Frame) -> f32 {
            1.0
        }
    }

    /// 固定增益。
    struct Gain(f32);

    impl Stage for Gain {
        fn name(&self) -> &'static str {
            "gain"
        }
        fn latency_samples(&self) -> usize {
            0
        }
        fn process(&mut self, frame: &mut Frame) -> f32 {
            for sample in frame.iter_mut() {
                *sample *= self.0;
            }
            1.0
        }
    }

    /// 固定延迟（模拟「延迟更高的一档」）。
    struct Delay {
        samples: usize,
        history: Vec<f32>,
    }

    impl Delay {
        fn new(samples: usize) -> Self {
            Self {
                samples,
                history: vec![0.0; samples],
            }
        }
    }

    impl Stage for Delay {
        fn name(&self) -> &'static str {
            "delay"
        }
        fn latency_samples(&self) -> usize {
            self.samples
        }
        fn process(&mut self, frame: &mut Frame) -> f32 {
            for sample in frame.iter_mut() {
                self.history.push(*sample);
                *sample = self.history.remove(0);
            }
            1.0
        }
    }

    fn run(stage: &mut Crossfade, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::with_capacity(input.len());
        for chunk in input.as_chunks::<FRAME_SAMPLES>().0 {
            let mut frame: Frame = [0.0; FRAME_SAMPLES];
            frame.copy_from_slice(chunk);
            stage.process(&mut frame);
            out.extend_from_slice(&frame);
        }
        out
    }

    fn max_step(xs: &[f32]) -> f32 {
        xs.windows(2)
            .fold(0.0_f32, |acc, pair| acc.max((pair[1] - pair[0]).abs()))
    }

    #[test]
    fn equal_latency_switch_has_no_step() {
        // 直通 → 0.5 增益：线性淡入，任何相邻样本之间都不该出现跳变。
        let input: Vec<f32> = (0..10 * FRAME_SAMPLES)
            .map(|i| (i as f32 * 0.05).sin())
            .collect();
        let mut stage = Crossfade::new(Box::new(Pass));
        // 前 3 帧只跑直通，然后开始切换。
        let head = run(&mut stage, &input[..3 * FRAME_SAMPLES]);
        stage.begin(Box::new(Gain(0.5)));
        assert!(stage.is_switching());
        let tail = run(&mut stage, &input[3 * FRAME_SAMPLES..]);
        let mut all = head;
        all.extend_from_slice(&tail);

        // 输入自身的最大步长（正弦斜率）+ 淡入带来的额外变化，不应出现「台阶」。
        let input_step = max_step(&input);
        let output_step = max_step(&all);
        assert!(
            output_step <= input_step * 1.5 + 1e-6,
            "切换处出现台阶：输出最大步长 {output_step}，输入 {input_step}"
        );
        assert!(!stage.is_switching(), "20 ms 后应已切完");
        assert_eq!(stage.current_name(), "gain");
    }

    #[test]
    fn switch_finishes_after_two_frames() {
        let mut stage = Crossfade::new(Box::new(Pass));
        stage.begin(Box::new(Gain(0.0)));
        let mut frame: Frame = [1.0; FRAME_SAMPLES];
        stage.process(&mut frame);
        assert!(stage.is_switching(), "第一帧后还在淡入");
        let mut frame: Frame = [1.0; FRAME_SAMPLES];
        stage.process(&mut frame);
        assert!(!stage.is_switching(), "两帧（20 ms）后应切完");
        // 切完之后输出必须完全是新档的结果（增益 0 → 全零）。
        let mut frame: Frame = [1.0; FRAME_SAMPLES];
        stage.process(&mut frame);
        assert!(frame.iter().all(|x| *x == 0.0), "切完后仍有旧档成分");
    }

    #[test]
    fn mismatched_latency_is_aligned() {
        // 直通 → 10 ms 延迟。对齐后总延迟 = 480 样本，所以「0 变 1」的阶跃必须只出现一次，
        // 且不出现 0→0.5→1 的双台阶（那说明两路没有对齐，混出了回声）。
        let mut stage = Crossfade::new(Box::new(Pass));
        let input = vec![1.0_f32; 6 * FRAME_SAMPLES];
        // 先喂 3 帧，让直通路径的输出历史里有内容。
        let head = run(&mut stage, &input[..3 * FRAME_SAMPLES]);
        assert!(head.iter().all(|x| *x == 1.0), "直通路径应原样输出");

        stage.begin(Box::new(Delay::new(FRAME_SAMPLES)));
        assert_eq!(
            stage.latency_samples(),
            FRAME_SAMPLES,
            "延迟应对齐到较大的一档"
        );
        let tail = run(&mut stage, &input[3 * FRAME_SAMPLES..]);

        // 对齐正确时，两路在同一时刻都给出 1.0，混合结果仍是 1.0（没有中间值）。
        let intermediates = tail.iter().filter(|x| **x > 0.01 && **x < 0.99).count();
        assert_eq!(intermediates, 0, "出现中间值，说明两路没对齐");
    }

    #[test]
    fn latency_target_never_shrinks() {
        // 从高延迟档切回低延迟档：v1 不回收对齐延迟（回收要变速率滑移，见模块文档）。
        let mut stage = Crossfade::new(Box::new(Delay::new(FRAME_SAMPLES)));
        assert_eq!(stage.latency_samples(), FRAME_SAMPLES);
        stage.begin(Box::new(Pass));
        assert_eq!(
            stage.latency_samples(),
            FRAME_SAMPLES,
            "对齐目标不该因为切到低延迟档而变小"
        );
        let mut frame: Frame = [1.0; FRAME_SAMPLES];
        stage.process(&mut frame);
        let mut frame: Frame = [1.0; FRAME_SAMPLES];
        stage.process(&mut frame);
        assert_eq!(stage.latency_samples(), FRAME_SAMPLES);
    }

    #[test]
    fn priming_fills_the_incoming_pipeline() {
        // 新环节被历史输入预热过，所以切换后的第一帧就有真实输出，不是静音。
        let mut stage = Crossfade::new(Box::new(Pass));
        let input = vec![1.0_f32; 4 * FRAME_SAMPLES];
        let _ = run(&mut stage, &input[..3 * FRAME_SAMPLES]);
        stage.begin(Box::new(Delay::new(FRAME_SAMPLES)));
        let mut frame: Frame = [1.0; FRAME_SAMPLES];
        stage.process(&mut frame);
        // 淡入第一帧的权重从 1/960 开始，输出应当已经接近 1.0（两路都是 1.0），而不是 0。
        assert!(
            frame[FRAME_SAMPLES - 1] > 0.9,
            "预热没生效，切换后第一帧末尾只有 {}",
            frame[FRAME_SAMPLES - 1]
        );
    }
}
