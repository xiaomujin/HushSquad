//! 漂移补偿（P4-6 / 对策 D）：把时钟漂移从抖动缓冲里拿出来自己管。
//!
//! # 为什么非做不可
//!
//! P4-4 量出来的事实（C-30/C-31）：`neteq` 的压缩阈值是 `max(target, low_limit + 20 ms)`，
//! 那个 **+20 ms 是常数**，所以 20 ms 的 target 下它要等缓冲涨到 35 ms 才动手；反向的预拉伸
//! 还额外要求「当前缓冲 ≥ 3 帧 = 30 ms」，在 20 ms 预算里**永不成立**。于是 +100 ppm 稳在
//! 36.76 ms（多付 17 ms），−100 ppm 一次预拉伸都没有、靠 PLC 补了 1.44 s。
//!
//! # 执行器只有一个：拉包次数
//!
//! 这条推导必须写下来，否则很容易做出一个「看起来对但根本不生效」的补偿器：
//!
//! - 抖动缓冲深度 `B` 每 tick 的变化量 = `480(1+ε) − 480×pulls`，`ε` 是漂移、`pulls` 是本 tick
//!   拉了几帧。也就是说 **B 只受「拉几帧」影响**——丢样本、补样本、改重采样比，对 B 一点作用
//!   都没有。
//! - 音频侧每个 tick 固定取走 480 个样本，所以我们每 tick 也必须**写进** 480 个样本。
//!
//! 两条合起来给出唯一可行的结构：**丢/补样本改变的是「总延迟」的另一半**，而「拉几帧」改变
//! 的是 B 与环水位之间的**分配**。于是：
//!
//! - 抖动缓冲深度的误差 → 每 tick 该丢（或补）多少样本 `slip`；
//! - `slip` 累积成「拉包信用」：累计丢了 480 个样本就**多拉一帧**，累计补了 480 就**少拉一帧**。
//!
//! 稳态下 `slip = 480ε`，多拉的帧数正好等于丢掉的样本数，环水位不变，而 B 被钉在目标上。
//! 这就是把 `neteq` 的 accelerate/expand 搬到我们自己的、正确的**工作点**上。
//!
//! # 为什么不是重采样比微调
//!
//! 技术设计 §6.6 与执行方案 §11 的对策 D 原话是「按缓冲深度微调重采样比」。**读完上面那条推导
//! 之后改成了丢/补样本**：重采样比只改变「我们消费 NetEQ 输出的速率」，而 B 只受拉包次数影响，
//! 所以重采样同样需要「拉包信用」这一层——它比丢/补多付了插值延迟、CPU 与每帧分配，换来的只是
//! 「拼接更平滑」。而拼接的代价可以用 1.33 ms 等功率交叉淡化压到测不出来（P4-4 的 `max_step`
//! 判据会盯着它）。同样的活，代价更小，所以选它。
//!
//! # 拼接口径
//!
//! 丢 `d` 个样本：在 `X` 处把 `in[X..X+F]` 淡出、`in[X+d..X+d+F]` 淡入（等功率），输出短 `d`。
//! 补 `m` 个样本：把信号在 `X` 处**倒回** `m` 个样本（等于重放 `in[X-m..X]`），再用同样长度的
//! 交叉淡化接回 `in[X-m+F..]`，输出长 `m`。`F = 64`（1.33 ms）足够盖住 1 ms 级的时间跳变。

use crate::{FRAME_SAMPLES, Frame};

/// 抖动缓冲里**排队包数**的目标。
///
/// 为什么用包数而不是 `buffered_samples`：后者把 `leftover_samples` 与
/// `leftover_time_stretched_samples` 也算进去了，而这两个在 NetEQ 做时间伸缩时会自己涨——
/// 拿它当反馈会形成正反馈（实测一路跑到 10 % 的时间压缩、然后全程 PLC）。包数不会。
///
/// **口径必须和采样时刻对齐**：补偿器在「收包之后、拉包之前」看队列，而无漂移时实测那个时刻
/// 恒为 2（= 缓冲里 1 包 + 本 tick 刚到的 1 包）。所以目标写 2.0，对应真实排队 1 包 = 10 ms。
/// 写 1.0 会让它一直以为「太深」而满速丢样本——实测 10 % 时间压缩 + 全程 PLC。
pub const DEFAULT_TARGET_PACKETS: f32 = 2.0;

/// 深度滤波的 EMA 系数。抖动缓冲深度按包跳变（±480 一次），不滤会带着补偿器乱跳。
const EMA_ALPHA: f32 = 0.05;

/// 比例增益：排队包数的误差 → 本 tick 该丢的样本数。
///
/// 稳态误差 = `ε / KP` 个包，ε = 100 ppm 时约 0.0002 个包（0.1 个样本）——是「钉住」
/// 而不是「跟随」。
const KP: f32 = 0.02;

/// 死区（单位：包）。误差在死区内时不做任何拼接。
///
/// 队列是按整包计量的，采样点上天然有 ±1 包的跳变；没有死区的话，滤波后的残余纹波会让
/// 补偿器每 tick 都做一次 ±1 样本的拼接——实测 4 小时里有 24 % 的样本流过拼接点，
/// 白白把 `max_step` 抬到平滑上界的 4 倍。
const DEADBAND_PACKETS: f32 = 0.1;

/// 每 tick 最多丢/补多少样本。4 个样本 = 0.083 ms，即最多 0.83 % 的瞬时压缩率。
///
/// 这个上限同时决定「拼接有多狠」：1 个样本的拼接在 64 样本交叉淡化下几乎透明，
/// 48 个样本（1 ms）就会听成「咯」一下——P4-4 的 `max_step` 判据实测能抓到。
const MAX_SLIP: i32 = 2;

/// 交叉淡化长度（样本）。1.33 ms。
const CROSSFADE: usize = 64;

/// 拉包信用的阈值：累计 480 个样本就多拉/少拉一帧。
const CREDIT_LIMIT: i32 = FRAME_SAMPLES as i32;

/// 输出缓冲的上限：两帧加最多一次插入。`ReceivePath` 按这个尺寸备缓冲。
pub const MAX_OUTPUT_SAMPLES: usize = 2 * FRAME_SAMPLES + MAX_SLIP as usize;

/// 补偿器配置。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DriftConfig {
    /// 抖动缓冲里排队包数的目标。
    pub target_packets: f32,
    /// 关掉就是完全直通（逐样本一致），用于回归对照。
    pub enabled: bool,
}

impl Default for DriftConfig {
    fn default() -> Self {
        Self {
            target_packets: DEFAULT_TARGET_PACKETS,
            enabled: true,
        }
    }
}

/// 本 tick 该从抖动缓冲拉几帧。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullPlan {
    /// 正常一帧。
    One,
    /// 多拉一帧（把缓冲吃下去）。
    Two,
    /// 本 tick 不拉（让缓冲回血），用上一帧回放顶上。
    Zero,
}

/// 补偿器的累计统计。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DriftStats {
    /// 被丢掉的样本数。
    pub excised_samples: u64,
    /// 被补进去的样本数。
    pub inserted_samples: u64,
    /// 丢样本的事件数。
    pub excise_events: u64,
    /// 补样本的事件数。
    pub insert_events: u64,
    /// 多拉帧的次数。
    pub extra_pulls: u64,
    /// 少拉帧的次数。
    pub skipped_pulls: u64,
    /// 估计的环水位（样本）。诊断用：稳态下它应当是一条平线。
    pub ring_level: i64,
}

/// 漂移补偿器（网络线程侧，零分配）。
pub struct DriftControl {
    config: DriftConfig,
    filtered: f32,
    primed: bool,
    /// 带小数的「本 tick 该丢多少样本」。整数部分才是这一 tick 真正丢的。
    ///
    /// 必须有这个累加器：100 ppm 下稳态只需每 tick 丢 **0.048** 个样本，直接取整会永远是 0，
    /// 补偿器就完全不动了。
    accum: f32,
    slip: i32,
    credit: i32,
    history: Frame,
    has_history: bool,
    stats: DriftStats,
}

/// 等功率权重：`i` 从 0 到 `CROSSFADE-1`，返回 `(淡出, 淡入)`。
fn weights(i: usize) -> (f32, f32) {
    let theta = std::f32::consts::FRAC_PI_2 * (i as f32 + 0.5) / CROSSFADE as f32;
    (theta.cos(), theta.sin())
}

/// 在 `input` 上做一次丢（`slip > 0`）或补（`slip < 0`）的拼接，返回输出长度。
///
/// `out` 至少要有 `input.len() + MAX_SLIP` 个位置。
fn splice(input: &[f32], slip: i32, out: &mut [f32]) -> usize {
    let n = input.len();
    if slip == 0 || n < CROSSFADE * 2 {
        let len = n.min(out.len());
        out[..len].copy_from_slice(&input[..len]);
        return len;
    }
    if slip > 0 {
        // 丢 d 个：在 X 处把 in[X..X+F] 淡出、in[X+d..X+d+F] 淡入。
        let d = (slip as usize)
            .min(n - CROSSFADE)
            .min(out.len() - (n - slip as usize));
        if d == 0 {
            out[..n].copy_from_slice(input);
            return n;
        }
        let x = (n - d - CROSSFADE) / 2;
        out[..x].copy_from_slice(&input[..x]);
        for i in 0..CROSSFADE {
            let (fade_out, fade_in) = weights(i);
            out[x + i] = input[x + i] * fade_out + input[x + d + i] * fade_in;
        }
        let tail_src = x + d + CROSSFADE;
        let tail_dst = x + CROSSFADE;
        let tail = n - tail_src;
        out[tail_dst..tail_dst + tail].copy_from_slice(&input[tail_src..]);
        tail_dst + tail
    } else {
        // 补 m 个：在 X 处倒回 m 个样本（等于重放 in[X-m..X]），再交叉淡化接回。
        let m = ((-slip) as usize).min(n / 4).min(out.len() - n);
        if m == 0 {
            out[..n].copy_from_slice(input);
            return n;
        }
        let x = ((n - CROSSFADE) / 2).max(m);
        out[..x].copy_from_slice(&input[..x]);
        for i in 0..CROSSFADE {
            let (fade_out, fade_in) = weights(i);
            out[x + i] = input[x + i] * fade_in + input[x - m + i] * fade_out;
        }
        // 尾巴要从**倒回之后**的位置接：`x - m + F`，这样总长正好是 n + m。
        let tail_src = x - m + CROSSFADE;
        let tail = n - tail_src;
        let tail_dst = x + CROSSFADE;
        out[tail_dst..tail_dst + tail].copy_from_slice(&input[tail_src..]);
        tail_dst + tail
    }
}

impl DriftControl {
    /// 构造。
    #[must_use]
    pub fn new(config: DriftConfig) -> Self {
        Self {
            config,
            filtered: 0.0,
            primed: false,
            accum: 0.0,
            slip: 0,
            credit: 0,
            history: [0.0; FRAME_SAMPLES],
            has_history: false,
            // 环水位从 0 开始记账；给一帧垫子，否则第一个 tick 就判成「见底」而去多拉。
            stats: DriftStats {
                ring_level: FRAME_SAMPLES as i64,
                ..DriftStats::default()
            },
        }
    }

    /// 每 tick 开头调一次：喂当前排队包数，返回本 tick 该拉几帧。
    pub fn begin_tick(&mut self, queued_packets: u32) -> PullPlan {
        if !self.config.enabled {
            self.slip = 0;
            return PullPlan::One;
        }
        let depth = queued_packets as f32;
        if self.primed {
            self.filtered += (depth - self.filtered) * EMA_ALPHA;
        } else {
            self.filtered = depth;
            self.primed = true;
        }

        let mut error_packets = self.filtered - self.config.target_packets;
        if error_packets.abs() < DEADBAND_PACKETS {
            error_packets = 0.0;
        }
        self.accum += error_packets * FRAME_SAMPLES as f32 * KP;
        let limit = MAX_SLIP as f32 * 8.0;
        self.accum = self.accum.clamp(-limit, limit);
        let mut slip = self
            .accum
            .trunc()
            .clamp(-(MAX_SLIP as f32), MAX_SLIP as f32) as i32;
        // 最后的保险：环里几乎没货时不许再丢样本，否则音频侧当场饿。
        //
        // 门槛故意设得很低（8 个样本）而不是一帧：设成一帧会让「环正好在一帧」时丢掉样本被
        // 禁止，而抬升环水位又要靠多拉、多拉又要靠丢样本的信用——直接死锁。
        if slip > 0 && self.stats.ring_level < 2 * MAX_SLIP as i64 {
            slip = 0;
        }
        self.accum -= slip as f32;
        self.slip = slip;

        // 拉包次数：**信用为主，环水位为界**。
        //
        // 只用信用（slip 的积分）在 4 小时长跑里会把环慢慢撑到溢出——两套记账的误差会累积；
        // 只看环水位又会丢掉「漂移该拉多少」这个信息，队列会一路涨。两个都要：信用决定
        // 平均拉包率，环水位只做**硬边界**（见底就补一帧、超过两帧就少拉一帧）。
        self.credit += self.slip;
        let low = FRAME_SAMPLES as i64;
        let high = 2 * FRAME_SAMPLES as i64;
        let mut plan = PullPlan::One;
        if self.credit >= CREDIT_LIMIT {
            self.credit -= CREDIT_LIMIT;
            plan = PullPlan::Two;
        } else if self.credit <= -CREDIT_LIMIT && self.has_history {
            self.credit += CREDIT_LIMIT;
            plan = PullPlan::Zero;
        } else if self.stats.ring_level < low {
            self.credit -= CREDIT_LIMIT;
            plan = PullPlan::Two;
        } else if self.stats.ring_level > high && self.has_history {
            self.credit += CREDIT_LIMIT;
            plan = PullPlan::Zero;
        }
        match plan {
            PullPlan::Two => self.stats.extra_pulls += 1,
            PullPlan::Zero => self.stats.skipped_pulls += 1,
            PullPlan::One => {}
        }
        plan
    }

    /// 把本 tick 取到的样本（一帧、两帧，或空 = 没拉）加工成要写进 PCM 环的样本，返回长度。
    pub fn process(&mut self, input: &[f32], out: &mut [f32]) -> usize {
        let mut scratch = [0.0_f32; FRAME_SAMPLES];
        let source: &[f32] = if input.is_empty() {
            if !self.has_history {
                return 0;
            }
            scratch.copy_from_slice(&self.history);
            &scratch
        } else {
            input
        };

        let slip = if self.config.enabled { self.slip } else { 0 };
        let len = splice(source, slip, out);

        if source.len() >= FRAME_SAMPLES {
            let start = source.len() - FRAME_SAMPLES;
            self.history.copy_from_slice(&source[start..]);
            self.has_history = true;
        }

        self.stats.ring_level += len as i64 - FRAME_SAMPLES as i64;
        if slip > 0 {
            self.stats.excised_samples += slip as u64;
            self.stats.excise_events += 1;
        } else if slip < 0 {
            self.stats.inserted_samples += (-slip) as u64;
            self.stats.insert_events += 1;
        }
        len
    }

    /// 统计快照。
    #[must_use]
    pub fn stats(&self) -> DriftStats {
        self.stats
    }

    /// 当前滤波后的深度估计（样本）。诊断用。
    #[must_use]
    pub fn filtered_samples(&self) -> f32 {
        self.filtered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize) -> Vec<f32> {
        (0..frames * FRAME_SAMPLES)
            .map(|n| {
                let t = n as f32 / 48_000.0;
                0.25 * (std::f32::consts::TAU * 1_000.0 * t).sin()
            })
            .collect()
    }

    /// 平滑正弦的最大一阶差分：2A·sin(πf/fs)。
    fn smooth_bound() -> f32 {
        2.0 * 0.25 * (std::f32::consts::PI * 1_000.0 / 48_000.0).sin()
    }

    fn max_step(samples: &[f32]) -> f32 {
        samples
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0_f32, f32::max)
    }

    #[test]
    fn disabled_is_bit_exact() {
        let mut control = DriftControl::new(DriftConfig {
            enabled: false,
            ..DriftConfig::default()
        });
        let input = tone(1);
        let mut out = [0.0_f32; MAX_OUTPUT_SAMPLES];
        for _ in 0..5 {
            assert_eq!(control.begin_tick(99), PullPlan::One);
            let len = control.process(&input, &mut out);
            assert_eq!(len, FRAME_SAMPLES);
            assert_eq!(&out[..len], &input[..]);
        }
        assert_eq!(control.stats().excised_samples, 0);
        assert_eq!(control.stats().inserted_samples, 0);
    }

    #[test]
    fn excising_shortens_and_inserting_lengthens() {
        let input = tone(1);
        let mut out = [0.0_f32; MAX_OUTPUT_SAMPLES];
        for (slip, expected) in [(1_i32, FRAME_SAMPLES - 1), (4, FRAME_SAMPLES - 4)] {
            let len = splice(&input, slip, &mut out);
            assert_eq!(len, expected, "丢 {slip} 个样本的输出长度不对");
        }
        for (slip, expected) in [(-1_i32, FRAME_SAMPLES + 1), (-4, FRAME_SAMPLES + 4)] {
            let len = splice(&input, slip, &mut out);
            assert_eq!(len, expected, "补 {} 个样本的输出长度不对", -slip);
        }
        let len = splice(&input, 0, &mut out);
        assert_eq!(len, FRAME_SAMPLES);
        assert_eq!(&out[..len], &input[..], "不丢不补必须逐样本一致");
    }

    #[test]
    fn a_splice_keeps_the_waveform_continuous() {
        // 这是「不爆音」的机器判据：拼接后的一阶差分不能超过平滑正弦上界的若干倍。
        let input = tone(1);
        let bound = smooth_bound();
        let mut out = [0.0_f32; MAX_OUTPUT_SAMPLES];
        for slip in [-MAX_SLIP, -2, -1, 1, 2, MAX_SLIP] {
            let len = splice(&input, slip, &mut out);
            let step = max_step(&out[..len]);
            assert!(
                step < 3.0 * bound,
                "slip = {slip} 的拼接撕裂了波形：一阶差分 {step:.4} vs 平滑上界 {bound:.4}"
            );
        }
    }

    #[test]
    fn a_queue_at_target_does_nothing() {
        let mut control = DriftControl::new(DriftConfig::default());
        let input = tone(1);
        let mut out = [0.0_f32; MAX_OUTPUT_SAMPLES];
        for _ in 0..500 {
            let plan = control.begin_tick(DEFAULT_TARGET_PACKETS as u32);
            assert_eq!(plan, PullPlan::One, "目标深度上不该多拉或少拉");
            let len = control.process(&input, &mut out);
            assert_eq!(len, FRAME_SAMPLES, "目标深度上不该丢样本");
        }
        assert_eq!(control.stats().excised_samples, 0);
        assert_eq!(control.stats().inserted_samples, 0);
    }

    #[test]
    fn a_deep_queue_excises_and_pulls_extra() {
        let mut control = DriftControl::new(DriftConfig::default());
        let mut out = [0.0_f32; MAX_OUTPUT_SAMPLES];
        let mut saw_two = false;
        for _ in 0..400 {
            let plan = control.begin_tick(6);
            saw_two |= plan == PullPlan::Two;
            // 多拉一帧时喂两帧，跟真实调用方一致。
            let frames = if plan == PullPlan::Two { 2 } else { 1 };
            let len = control.process(&tone(frames), &mut out);
            assert!(len > 0);
        }
        assert!(saw_two, "缓冲太深却一次都没多拉");
        assert!(
            control.stats().excised_samples > 0,
            "缓冲太深却一个样本都没丢"
        );
    }

    #[test]
    fn a_shallow_queue_inserts_and_skips_pulls() {
        let mut control = DriftControl::new(DriftConfig::default());
        let input = tone(1);
        let mut out = [0.0_f32; MAX_OUTPUT_SAMPLES];
        let mut saw_zero = false;
        for _ in 0..400 {
            let plan = control.begin_tick(0);
            saw_zero |= plan == PullPlan::Zero;
            let len = control.process(&input, &mut out);
            assert!(len >= FRAME_SAMPLES, "缓冲太浅却把输出弄短了");
        }
        assert!(saw_zero, "缓冲见底却一次都没少拉");
        assert!(
            control.stats().inserted_samples > 0,
            "缓冲见底却一个样本都没补"
        );
    }
}
