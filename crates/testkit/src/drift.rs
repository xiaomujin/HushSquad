//! 时钟漂移长跑（P4-4）：两端采样时钟永远不可能完全一样。
//!
//! # 为什么这件事必须单独测
//!
//! 抖动缓冲吸收的是**网络抖动**，那是零均值的；时钟漂移是**单调**的。48 kHz 晶振 ±100 ppm
//! 意味着每小时 0.36 s 的账：缓冲要么一路涨到溢出（延迟越来越大），要么一路跌到见底（每几秒
//! 一次 PLC 爆音）。两件事都不会在 3 秒的用例里显形，也不会在任何「跑一次听一下」的验收里被
//! 发现——它只在「连续开两小时会议」时出现。所以 P4-4 是一次**长跑**。
//!
//! # 模型：谁是被测时钟
//!
//! 播放设备的时钟是**主时钟**（`tick` 的节拍），发送方的采集时钟相对它快/慢 `ppm`：每过一个
//! 10 ms 的播放 tick，发送方产生 `480 × (1 + ppm×10⁻⁶)` 个样本。于是 `+100 ppm` 下每 10 000
//! 个 tick（100 秒）多出一个包，4 小时多出 144 个包 = 69 120 样本 = 1.44 秒。
//!
//! 注入点选在**发送侧**而不是改播放节拍：真实世界里没法让声卡跑快一点，能改的只有「我们收到
//! 多少」。序列号与时间戳都按发送方的样本计数走，所以漂移对抖动缓冲来说就是「包来得比消耗
//! 快」，正是它该处理的东西。
//!
//! # 不睡觉
//!
//! 整条循环里没有任何 `sleep`：一个 tick 就是一次 10 ms 的虚拟时间。所谓「4 小时」是 144 万次
//! 迭代的**虚拟**时长，墙钟时间只取决于 Opus 编解码的速度。这也是这个测试能被反复重跑的原因。
//!
//! # 判据
//!
//! 1. **缓冲深度不单调漂移**：把长跑切成 60 秒一窗，末窗均值与首窗均值之差 ≤ 2 ms。
//! 2. **不爆音**：稳态下音频侧零下溢、零硬跳变。
//! 3. **延迟不累积**：即判据 1——抖动缓冲深度就是排队延迟，它平了就说明总延迟平了。
//! 4. **账要对得上**：`被压缩样本数 − 被拉伸样本数 ≈ 两端累计样本数差`。对不上说明漂移不是
//!    被时间伸缩吸收的，而是被别的东西（丢包、PLC、缓冲清空）掩盖了。
//!
//! # 为什么用单音而不是语音素材
//!
//! 这里量的不是「听起来像不像」，而是缓冲会不会溢出/见底、时间伸缩有没有把波形撕开。1 kHz
//! 正弦的相位连续、一阶差分有解析上界，所以任何拼接都能被 `max_step` 抓到；语音素材的天然
//! 跳变会把这种检测淹掉。真正的音质判决留给 D6 盲听（P3-6）。

use serde::{Deserialize, Serialize};

use crate::netsim::{NetSim, NetSimConfig};
use audio_core::receive::{IncomingPacket, ReceivePath};
use audio_core::{
    AudioCodec, FRAME_MS, FRAME_SAMPLES, Frame, MAX_PACKET_BYTES, OpusCodec, SAMPLE_RATE,
};

/// 默认跑多久（小时）。
pub const DEFAULT_HOURS: f64 = 4.0;

/// 默认注入的时钟失配（ppm）。±100 ppm 是消费级晶振的坏日子（典型 ±20…±50 ppm）。
pub const DEFAULT_PPM: f64 = 100.0;

/// 目标延迟下限（毫秒）：附录 A 第 9 行 L2 档下界。
pub const DEFAULT_MIN_DELAY_MS: u32 = 15;

/// 目标延迟上限（毫秒）：附录 A 第 9 行 L2 档上界。
pub const DEFAULT_MAX_DELAY_MS: u32 = 20;

/// 判据「不单调漂移」的容差（毫秒）：稳态后三分之一与中间三分之一的均值之差超过它就判定
/// 「还在爬」。
pub const TREND_TOLERANCE_MS: f64 = 2.0;

/// 判据「被控住」的容差（毫秒）：稳态缓冲深度的极差超过它，说明缓冲在乱走。
///
/// **下限由包结构决定，不由控制器决定**：抖动缓冲深度按整包计量，一包 = 480 样本 = 10 ms，
/// 所以「1 包」与「2 包」之间必然跳 10 ms。要求 < 10 ms 等于要求它停在半个包上。
/// 取 12 ms = 一个包 + 2 ms 余量：放得过「在 1↔2 包之间跳」，放不过「在 1↔4 包之间乱走」。
pub const SPREAD_TOLERANCE_MS: f64 = 12.0;

/// 入站包环容量。一个 tick 最多产生 2 个包，64 足够。
const PACKET_CAPACITY: usize = 64;

/// PCM 环深度（帧）。要能吃下一次时间伸缩吐出的 2 帧，8 帧（80 ms）很宽裕；
/// 它不影响被测的抖动缓冲深度。
const PCM_FRAMES: usize = 8;

/// 单音频率。1 kHz 落在语音主频段，Opus 编它不费劲，相位跳变又够显眼。
pub const TONE_HZ: f64 = 1_000.0;

/// 单音幅度（−12 dBFS）。留足余量，避免编码器削波掩盖跳变。
pub const TONE_AMPLITUDE: f32 = 0.25;

/// 起播跳过多久（秒）。缓冲从 0 垫到目标延迟的那一段必然有 PLC 与拼接，不算稳态。
pub const WARMUP_S: f64 = 2.0;

/// 统计窗口长度（秒）。
pub const WINDOW_S: f64 = 60.0;

/// 缓冲深度的采样间隔（tick，即 100 ms）。
const SAMPLE_TICKS: u64 = 10;

/// 每毫秒多少样本（48）。直方图按毫秒分桶。
const SAMPLES_PER_MS: usize = (SAMPLE_RATE / 1_000) as usize;

/// 直方图上界（毫秒）。上限被夹在 20 ms，留到 128 只是为了「万一」也看得见。
const HIST_BUCKETS: usize = 129;

/// 「软跳变」的阈值倍数（相对平滑正弦的最大一阶差分）。
///
/// 平滑正弦的 `max |x[n]−x[n−1]| = 2A·sin(πf/fs)`，本配置下约 0.0327。
pub const SOFT_STEP_FACTOR: f32 = 3.0;

/// 「硬跳变」（爆音）的阈值倍数。8 倍约 0.26，接近满量程——只有真正撕裂波形才会到。
pub const HARD_STEP_FACTOR: f32 = 8.0;

/// 发送方标识。
const SENDER: u32 = 0x4853_5102;

/// 报告格式版本。指标口径变了就加一。
///
/// v2：把「样本守恒残差」换成区间判据 + 推导出的 `fabricated_samples`（见该字段文档）。
/// v3：加入网络条件（抖动/丢包/种子），支持「抖动 + 漂移」联合长跑。
pub const VERSION: u32 = 3;

/// 网络条件（联合长跑用）。全 0 = 理想链路，与 P4-4/P4-6 的纯漂移跑逐位一致。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct NetConditions {
    /// 抖动上限（毫秒）：每个包的额外延迟在 `0..=jitter_ms` 内均匀分布。
    pub jitter_ms: u32,
    /// 丢包概率（0.0–1.0）。
    pub loss: f32,
    /// 随机种子：同种子 + 同参数 ⇒ 逐位相同的投递时刻。
    pub seed: u64,
}

impl NetConditions {
    /// 是否就是理想链路（此时完全不经过模拟器，保证旧跑法的数字一字不改）。
    #[must_use]
    pub fn is_ideal(&self) -> bool {
        self.jitter_ms == 0 && self.loss == 0.0
    }
}

/// 补偿器的三个可调参数。默认值就是 `DriftConfig::default()`。
///
/// 暴露出来只有一个理由：**「漂移是直流、抖动是交流」，而滤波器的时间常数决定了两者分不分得开**。
/// 这件事只能用长跑量，不能靠推。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DriftTuning {
    /// 深度滤波的 EMA 系数。
    pub ema_alpha: f32,
    /// 比例增益。
    pub kp: f32,
    /// 死区（包）。
    pub deadband_packets: f32,
}

impl Default for DriftTuning {
    fn default() -> Self {
        Self {
            ema_alpha: audio_core::drift::DEFAULT_EMA_ALPHA,
            kp: audio_core::drift::DEFAULT_KP,
            deadband_packets: audio_core::drift::DEFAULT_DEADBAND_PACKETS,
        }
    }
}

impl DriftTuning {
    fn to_config(self, compensate: bool) -> audio_core::DriftConfig {
        audio_core::DriftConfig {
            target_packets: audio_core::drift::DEFAULT_TARGET_PACKETS,
            ema_alpha: self.ema_alpha,
            kp: self.kp,
            deadband_packets: self.deadband_packets,
            enabled: compensate,
        }
    }
}

/// 一个统计窗口的摘要。长跑的唯一用途是看这条序列有没有趋势。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// 窗口起点（秒）。
    pub start_s: f64,
    /// 缓冲深度均值（毫秒）。
    pub mean_buffered_ms: f64,
    /// 缓冲深度最小值（毫秒）。
    pub min_buffered_ms: f64,
    /// 缓冲深度最大值（毫秒）。
    pub max_buffered_ms: f64,
    /// 窗口末尾的目标延迟（毫秒）。
    pub target_ms: u32,
    /// 本窗口内的下溢次数。
    pub underruns: u64,
    /// 本窗口内的硬跳变次数。
    pub hard_steps: u64,
    /// 本窗口内被时间压缩吃掉的样本（毫秒）。
    pub removed_ms: f64,
    /// 本窗口内 PLC 外推出来的样本（毫秒）。
    pub concealed_ms: f64,
}

/// 一次长跑的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    /// 报告格式版本。
    pub version: u32,
    /// 虚拟时长（小时）。
    pub hours: f64,
    /// 注入的时钟失配（ppm，正数表示发送方快）。
    pub ppm: f64,
    /// 注入的网络抖动上限（毫秒）。0 = 理想链路。
    #[serde(default)]
    pub jitter_ms: u32,
    /// 注入的丢包概率。0 = 不丢。
    #[serde(default)]
    pub loss: f32,
    /// 网络模拟的随机种子。
    #[serde(default)]
    pub net_seed: u64,
    /// 目标延迟下限（毫秒）。
    pub min_delay_ms: u32,
    /// 目标延迟上限（毫秒）。
    pub max_delay_ms: u32,
    /// 结束时的目标延迟（毫秒）。
    pub target_delay_ms: u32,
    /// 单音频率（Hz）。
    pub tone_hz: f64,
    /// 播放 tick 总数（= 虚拟秒数 × 100）。
    pub ticks: u64,
    /// 发送方产出的帧数。
    pub sender_frames: u64,
    /// 音频侧真正取到的帧数。
    pub receiver_frames: u64,
    /// 两端累计样本数差（样本）。正数 = 发送方多出来的。
    pub drift_samples: i64,
    /// 被时间压缩吃掉的样本数（累计）。
    pub removed_samples: u64,
    /// 被时间拉伸补进去的样本数（累计）。
    pub inserted_samples: u64,
    /// `removed − inserted`。理论上应当 ≈ `drift_samples`。
    pub net_removed_samples: i64,
    /// PLC 外推的样本数。
    pub concealed_samples: u64,
    /// PLC 事件次数。
    pub concealment_events: u64,
    /// 迟到丢弃的包数。
    pub late_packets: u64,
    /// 缓冲被清空的次数。
    pub buffer_flushes: u64,
    /// 被判为抖动尖峰的次数（理想链路下应恒为 0；联合长跑里它是正常现象）。
    pub jitter_peaks: u16,
    /// 是否开启 P4-6 的漂移补偿。
    pub compensate: bool,
    /// 补偿器丢掉的样本数（稳态窗口）。
    pub drift_excised_samples: i64,
    /// 补偿器补进去的样本数（稳态窗口）。
    pub drift_inserted_samples: i64,
    /// 多拉帧次数（累计）。
    pub extra_pulls: u64,
    /// 少拉帧次数（累计）。
    pub skipped_pulls: u64,
    /// 稳态窗口内 PCM 环水位的变化（样本）。
    pub ring_delta_samples: i64,
    /// 稳态排队包数的均值。
    pub packets_queued_mean: f64,
    /// 稳态排队包数的最小值。
    pub packets_queued_min: u32,
    /// 稳态排队包数的最大值。
    pub packets_queued_max: u32,
    /// PCM 环满被丢掉的样本数（音频侧没跟上）。
    pub samples_dropped_full: u64,
    /// 稳态缓冲深度最小值（毫秒）。
    pub buffered_ms_min: f64,
    /// 稳态缓冲深度最大值（毫秒）。
    pub buffered_ms_max: f64,
    /// 稳态缓冲深度均值（毫秒）。
    pub buffered_ms_mean: f64,
    /// 稳态缓冲深度 p50（毫秒）。
    pub buffered_ms_p50: f64,
    /// 稳态缓冲深度 p95（毫秒）。
    pub buffered_ms_p95: f64,
    /// 稳态（跳过前三分之一窗口）缓冲深度均值（毫秒）。判据「在预算内」用它。
    pub settled_mean_ms: f64,
    /// 稳态缓冲深度的极差（毫秒）。反映「有没有被控住」。
    pub settled_spread_ms: f64,
    /// 稳态的长期爬升：后三分之一均值 − 中间三分之一均值（毫秒）。
    ///
    /// 与 [`Run::drift_trend_ms`] 的区别：那个把「起播垫缓冲」也算成漂移，这个只看
    /// **稳定之后**还在不在慢慢爬。判据「不单调漂移」用它。
    pub settled_trend_ms: f64,
    /// 稳态下溢次数。
    pub underruns: u64,
    /// 起播期的下溢次数（单独记，不参与判定）。
    pub warmup_underruns: u64,
    /// 抖动缓冲报缺帧的次数。
    pub frames_missing: u64,
    /// 帧长不是 480 的次数（时间伸缩的直接痕迹）。
    pub frames_off_size: u64,
    /// 软跳变次数（一阶差分 > 3 × 平滑上界）。
    pub soft_steps: u64,
    /// 硬跳变次数（一阶差分 > 8 × 平滑上界）——这才是「爆音」。
    pub hard_steps: u64,
    /// 稳态最大一阶差分（绝对值）。
    pub max_step: f32,
    /// 平滑正弦的最大一阶差分（阈值基准）。
    pub step_bound: f32,
    /// 末窗均值 − 首窗均值（毫秒）。含起播垫缓冲，仅作对照。
    pub drift_trend_ms: f64,
    /// 稳态第一个采样点的缓冲深度（样本）。
    pub buffered_first_samples: u32,
    /// 稳态最后一个采样点的缓冲深度（样本）。
    pub buffered_last_samples: u32,
    /// 稳态窗口内两端累计样本数差（样本）。
    pub steady_drift_samples: i64,
    /// 稳态窗口内被时间压缩吃掉的样本数。
    pub steady_removed_samples: i64,
    /// 稳态窗口内被时间拉伸补进去的样本数。
    pub steady_inserted_samples: i64,
    /// 稳态窗口内 PLC 外推出来的样本数。
    pub steady_concealed_samples: i64,
    /// 稳态窗口内被网络丢掉的样本数（`packets_lost × 480`）。
    ///
    /// 联合长跑必须有这一项：丢了 1 个包，NetEQ 就得多造 480 个样本来顶替，而
    /// `发送帧数 − 收到帧数` 把「丢的包」和「时钟漂移」算成了一笔账。不拆开，守恒等式
    /// 会差出成千上万个样本（实测 1 小时 0.5 % 丢包：差 105 024 个，全是这一项漏的）。
    pub steady_lost_samples: i64,
    /// 由守恒关系**推导**出来的「凭空造出来的样本数」：
    /// `缓冲变化 + 压缩 − 拉伸 + 环水位变化 + 网络丢的样本 − 累计差`。
    ///
    /// 它必须落在 `[0, steady_concealed_samples]` 之间：下限 0 是说「没有样本凭空消失」，
    /// 上限是 NetEQ 自报的 PLC 样本数。
    ///
    /// 为什么是区间而不是等式：`neteq` 的 `concealed_samples` 把 Expand 的**整个输出帧**
    /// 都记成隐藏样本，但 Expand 实际上先吃掉一部分真音频、只外推剩下的一小段——所以它
    /// 只是**上界**（实测 4 小时 −100 ppm：自报 93 120，推导 68 928，高报 25 %）。
    pub fabricated_samples: i64,
    /// 逐窗口摘要。
    pub windows: Vec<Window>,
}

/// 直方图分位数（毫秒，取桶中心，故分辨率 1 ms）。
fn percentile(hist: &[u64; HIST_BUCKETS], total: u64, q: f64) -> f64 {
    if total == 0 {
        return f64::NAN;
    }
    let want = (q * total as f64).ceil() as u64;
    let mut acc = 0_u64;
    for (bucket, count) in hist.iter().enumerate() {
        acc += *count;
        if acc >= want {
            return bucket as f64 + 0.5;
        }
    }
    (HIST_BUCKETS - 1) as f64
}

fn samples_to_ms(samples: f64) -> f64 {
    samples / SAMPLES_PER_MS as f64
}

fn mean_of(windows: &[Window]) -> f64 {
    if windows.is_empty() {
        return f64::NAN;
    }
    windows.iter().map(|w| w.mean_buffered_ms).sum::<f64>() / windows.len() as f64
}

fn spread_of(windows: &[Window]) -> f64 {
    if windows.is_empty() {
        return f64::NAN;
    }
    let min = windows
        .iter()
        .map(|w| w.min_buffered_ms)
        .fold(f64::INFINITY, f64::min);
    let max = windows
        .iter()
        .map(|w| w.max_buffered_ms)
        .fold(f64::NEG_INFINITY, f64::max);
    max - min
}

/// 跑一次漂移长跑。
pub fn run(hours: f64, ppm: f64, min_ms: u32, max_ms: u32) -> Result<Run, String> {
    run_with(hours, ppm, min_ms, max_ms, true)
}

/// 跑一次漂移长跑，`compensate` 决定是否开启 P4-6 的漂移补偿。理想链路。
pub fn run_with(
    hours: f64,
    ppm: f64,
    min_ms: u32,
    max_ms: u32,
    compensate: bool,
) -> Result<Run, String> {
    run_full(
        hours,
        ppm,
        min_ms,
        max_ms,
        compensate,
        NetConditions::default(),
        DriftTuning::default(),
    )
}

/// 跑一次漂移长跑，并叠加网络条件（抖动 + 丢包）。
///
/// 联合长跑的意义：单靠漂移，缓冲谷底是 14 ms；叠加抖动之后谷底会被压到目标延迟的下沿，
/// 那时**漂移补偿器的每一次误判都会变成一次爆音**——这是纯漂移跑永远看不到的。
pub fn run_with_net(
    hours: f64,
    ppm: f64,
    min_ms: u32,
    max_ms: u32,
    compensate: bool,
    net: NetConditions,
) -> Result<Run, String> {
    run_full(
        hours,
        ppm,
        min_ms,
        max_ms,
        compensate,
        net,
        DriftTuning::default(),
    )
}

/// 跑一次长跑，网络条件与补偿器参数都显式给出。
pub fn run_full(
    hours: f64,
    ppm: f64,
    min_ms: u32,
    max_ms: u32,
    compensate: bool,
    net: NetConditions,
    tuning: DriftTuning,
) -> Result<Run, String> {
    if !hours.is_finite() || hours <= 0.0 {
        return Err(format!("时长必须是正的有限值（小时），收到 {hours}"));
    }
    if !ppm.is_finite() || ppm.abs() > 10_000.0 {
        return Err(format!("ppm 必须有限且绝对值不超过 10000，收到 {ppm}"));
    }
    if min_ms > max_ms {
        return Err(format!("延迟下限 {min_ms} ms 大于上限 {max_ms} ms"));
    }

    let ticks_per_s = u64::from(SAMPLE_RATE / FRAME_SAMPLES as u32);
    let ticks = (hours * 3_600.0 * ticks_per_s as f64).round() as u64;
    if ticks == 0 {
        return Err("时长不足一个 tick（10 ms）".to_owned());
    }
    let warmup_ticks = (WARMUP_S * ticks_per_s as f64).round() as u64;
    let window_ticks = ((WINDOW_S * ticks_per_s as f64).round() as u64).max(1);

    let (mut tx, mut path, mut out) = ReceivePath::new(SENDER, PACKET_CAPACITY, PCM_FRAMES)?;
    path.set_delay_limits(min_ms, max_ms);
    path.set_drift_config(tuning.to_config(compensate));
    let mut codec = OpusCodec::with_defaults().map_err(|e| format!("构造编码器失败：{e}"))?;
    let mut sim = NetSim::new(NetSimConfig {
        loss: net.loss,
        jitter_ms: net.jitter_ms,
        reorder: 0.0,
        seed: net.seed,
    });

    // 平滑正弦的一阶差分上界：2A·sin(πf/fs)。
    let step_bound = 2.0
        * TONE_AMPLITUDE
        * (std::f64::consts::PI * TONE_HZ / f64::from(SAMPLE_RATE)).sin() as f32;
    let soft_threshold = SOFT_STEP_FACTOR * step_bound;
    let hard_threshold = HARD_STEP_FACTOR * step_bound;

    let samples_per_tick = FRAME_SAMPLES as f64 * (1.0 + ppm * 1e-6);
    let mut produced = 0.0_f64;
    let mut sender_frames: u64 = 0;
    let mut receiver_frames: u64 = 0;

    let mut hist = [0_u64; HIST_BUCKETS];
    let mut underruns = 0_u64;
    let mut warmup_underruns = 0_u64;
    let mut soft_steps = 0_u64;
    let mut hard_steps = 0_u64;
    let mut max_step = 0.0_f32;
    let mut prev_sample = 0.0_f32;

    let mut sum_samples = 0_u64;
    let mut count_samples = 0_u64;
    let mut min_samples = u32::MAX;
    let mut max_samples = 0_u32;
    let mut target_ms = 0_u32;
    let mut packets_sum = 0_u64;
    let mut packets_min = u32::MAX;
    let mut packets_max = 0_u32;

    let mut windows: Vec<Window> = Vec::new();
    let mut win_sum = 0_u64;
    let mut win_count = 0_u64;
    let mut win_min = u32::MAX;
    let mut win_max = 0_u32;
    let mut win_target = 0_u32;
    let mut win_underruns = 0_u64;
    let mut win_hard = 0_u64;
    let mut win_start = warmup_ticks;
    // 稳态首尾采样点：样本守恒要它们，光有直方图算不出缓冲变化量。
    // 守恒必须按**同一段**时间的两端快照来算——把起播期算进去就永远对不上（那一段
    // 缓冲从 0 垫到目标、PCM 环也从空填到稳态，两笔都记在账上却不在稳态窗口里）。
    let mut first_samples: Option<u32> = None;
    let mut last_samples = 0_u32;
    let mut start_sender = 0_u64;
    let mut start_receiver = 0_u64;
    let mut start_removed = 0_u64;
    let mut start_inserted = 0_u64;
    let mut start_concealed = 0_u64;
    let mut start_ring = 0_i64;
    let mut start_lost = 0_u64;
    let mut start_excised = 0_u64;
    let mut start_inserted_drift = 0_u64;
    // 上一个封窗时刻的累计值，用来算窗口内的增量。
    let mut prev_removed = 0_u64;
    let mut prev_concealed = 0_u64;

    let mut frame: Frame = [0.0; FRAME_SAMPLES];
    let mut packet = [0_u8; MAX_PACKET_BYTES];

    for tick in 0..ticks {
        let steady = tick >= warmup_ticks;

        // ---- 发送方：按自己的时钟产包 ----
        produced += samples_per_tick;
        while produced >= FRAME_SAMPLES as f64 {
            produced -= FRAME_SAMPLES as f64;
            let base = sender_frames * FRAME_SAMPLES as u64;
            for (n, sample) in frame.iter_mut().enumerate() {
                let t = (base + n as u64) as f64 / f64::from(SAMPLE_RATE);
                *sample = TONE_AMPLITUDE * (std::f64::consts::TAU * TONE_HZ * t).sin() as f32;
            }
            let len = codec.encode(&frame, &mut packet);
            if len == 0 {
                return Err(format!("第 {sender_frames} 帧编码失败"));
            }
            let timestamp = sender_frames.wrapping_mul(FRAME_SAMPLES as u64) as u32;
            let pkt = IncomingPacket::new(SENDER, sender_frames as u16, timestamp, &packet[..len]);
            if net.is_ideal() {
                // 理想链路完全不过模拟器：P4-4/P4-6 的旧数字必须逐位不变。
                if !tx.send(pkt) {
                    return Err("入站包环溢出：一个 tick 产出的包数超出容量".to_owned());
                }
            } else {
                let now_ms = tick * u64::from(FRAME_MS);
                for (_, delivered) in sim.step(pkt, now_ms) {
                    if !tx.send(delivered) {
                        return Err("入站包环溢出：一个 tick 到期的包数超出容量".to_owned());
                    }
                }
            }
            sender_frames += 1;
        }

        // ---- 接收方：一次 tick = 一次 10 ms 播放 ----
        path.tick();
        if out.pull_frame(&mut frame) {
            receiver_frames += 1;
            for sample in frame.iter() {
                let delta = (sample - prev_sample).abs();
                if delta > max_step {
                    max_step = delta;
                }
                if steady {
                    if delta > hard_threshold {
                        hard_steps += 1;
                        win_hard += 1;
                    } else if delta > soft_threshold {
                        soft_steps += 1;
                    }
                }
                prev_sample = *sample;
            }
        } else if steady {
            underruns += 1;
            win_underruns += 1;
        } else {
            warmup_underruns += 1;
        }

        // ---- 采样缓冲深度（每 100 ms 一次）----
        if steady && tick % SAMPLE_TICKS == 0 {
            let samples = path.buffered_samples();
            sum_samples += u64::from(samples);
            count_samples += 1;
            min_samples = min_samples.min(samples);
            max_samples = max_samples.max(samples);
            target_ms = path.target_delay_ms();
            let queued = path.queued_packets();
            packets_sum += u64::from(queued);
            packets_min = packets_min.min(queued);
            packets_max = packets_max.max(queued);
            let bucket = (samples as usize / SAMPLES_PER_MS).min(HIST_BUCKETS - 1);
            hist[bucket] += 1;
            if first_samples.is_none() {
                first_samples = Some(samples);
                start_sender = sender_frames;
                start_receiver = receiver_frames;
                let snapshot = path.jitter_stats();
                start_removed = snapshot.removed_samples_for_acceleration;
                start_inserted = snapshot.inserted_samples_for_deceleration;
                start_concealed = snapshot.concealed_samples;
                let drift = path.drift_stats();
                start_ring = drift.ring_level;
                start_excised = drift.excised_samples;
                start_inserted_drift = drift.inserted_samples;
                start_lost = path.stats().packets_lost;
            }
            last_samples = samples;

            win_sum += u64::from(samples);
            win_count += 1;
            win_min = win_min.min(samples);
            win_max = win_max.max(samples);
            win_target = target_ms;
        }

        // ---- 封窗 ----
        if steady && (tick + 1 == ticks || tick + 1 >= win_start + window_ticks) {
            let snapshot = path.jitter_stats();
            let removed_in_window = snapshot
                .removed_samples_for_acceleration
                .saturating_sub(prev_removed);
            let concealed_in_window = snapshot.concealed_samples.saturating_sub(prev_concealed);
            prev_removed = snapshot.removed_samples_for_acceleration;
            prev_concealed = snapshot.concealed_samples;
            if win_count > 0 {
                windows.push(Window {
                    start_s: win_start as f64 / ticks_per_s as f64,
                    mean_buffered_ms: samples_to_ms(win_sum as f64 / win_count as f64),
                    min_buffered_ms: samples_to_ms(f64::from(win_min)),
                    max_buffered_ms: samples_to_ms(f64::from(win_max)),
                    target_ms: win_target,
                    underruns: win_underruns,
                    hard_steps: win_hard,
                    removed_ms: samples_to_ms(removed_in_window as f64),
                    concealed_ms: samples_to_ms(concealed_in_window as f64),
                });
            }
            win_start = tick + 1;
            win_sum = 0;
            win_count = 0;
            win_min = u32::MAX;
            win_max = 0;
            win_underruns = 0;
            win_hard = 0;
        }
    }

    let jitter = path.jitter_stats();
    let receive = path.stats();
    let removed = jitter.removed_samples_for_acceleration;
    let inserted = jitter.inserted_samples_for_deceleration;
    let drift_trend_ms = match (windows.first(), windows.last()) {
        (Some(first), Some(last)) if windows.len() >= 2 => {
            last.mean_buffered_ms - first.mean_buffered_ms
        }
        _ => 0.0,
    };

    // 稳态 = 跳过前三分之一窗口。那一段是「把缓冲垫到目标」的过程，把它算成漂移会
    // 把「收敛到错误的水平」和「永远在爬」混为一谈——这两件事的修法完全不同。
    let third = windows.len() / 3;
    let settled = &windows[third.min(windows.len())..];
    let settled_mean_ms = mean_of(settled);
    let settled_spread_ms = spread_of(settled);
    let settled_trend_ms = if windows.len() >= 3 {
        mean_of(&windows[2 * third..]) - mean_of(&windows[third..2 * third])
    } else {
        0.0
    };

    let frame_bytes = FRAME_SAMPLES as i64;
    let drift_samples = (sender_frames as i64 - receiver_frames as i64) * frame_bytes;
    let first_samples = first_samples.unwrap_or(0);
    let delta_buffer = i64::from(last_samples) - i64::from(first_samples);
    // 样本守恒（只算稳态窗口）：drift = 压缩 − 拉伸 − PLC + 缓冲变化。
    let steady_drift = (sender_frames - start_sender) as i64 * frame_bytes
        - (receiver_frames - start_receiver) as i64 * frame_bytes;
    let steady_removed = removed.saturating_sub(start_removed) as i64;
    let steady_inserted = inserted.saturating_sub(start_inserted) as i64;
    let steady_concealed = jitter.concealed_samples.saturating_sub(start_concealed) as i64;
    let drift_now = path.drift_stats();
    let ring_delta_samples = drift_now.ring_level - start_ring;
    let steady_excised = drift_now.excised_samples.saturating_sub(start_excised) as i64;
    let steady_inserted_drift = drift_now
        .inserted_samples
        .saturating_sub(start_inserted_drift) as i64;
    // 网络丢掉的样本：这些包从没到过抖动缓冲，NetEQ 必须靠 PLC 造出来顶上。
    let steady_lost =
        (receive.packets_lost.saturating_sub(start_lost)) as i64 * FRAME_SAMPLES as i64;
    // 样本守恒（含补偿器与网络丢包）：
    //   fabricated = ΔB + 丢 − 补 + Δ环 + 网络丢的 − 两端累计差
    let fabricated_samples =
        delta_buffer + steady_excised - steady_inserted_drift + ring_delta_samples + steady_lost
            - steady_drift;

    Ok(Run {
        version: VERSION,
        hours,
        ppm,
        jitter_ms: net.jitter_ms,
        loss: net.loss,
        net_seed: net.seed,
        compensate,
        drift_excised_samples: steady_excised,
        drift_inserted_samples: steady_inserted_drift,
        extra_pulls: drift_now.extra_pulls,
        skipped_pulls: drift_now.skipped_pulls,
        ring_delta_samples,
        packets_queued_mean: if count_samples == 0 {
            f64::NAN
        } else {
            packets_sum as f64 / count_samples as f64
        },
        packets_queued_min: packets_min,
        packets_queued_max: packets_max,
        samples_dropped_full: receive.samples_dropped_full,
        min_delay_ms: min_ms,
        max_delay_ms: max_ms,
        target_delay_ms: target_ms,
        tone_hz: TONE_HZ,
        ticks,
        sender_frames,
        receiver_frames,
        drift_samples,
        removed_samples: removed,
        inserted_samples: inserted,
        net_removed_samples: removed as i64 - inserted as i64,
        concealed_samples: jitter.concealed_samples,
        concealment_events: jitter.concealment_events,
        late_packets: jitter.late_packets_discarded,
        buffer_flushes: jitter.buffer_flushes,
        jitter_peaks: jitter.jitter_peaks_found,
        buffered_ms_min: samples_to_ms(f64::from(min_samples)),
        buffered_ms_max: samples_to_ms(f64::from(max_samples)),
        buffered_ms_mean: if count_samples == 0 {
            f64::NAN
        } else {
            samples_to_ms(sum_samples as f64 / count_samples as f64)
        },
        buffered_ms_p50: percentile(&hist, count_samples, 0.50),
        buffered_ms_p95: percentile(&hist, count_samples, 0.95),
        settled_mean_ms,
        settled_spread_ms,
        settled_trend_ms,
        underruns,
        warmup_underruns,
        frames_missing: receive.frames_missing,
        frames_off_size: receive.frames_off_size,
        soft_steps,
        hard_steps,
        max_step,
        step_bound,
        drift_trend_ms,
        buffered_first_samples: first_samples,
        buffered_last_samples: last_samples,
        steady_drift_samples: steady_drift,
        steady_removed_samples: steady_removed,
        steady_inserted_samples: steady_inserted,
        steady_concealed_samples: steady_concealed,
        steady_lost_samples: steady_lost,
        fabricated_samples,
        windows,
    })
}

/// 判据是否全部满足。返回违反的条目（空 = 通过）。
#[must_use]
pub fn violations(run: &Run) -> Vec<String> {
    let mut bad: Vec<String> = Vec::new();

    // 判据 1a：稳态水平不许超预算。漂移被「吸收」到 40 ms 也算失败——那 20 ms 的账
    // 迟早要在延迟预算表里还。
    //
    // 下界取 10 ms（一包）而**不是**预算下界 15 ms：缓冲比预算浅不是问题，那是省了延迟；
    // 浅到会饿才是问题，而那个由「0 下溢」单独判（实测 +1000 ppm 稳态 13.83 ms、0 下溢）。
    // 补偿器的控制目标是「排队 2 包」，而 `buffered_samples` 在这个点上读 14 ms——拿 15 ms
    // 当硬下界等于要求控制器瞄准一个比它自己标定值高 1 ms 的点。
    let low = 10.0;
    let high = f64::from(run.max_delay_ms) + 1.0;
    if !(run.settled_mean_ms >= low && run.settled_mean_ms <= high) {
        bad.push(format!(
            "稳态缓冲深度 {:.2} ms 超出预算 {}–{} ms",
            run.settled_mean_ms, run.min_delay_ms, run.max_delay_ms
        ));
    }

    // 判据 1b：稳定之后不能再爬。
    if run.settled_trend_ms.abs() > TREND_TOLERANCE_MS {
        bad.push(format!(
            "稳态仍在爬升：后三分之一 − 中间三分之一 = {:+.2} ms（容差 ±{TREND_TOLERANCE_MS:.1}）",
            run.settled_trend_ms
        ));
    }

    // 判据 1c：不能乱走。缓冲在 5–21 ms 之间来回摆，等于没有任何控制——5 ms 的时候
    // 只要来一个晚到的包就是一次爆音。
    // 摆动这条判据**只在干净链路上成立**。抖动一来，缓冲的上下摆是 NetEQ 在追抖动（实测
    // 4 小时「抖动 0–10 ms + 0.5 % 丢包」：3–48 ms，摆动 45 ms），那是网络的性质，不是补偿器
    // 失控。硬判它等于拿「网络抖不抖」当「控制器好不好」。
    //
    // 抖动链路上靠另外四条兜底：稳态均值不许超预算（超了就是补偿器把缓冲推走了）、
    // 不许有趋势（爬 = 没吸收掉漂移）、0 下溢、0 硬跳变。
    if run.jitter_ms == 0 && run.settled_spread_ms > SPREAD_TOLERANCE_MS {
        bad.push(format!(
            "稳态缓冲深度摆动 {:.2} ms（容差 {SPREAD_TOLERANCE_MS:.1} ms）：没有被控住",
            run.settled_spread_ms
        ));
    }

    if run.underruns > 0 {
        bad.push(format!("稳态下溢 {} 次（会听成断续）", run.underruns));
    }
    if run.hard_steps > 0 {
        bad.push(format!("硬跳变 {} 次（爆音）", run.hard_steps));
    }
    if run.buffer_flushes > 0 {
        bad.push(format!("缓冲被清空 {} 次", run.buffer_flushes));
    }

    // 判据 4：样本守恒。推导出来的「造出来的样本数」必须落在 [0, NetEQ 自报的 PLC 样本数]。
    // 上界不是等号，原因见 `Run::fabricated_samples` 的文档。
    // 容差 4 帧：PCM 环水位是**记账估出来的**（不是读环），加上补偿器多拉/少拉的边界效应，
    // 残差本来就有几帧的模糊。这个判据是用来抓「成百上千帧凭空消失」的，不是拿来做精密对账的。
    let tolerance = 4 * FRAME_SAMPLES as i64;
    if run.fabricated_samples < -tolerance
        || run.fabricated_samples > run.steady_concealed_samples + tolerance
    {
        bad.push(format!(
            "样本守恒对不上：推导出的「造出来的样本」{} 不在 [0, NetEQ 自报 {}] 之间（稳态累计差 {}，压缩 {}，拉伸 {}，缓冲变化 {}）",
            run.fabricated_samples,
            run.steady_concealed_samples,
            run.steady_drift_samples,
            run.steady_removed_samples,
            -run.steady_inserted_samples,
            run.buffered_last_samples as i64 - run.buffered_first_samples as i64
        ));
    }
    bad
}

fn print_report(run: &Run) {
    println!(
        "漂移长跑：{:.2} 小时 / {:+.1} ppm / 目标延迟 {}–{} ms / 单音 {:.0} Hz",
        run.hours, run.ppm, run.min_delay_ms, run.max_delay_ms, run.tone_hz
    );
    if run.jitter_ms > 0 || run.loss > 0.0 {
        println!(
            "网络条件：抖动 0–{} ms / 丢包 {:.2}% / 种子 {}",
            run.jitter_ms,
            run.loss * 100.0,
            run.net_seed
        );
    }
    println!(
        "发送 {} 帧，收到 {} 帧，累计样本差 {}（{:+.1} ms）",
        run.sender_frames,
        run.receiver_frames,
        run.drift_samples,
        samples_to_ms(run.drift_samples as f64)
    );
    println!(
        "时间压缩 {} 样本 − 时间拉伸 {} 样本 = {:+.0}（应为 {:+.0}）",
        run.removed_samples, run.inserted_samples, run.net_removed_samples, run.drift_samples
    );
    println!(
        "缓冲深度 {:.2}–{:.2} ms，均值 {:.2}，p50 {:.1}，p95 {:.1}；目标 {} ms",
        run.buffered_ms_min,
        run.buffered_ms_max,
        run.buffered_ms_mean,
        run.buffered_ms_p50,
        run.buffered_ms_p95,
        run.target_delay_ms
    );
    println!(
        "下溢 {}（起播 {}）／缺帧 {}／帧长异常 {}／迟到 {}／清空 {}／抖动尖峰 {}",
        run.underruns,
        run.warmup_underruns,
        run.frames_missing,
        run.frames_off_size,
        run.late_packets,
        run.buffer_flushes,
        run.jitter_peaks
    );
    println!(
        "漂移补偿{}：丢 {} 样本／补 {} 样本，多拉 {} 次／少拉 {} 次，环水位变化 {:+} 样本",
        if run.compensate {
            "（开）"
        } else {
            "（关）"
        },
        run.drift_excised_samples,
        run.drift_inserted_samples,
        run.extra_pulls,
        run.skipped_pulls,
        run.ring_delta_samples
    );
    println!(
        "排队包数 均值 {:.2}／最小 {}／最大 {}；PCM 环丢样本 {}",
        run.packets_queued_mean,
        run.packets_queued_min,
        run.packets_queued_max,
        run.samples_dropped_full
    );
    println!(
        "PLC 自报 {} 样本（{} 次，上界）／守恒推导造出 {} 样本（{:+.1} ms，容差 ±4 帧）；网络丢 {} 样本",
        run.concealed_samples,
        run.concealment_events,
        run.fabricated_samples,
        samples_to_ms(run.fabricated_samples as f64),
        run.steady_lost_samples
    );
    println!(
        "硬跳变 {}／软跳变 {}／最大一阶差分 {:.4}（平滑上界 {:.4}）",
        run.hard_steps, run.soft_steps, run.max_step, run.step_bound
    );
    println!(
        "首窗 {:.2} ms → 末窗 {:.2} ms（含起播垫缓冲，{:+.2} ms）",
        run.windows.first().map_or(f64::NAN, |w| w.mean_buffered_ms),
        run.windows.last().map_or(f64::NAN, |w| w.mean_buffered_ms),
        run.drift_trend_ms
    );
    println!(
        "稳态：均值 {:.2} ms，摆动 {:.2} ms，爬升 {:+.2} ms（容差 ±{:.1} / {:.1} ms）",
        run.settled_mean_ms,
        run.settled_spread_ms,
        run.settled_trend_ms,
        TREND_TOLERANCE_MS,
        SPREAD_TOLERANCE_MS
    );

    let step = (run.windows.len() / 24).max(1);
    println!(
        "{:>8} {:>9} {:>8} {:>8} {:>6} {:>7} {:>7} {:>6} {:>6}",
        "起点s", "均值ms", "最小ms", "最大ms", "目标", "压缩ms", "PLC ms", "下溢", "硬跳"
    );
    for window in run.windows.iter().step_by(step) {
        println!(
            "{:>8.0} {:>9.2} {:>8.2} {:>8.2} {:>6} {:>7.2} {:>7.2} {:>6} {:>6}",
            window.start_s,
            window.mean_buffered_ms,
            window.min_buffered_ms,
            window.max_buffered_ms,
            window.target_ms,
            window.removed_ms,
            window.concealed_ms,
            window.underruns,
            window.hard_steps
        );
    }

    let bad = violations(run);
    if run.jitter_ms > 0 {
        println!(
            "（抖动链路不判摆动：实测 {:.2} ms，由网络决定，不是控制器的事）",
            run.settled_spread_ms
        );
    }
    if bad.is_empty() {
        println!("结论：四项判据全部满足。");
    } else {
        println!("结论：未通过——");
        for line in bad {
            println!("  · {line}");
        }
    }
}

/// `testkit drift-run`。
pub fn main(args: &[String]) -> Result<(), String> {
    let mut out: Option<String> = None;
    let mut hours = DEFAULT_HOURS;
    let mut ppm = DEFAULT_PPM;
    let mut min_ms = DEFAULT_MIN_DELAY_MS;
    let mut max_ms = DEFAULT_MAX_DELAY_MS;
    let mut compensate = true;
    let mut net = NetConditions::default();
    let mut tuning = DriftTuning::default();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => out = Some(crate::next_value(args, &mut i, "--out")?),
            "--hours" => {
                hours = crate::next_value(args, &mut i, "--hours")?
                    .parse()
                    .map_err(|e| format!("--hours 不是数字：{e}"))?;
            }
            "--ppm" => {
                ppm = crate::next_value(args, &mut i, "--ppm")?
                    .parse()
                    .map_err(|e| format!("--ppm 不是数字：{e}"))?;
            }
            "--min-ms" => {
                min_ms = crate::next_value(args, &mut i, "--min-ms")?
                    .parse()
                    .map_err(|e| format!("--min-ms 不是整数：{e}"))?;
            }
            "--max-ms" => {
                max_ms = crate::next_value(args, &mut i, "--max-ms")?
                    .parse()
                    .map_err(|e| format!("--max-ms 不是整数：{e}"))?;
            }
            "--jitter-ms" => {
                net.jitter_ms = crate::next_value(args, &mut i, "--jitter-ms")?
                    .parse()
                    .map_err(|e| format!("--jitter-ms 不是整数：{e}"))?;
            }
            "--loss" => {
                net.loss = crate::next_value(args, &mut i, "--loss")?
                    .parse()
                    .map_err(|e| format!("--loss 不是小数：{e}"))?;
            }
            "--seed" => {
                net.seed = crate::next_value(args, &mut i, "--seed")?
                    .parse()
                    .map_err(|e| format!("--seed 不是整数：{e}"))?;
            }
            "--alpha" => {
                tuning.ema_alpha = crate::next_value(args, &mut i, "--alpha")?
                    .parse()
                    .map_err(|e| format!("--alpha 不是小数：{e}"))?;
            }
            "--kp" => {
                tuning.kp = crate::next_value(args, &mut i, "--kp")?
                    .parse()
                    .map_err(|e| format!("--kp 不是小数：{e}"))?;
            }
            "--deadband" => {
                tuning.deadband_packets = crate::next_value(args, &mut i, "--deadband")?
                    .parse()
                    .map_err(|e| format!("--deadband 不是小数：{e}"))?;
            }
            "--no-compensate" => compensate = false,
            other => return Err(format!("drift-run 不认识参数 {other:?}")),
        }
        i += 1;
    }

    let run = run_full(hours, ppm, min_ms, max_ms, compensate, net, tuning)?;
    print_report(&run);

    if let Some(path) = out {
        let json = serde_json::to_string_pretty(&run).map_err(|e| e.to_string())?;
        if let Some(parent) = std::path::Path::new(&path).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("建目录 {} 失败：{e}", parent.display()))?;
        }
        std::fs::write(&path, json).map_err(|e| format!("写 {path} 失败：{e}"))?;
        println!("报告已写入 {path}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 5 分钟虚拟时间。debug 构建下约 20 秒。
    ///
    /// **不能再短了**：补偿器的滤波时间常数是 4 s、比例环也是 4 s，起播那一段（队列从 0 垫到
    /// 目标）要 20 s 才收干净；60 秒的跑法只有 1 个统计窗口，"稳态"里会整段混着起播过程，
    /// 测出来的是收敛速度，不是稳态行为。
    fn short(ppm: f64) -> Run {
        run(5.0 / 60.0, ppm, DEFAULT_MIN_DELAY_MS, DEFAULT_MAX_DELAY_MS).expect("跑 5 分钟")
    }

    #[test]
    fn zero_drift_keeps_the_buffer_flat() {
        let run = short(0.0);
        assert_eq!(run.drift_samples, 0, "0 ppm 不该有样本差");
        assert_eq!(run.underruns, 0);
        assert_eq!(run.hard_steps, 0);
        assert_eq!(run.settled_spread_ms, 0.0, "没有漂移时缓冲应当纹丝不动");
        assert!(
            run.settled_mean_ms <= f64::from(DEFAULT_MAX_DELAY_MS) + 1.0,
            "没有漂移却超预算：{:.2} ms",
            run.settled_mean_ms
        );
        assert_eq!(
            run.fabricated_samples, 0,
            "没有漂移、没有 PLC 时不该有任何样本被凭空造出来：{}",
            run.fabricated_samples
        );
    }

    #[test]
    fn compensation_holds_the_budget_for_a_fast_sender() {
        // P4-6 的正向验收：+1000 ppm（设计值的 10 倍）下四条判据必须全过。
        let fast = short(1_000.0);
        let bad = violations(&fast);
        assert!(bad.is_empty(), "正向 +1000 ppm 判据不过：{bad:?}");
        assert!(
            fast.drift_excised_samples > 0,
            "正向漂移一个样本都没丢，补偿器根本没动"
        );
        assert!(fast.extra_pulls > 0, "正向漂移一次都没多拉");
    }

    #[test]
    fn compensation_holds_the_budget_for_a_slow_sender() {
        // P4-6 的反向验收。第一版（C-35）在这里自激：补样本抬高 PCM 环，而「环太高就少拉」
        // 又降不下环（少拉写 480 = 环不变），于是永远少拉、队列涨到 18 包。
        let slow = short(-1_000.0);
        let bad = violations(&slow);
        assert!(bad.is_empty(), "反向 −1000 ppm 判据不过：{bad:?}");
        assert!(
            slow.drift_inserted_samples > 0,
            "反向漂移一个样本都没补，补偿器根本没动"
        );
        assert!(slow.skipped_pulls > 0, "反向漂移一次都没少拉");
        // 丢和补不能同时对打：那是控制环在自我追逐。
        assert!(
            slow.drift_excised_samples * 4 < slow.drift_inserted_samples,
            "反向漂移里丢了 {} 个样本、补了 {} 个——两个方向在对打",
            slow.drift_excised_samples,
            slow.drift_inserted_samples
        );
    }

    #[test]
    fn same_parameters_give_the_same_report() {
        let hours = 12.0 / 3_600.0;
        let a = run(hours, 100.0, 15, 20).expect("第一次");
        let b = run(hours, 100.0, 15, 20).expect("第二次");
        assert_eq!(a, b, "同参数必须逐位复现，否则长跑结论不可信");
    }
}
