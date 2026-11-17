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
//!
//! # 为什么「少拉一帧」必须**一个样本都不写**
//!
//! 这一条是 P4-6 反向（发送方慢）自激振荡的根因，改了一版才看清：
//!
//! PCM 环每 tick 的净变化 = `写进去的样本数 − 480`（音频侧固定取走 480）。而
//!
//! - `多拉一帧` 写 960 个 → 环 +480（**唯一能让环变深的动作**）；
//! - `正常一帧` 写 480 个 → 环不变；
//! - `少拉一帧` 写多少？**必须是 0**。写 480（回放上一帧）等于环不变，那么反向漂移时
//!   「补样本让环变深」就没有任何东西能把它变浅——环只会一路涨到溢出。
//!
//! 第一版就是回放上一帧，于是反向长跑变成：补样本抬高环 → 环太高就少拉 → 少拉不降环 →
//! 永远少拉 → 队列涨到 18 包、均值 55.84 ms、峰值 197 ms。少拉写 0 之后，环靠**存货**顶上
//! 那一帧，行为完全透明（PCM 环本来就是连续样本流，不写就是不写，没有任何拼接）。
//!
//! # 已知局限：抖动会让它做无效功（C-37）
//!
//! 这个补偿器唯一的传感器是「排队包数」，而排队包数同时被**漂移**（直流）和**网络抖动**
//! （交流）推动。滤波器是 200 ms 的 EMA，抖动的尺度就在 10–100 ms，于是抖动原样进到误差里，
//! 控制器每 tick 都饱和在 ±`MAX_SLIP`。实测 1 小时「抖动 0–10 ms + 0.5 % 丢包」：
//! 丢 178 k、补 265 k 个样本，而真正的漂移只有 17 k —— **26 倍的无效工作**。
//!
//! 它没有变坏（0 下溢、0 硬跳变、0 软跳变、`max_step` 只是平滑上界的 1.49 倍），但这份工作是白做的。
//! 正确的修法**不是**把 EMA 放慢：闭环特征方程是 `s² + αs + kp = 0`，α 必须 ≥ 2√kp 才不振荡，
//! 实测 α = 0.0005、kp = 0.02 时 Q ≈ 283，反向 4 小时里丢 30 918、补 40 635 个样本，两个方向
//! 在对打。真正该换的是**传感器**：漂移可以从「序号推进 / tick 推进」直接量出来
//! （`Δseq / Δtick − 1`），这个比值对抖动和丢包都免疫，量出来就是 ε，不需要反馈环去猜。
//! 那需要网络层把序号喂进来，属于 P5 的接线范围，所以这一版先记在这里。

use crate::FRAME_SAMPLES;

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
///
/// **这个数字决定了「补偿器会不会去追网络抖动」**。漂移是直流、抖动是交流，滤波器的截止频率
/// 决定两者分不分得开。实测（1 小时「抖动 0–10 ms + 0.5 % 丢包」联合跑）：0.05 下补偿器
/// 一小时丢了 178 k、补了 265 k 个样本，而真正的漂移只有 17 k——26 倍的无效工作，听感上是
/// 持续的轻微抖动。把 EMA 放到秒级能把这份工作压掉 33 倍。
///
/// **但放慢 EMA 会毁掉环的阻尼**：闭环特征方程是 `s² + αs + kp = 0`，α 必须 ≥ 2√kp 才
/// 不振荡。α = 0.05、kp = 0.02 时 Q ≈ 5.6（几百个 tick 收干净）；α = 0.0005 时 Q ≈ 283，
/// 实测反向 4 小时里丢 30 918、补 40 635 个样本——两个方向在对打。所以这一版**不动** EMA，
/// 联合长跑的过度补偿记成已知缺陷（C-37），正确修法是换传感器（见 `DriftControl` 的文档）。
pub const DEFAULT_EMA_ALPHA: f32 = 0.05;

/// 比例增益：排队包数的误差 → 本 tick 该丢的样本数。
///
/// 稳态误差 = `ε / KP` 个包。100 ppm 下是 0.005 个包（2.4 个样本）——是「钉住」而不是
/// 「跟随」。**这个数字和 EMA 系数是两件独立的事**：KP 决定稳态偏置，EMA 决定抖动滤不滤得掉。
/// 一开始把它们一起缩放，结果 1000 ppm 下偏置涨到 20 ms、缓冲跑到 56 ms；分开之后就没事了。
pub const DEFAULT_KP: f32 = 0.02;

/// 死区（单位：包）。误差在死区内时不做任何拼接。
///
/// 队列是按整包计量的，采样点上天然有 ±1 包的跳变；没有死区的话，滤波后的残余纹波会让
/// 补偿器每 tick 都做一次 ±1 样本的拼接——实测 4 小时里有 24 % 的样本流过拼接点，
/// 白白把 `max_step` 抬到平滑上界的 4 倍。
pub const DEFAULT_DEADBAND_PACKETS: f32 = 0.005;

/// 每 tick 最多丢/补多少样本。4 个样本 = 0.083 ms，即最多 0.83 % 的瞬时压缩率。
///
/// 这个上限同时决定「拼接有多狠」：1 个样本的拼接在 64 样本交叉淡化下几乎透明，
/// 48 个样本（1 ms）就会听成「咯」一下——P4-4 的 `max_step` 判据实测能抓到。
const MAX_SLIP: i32 = 2;

/// 交叉淡化长度（样本）。1.33 ms。
const CROSSFADE: usize = 64;

/// 输出缓冲的上限：两帧加最多一次插入。`ReceivePath` 按这个尺寸备缓冲。
pub const MAX_OUTPUT_SAMPLES: usize = 2 * FRAME_SAMPLES + MAX_SLIP as usize;

/// PCM 环水位的下界（样本）。低于它就多拉一帧。
///
/// **为什么是「一帧 + MAX_SLIP」而不是「一帧」**：环水位的落差只能按整帧走（多拉 +480、
/// 少拉 −480），所以环只会在 480 的整数倍附近落脚；而丢样本每一 tick 最多从环里拿走
/// `MAX_SLIP` 个。下界取 `480 + MAX_SLIP = 482`，于是「刚好踩到下界」之后还丢得动 2 个，
/// 落点仍是 480 —— 音频侧一次取 480 个，正好够，不会饿。下界取 480 时实测会掉到 478，
/// 那就是一次两样本的静音（P4-6 正向长跑里 4 小时 2 次下溢的来源）。
const RING_LOW: i64 = FRAME_SAMPLES as i64 + MAX_SLIP as i64;

/// PCM 环水位的上界（样本）。高于它就少拉一帧。
///
/// 少拉一帧会从环里拿走整整 480 个，所以上界必须是「下界 + 一帧」，少拉之后才落得回安全区。
const RING_HIGH: i64 = RING_LOW + FRAME_SAMPLES as i64;

/// 补偿器配置。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DriftConfig {
    /// 抖动缓冲里排队包数的目标。
    pub target_packets: f32,
    /// 深度滤波的 EMA 系数（每 tick）。见 [`DEFAULT_EMA_ALPHA`]。
    pub ema_alpha: f32,
    /// 比例增益。见 [`DEFAULT_KP`]。
    pub kp: f32,
    /// 死区（包）。见 [`DEFAULT_DEADBAND_PACKETS`]。
    pub deadband_packets: f32,
    /// 关掉就是完全直通（逐样本一致），用于回归对照。
    pub enabled: bool,
}

impl Default for DriftConfig {
    fn default() -> Self {
        Self {
            target_packets: DEFAULT_TARGET_PACKETS,
            ema_alpha: DEFAULT_EMA_ALPHA,
            kp: DEFAULT_KP,
            deadband_packets: DEFAULT_DEADBAND_PACKETS,
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
    /// 带小数的「本 tick 该丢多少样本」。整数部分才是这一 tick 真正丢的。
    ///
    /// 必须有这个累加器：100 ppm 下稳态只需每 tick 丢 **0.048** 个样本，直接取整会永远是 0，
    /// 补偿器就完全不动了。
    accum: f32,
    /// 本 tick 打算丢（正）或补（负）多少样本。**还没记账**——真正做了才从 `accum` 里扣，
    /// 见 [`DriftControl::process`]。
    slip: i32,
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
            // 滤波器**从目标起步**，不是从第一次读数起步。起播那一瞬间队列是 0，从 0 起步会
            // 让补偿器把「刚开机还没收到包」当成「缓冲太浅」，一路补样本、把环推满，然后花
            // 几分钟收回来——实测 EMA 0.0005 时 5 分钟跑法的「稳态」里全是这段尾巴。
            filtered: config.target_packets,
            accum: 0.0,
            slip: 0,
            // 环水位**如实**从 0 开始记账。之前这里给了「一帧垫子」，等于让估计值比真实
            // 水位高 480：于是「环太满就少拉」在真实水位只有一帧时就触发，反向漂移下把环
            // 抽干、把队列推到 18 包。估计值必须等于真实值，垫子要靠多拉真拉出来。
            stats: DriftStats::default(),
        }
    }

    /// 每 tick 开头调一次：喂当前排队包数，返回本 tick 该拉几帧。
    pub fn begin_tick(&mut self, queued_packets: u32) -> PullPlan {
        if !self.config.enabled {
            self.slip = 0;
            return PullPlan::One;
        }
        let depth = queued_packets as f32;
        self.filtered += (depth - self.filtered) * self.config.ema_alpha;

        let mut error_packets = self.filtered - self.config.target_packets;
        if error_packets.abs() < self.config.deadband_packets {
            error_packets = 0.0;
        }
        self.accum += error_packets * FRAME_SAMPLES as f32 * self.config.kp;
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
        // 这里**不扣** `accum`：少拉一帧的那一 tick 什么都写不进环，也就没有丢/补可言。
        // 记账放到 `process` 里，按实际做成的幅度扣。
        self.slip = slip;

        // 拉包次数**只看 PCM 环水位**，一个规则，没有第二套记账。
        //
        // 为什么这样就够：环不漂的条件是 `Σslip = 480 × Σ(pulls − 1)`。把「环低于一帧就多拉、
        // 高于两帧就少拉」当成规则，它的**平均**拉包率就自动等于 `1 + slip/480`——也就是
        // 漂移量本身。于是 B 由 slip 唯一确定（经由「环不能漂」这个约束），一个控制环就够。
        //
        // 前面两版都错在这里：用「信用」（slip 的积分）当主规则时，信用和环水位是两套独立
        // 记账，误差会累积（4 小时里环涨 136 473 个样本、溢出丢 35 733 个）；两套规则同时
        // 改 credit 又会互相追（3 分钟 3 910 次多拉 + 3 911 次少拉）。
        //
        // 环里始终留一帧垫子：`< 一帧` 就多拉，`> 两帧` 就少拉，于是环恒在 [1 帧, 2 帧]，
        // 音频侧不会饿，也撑不爆。
        let mut plan = PullPlan::One;
        if self.stats.ring_level < RING_LOW {
            plan = PullPlan::Two;
        } else if self.stats.ring_level > RING_HIGH {
            plan = PullPlan::Zero;
        }
        match plan {
            PullPlan::Two => self.stats.extra_pulls += 1,
            PullPlan::Zero => self.stats.skipped_pulls += 1,
            PullPlan::One => {}
        }
        plan
    }

    /// 把本 tick 取到的样本（一帧、两帧，或空 = 一帧都没拉）加工成要写进 PCM 环的样本，
    /// 返回长度。
    ///
    /// `input` 为空 = 本 tick 少拉了一帧（[`PullPlan::Zero`]）。这时**一个样本都不写**，
    /// 音频侧要的那 480 个样本由 PCM 环里的存货顶上。这是「少拉」能真正让环变浅的唯一写法，
    /// 理由见模块文档「为什么少拉一帧必须一个样本都不写」。
    pub fn process(&mut self, input: &[f32], out: &mut [f32]) -> usize {
        if input.is_empty() {
            self.slip = 0;
            self.stats.ring_level -= FRAME_SAMPLES as i64;
            return 0;
        }

        let slip = if self.config.enabled { self.slip } else { 0 };
        let len = splice(input, slip, out);

        // 只有**真正做成**的幅度才记账：输入不足一帧时 `splice` 会自行缩小幅度，
        // 拿打算做的数字去记会让环水位和实际写进环的样本数对不上。
        let applied = input.len() as i64 - len as i64;
        self.accum -= applied as f32;
        self.stats.ring_level += len as i64 - FRAME_SAMPLES as i64;
        if applied > 0 {
            self.stats.excised_samples += applied as u64;
            self.stats.excise_events += 1;
        } else if applied < 0 {
            self.stats.inserted_samples += (-applied) as u64;
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
    fn a_queue_at_target_never_splices() {
        // 队列在目标上时，补偿器一个样本都不许动——这条保证它不会在正常链路上偷偷改音频。
        let mut control = DriftControl::new(DriftConfig::default());
        let mut out = [0.0_f32; MAX_OUTPUT_SAMPLES];
        for _ in 0..500 {
            let plan = control.begin_tick(DEFAULT_TARGET_PACKETS as u32);
            let frames = if plan == PullPlan::Two { 2 } else { 1 };
            let len = control.process(&tone(frames), &mut out);
            assert_eq!(len, frames * FRAME_SAMPLES, "目标深度上不该丢或补样本");
        }
        assert_eq!(control.stats().excised_samples, 0);
        assert_eq!(control.stats().inserted_samples, 0);
        // 环水位必须留在 [0, 2 帧] 之内：这是「音频侧不会饿」的硬条件。
        assert!(
            (0..=2 * FRAME_SAMPLES as i64).contains(&control.stats().ring_level),
            "环水位跑出边界：{}",
            control.stats().ring_level
        );
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

    /// 闭环复现：虚拟抖动缓冲 + 补偿器，跑 `ticks` 个 tick。
    ///
    /// 返回 `(环水位最小, 环水位最大, 末尾排队包数, 补偿器)`。排队量按样本记账、取整成包，
    /// 和真实路径「先收包、再看队列、再拉包」的顺序一致。
    fn closed_loop(ppm: f32, ticks: usize) -> (i64, i64, f32, DriftControl) {
        let mut control = DriftControl::new(DriftConfig::default());
        let signal = tone(2);
        let mut out = [0.0_f32; MAX_OUTPUT_SAMPLES];
        // 起播就有两帧，对应真实路径「第一个包到了才开拉」之后的状态。
        let mut queue = 2.0_f32 * FRAME_SAMPLES as f32;
        let mut ring_min = i64::MAX;
        let mut ring_max = i64::MIN;
        for tick in 0..ticks {
            queue += FRAME_SAMPLES as f32 * (1.0 + ppm * 1e-6);
            let queued = (queue / FRAME_SAMPLES as f32) as u32;
            let plan = control.begin_tick(queued);
            let pulls = match plan {
                PullPlan::Zero => 0,
                PullPlan::One => 1,
                PullPlan::Two => 2,
            };
            // 环里没那么多帧时拉不出来（真实路径里 buffer.pull() 返回 Err）。
            let take = (pulls as f32 * FRAME_SAMPLES as f32).min(queue);
            queue -= take;
            let frames = (take / FRAME_SAMPLES as f32).round() as usize;
            let len = control.process(&signal[..frames * FRAME_SAMPLES], &mut out);
            assert!(
                len + MAX_SLIP as usize >= frames * FRAME_SAMPLES,
                "输出比拉进来的还短了太多：{len} vs {frames} 帧"
            );
            // 起播几百个 tick 是「环从 0 垫到一帧」的过程，不算稳态。
            if tick > 500 {
                ring_min = ring_min.min(control.stats().ring_level);
                ring_max = ring_max.max(control.stats().ring_level);
            }
        }
        (ring_min, ring_max, queue / FRAME_SAMPLES as f32, control)
    }

    #[test]
    fn a_fast_sender_is_absorbed_without_runaway() {
        // 正向闭环：发送方每 tick 多产 0.048 个样本，补偿器必须靠「多拉 + 丢样本」吸收，
        // 队列钉住、环水位留在 [1 帧, 2 帧]。
        let (ring_min, ring_max, queue, control) = closed_loop(100.0, 200_000);
        let stats = control.stats();
        assert!(stats.extra_pulls > 0, "正向漂移一次都没多拉");
        assert!(stats.excised_samples > 0, "正向漂移一个样本都没丢");
        assert!(
            ring_min >= FRAME_SAMPLES as i64,
            "环被抽到 {ring_min} 个样本（不足一帧，音频侧会饿）"
        );
        assert!(
            ring_max <= RING_HIGH + MAX_SLIP as i64,
            "环涨到 {ring_max} 个样本（超过两帧，白白多付延迟）"
        );
        assert!(
            (1.0..=3.0).contains(&queue),
            "队列没被钉住：末尾 {queue:.2} 包"
        );
    }

    #[test]
    fn a_slow_sender_is_absorbed_without_runaway() {
        // 反向闭环 —— P4-6 第一版就是死在这里（自激：补样本抬环 → 环太高就少拉 →
        // 少拉不降环 → 永远少拉 → 队列涨到 18 包）。
        let (ring_min, ring_max, queue, control) = closed_loop(-100.0, 200_000);
        let stats = control.stats();
        assert!(stats.skipped_pulls > 0, "反向漂移一次都没少拉");
        assert!(stats.inserted_samples > 0, "反向漂移一个样本都没补");
        assert!(
            ring_min >= FRAME_SAMPLES as i64,
            "环被抽到 {ring_min} 个样本（不足一帧，音频侧会饿）"
        );
        assert!(
            ring_max <= RING_HIGH + MAX_SLIP as i64,
            "环涨到 {ring_max} 个样本（超过两帧，白白多付延迟）"
        );
        assert!(
            (1.0..=3.0).contains(&queue),
            "队列没被钉住：末尾 {queue:.2} 包（自激振荡）"
        );
        // 丢和补不能同时对打：那是控制环在自我追逐，不是补偿。
        assert!(
            stats.excised_samples * 4 < stats.inserted_samples,
            "反向漂移里丢了 {} 个样本、补了 {} 个——两个方向在对打",
            stats.excised_samples,
            stats.inserted_samples
        );
    }
}
