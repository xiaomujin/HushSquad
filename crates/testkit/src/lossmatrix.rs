//! 丢包矩阵（P4-3）：弱网下「还能不能听」，用 SNRseg 当代理指标。
//!
//! # 为什么是 SNRseg 而不是 PESQ
//!
//! PESQ / POLQA 是**授权**算法，没有可信的离线 Rust 实现；自己拍一个「感知分数」等于伪造
//! 结论。所以这里用能逐位复现、能写进测试的 SNRseg（分段信噪比，逐 10 ms 帧算、限幅到
//! −10…35 dB 后取均值），并且**明确承认它不是感知分数**：它衡量「波形有多像」，对丢包引入
//! 的爆音、断续、金属声只是间接敏感。真正的「可听性」判决留给 D6 盲听（P3-6）。
//!
//! # 参考信号为什么是「编解码往返」而不是原始素材
//!
//! 直接拿原始素材当参考，32 kbps Opus 自己就要吃掉约 17.5 dB（P1-3 实测），丢包的损伤会被
//! 编解码噪声淹掉。所以参考 = **同一份素材、同一套编码器、无损网络下的往返结果**：这样
//! SNRseg 只反映网络损伤。0 % 丢包时 SNRseg 应当接近上限（这是本模块的自检）。
//!
//! # 两种模式
//!
//! - `Plc`：现在的实现——丢掉就丢，NetEQ 自己用 PLC（外推）补一帧；
//! - `Redundant`：每个包**发两遍**（模型上等价于 100 % 冗余的 RED：两个副本独立丢失，
//!   恢复概率 1 − p²），代价是带宽翻倍。
//!
//! # 为什么不是 Opus 的 in-band FEC
//!
//! 试过了，**接不上**：`neteq` 的 `AudioDecoder` trait 只有 `decode(encoded) -> Vec<f32>`，
//! 没有 FEC 入口；而 Opus 的 in-band FEC 要求「丢包时用**下一包**解出当前帧」，得到的是
//! **PCM**，NetEQ 需要的是**包**才能排进它自己的播放队列。要接上只有两条路：改 neteq 的
//! trait，或者放弃它的时间伸缩自己写缓冲。两条都不是「顺手」的量级，所以 P4-3 先量出
//! 「冗余值多少 dB」，再决定要不要为 FEC 动 neteq。

use serde::{Deserialize, Serialize};

use audio_core::receive::{IncomingPacket, ReceivePath};
use audio_core::{AudioCodec, FRAME_SAMPLES, MAX_PACKET_BYTES, OpusCodec};

use crate::netsim::{NetSim, NetSimConfig};
use crate::{baseline, wav};

/// 矩阵默认覆盖的丢包率（%）。3 % 是「家用宽带的坏日子」，10 % 是「4G 边缘」。
pub const DEFAULT_LOSS: [f32; 5] = [0.0, 0.01, 0.03, 0.05, 0.10];

/// 默认素材与帧数（3 s）。
pub const DEFAULT_MATERIAL: &str = "clean";

/// 默认取多少帧。
pub const DEFAULT_FRAMES: usize = 300;

/// 默认种子。
pub const DEFAULT_SEED: u64 = 42;

/// 接收方标识（单发送方）。
const SENDER: u32 = 0x4853_5101;

/// 对齐搜索的最大延迟：200 ms。再大就不是「抖动缓冲」而是「串话了」。
const MAX_LAG_SAMPLES: usize = 9_600;

/// 起播瞬态跳过 200 ms：NetEQ 要先把目标延迟垫起来，头几帧是 expand/merge 出来的，不是稳态。
///
/// 实测依据：0 % 丢包时输出与参考本该逐样本相同，全局 SNR 却只有 32.6 dB——差的就是这段起播。
const SKIP_START_SAMPLES: usize = 9_600;

/// 对齐块长：20 帧 = 200 ms。块越短越能跟上时间轴漂移，但每块参与打分的帧越少。
const BLOCK_FRAMES: usize = 20;

/// 除第一块外，每块只在上一块延迟的 ±60 ms 内搜索（2880 样本）。
///
/// 收窄范围既省时间，也避免在损伤严重的块里跳到远处一个假的互相关峰。
const LAG_SEARCH_RADIUS: usize = 2_880;

/// 只有参考信号能量高于这个电平的帧才参与 SNRseg（静音帧除零没意义）。
const ACTIVE_RMS_DBFS: f64 = -50.0;

/// SNRseg 的限幅区间。上限 35 dB（超过就是「一样」，不再奖励），下限 −10 dB（避免个别帧主导均值）。
const SNRSEG_FLOOR_DB: f64 = -10.0;

/// SNRseg 的上限，同上。
const SNRSEG_CEIL_DB: f64 = 35.0;

/// 抖动缓冲目标延迟上限（毫秒）：附录 A 第 9 行 L2 档的上界。
const TARGET_DELAY_MS: u32 = 20;

/// 一条用例的指标。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Case {
    /// 素材名。
    pub material: String,
    /// 配置的丢包率（0–1）。
    pub loss_configured: f64,
    /// 实测丢包率（`NetSim` 真正丢掉的 ÷ 送入的）。
    pub loss_measured: f64,
    /// 是否开了「每包发两遍」的冗余。
    pub redundancy: bool,
    /// 分段信噪比（dB，限幅 −10…35）。**不是**感知分数。
    pub snrseg_db: f64,
    /// 全局信噪比（dB，不限幅），用来看整体波形偏差。
    pub snr_db: f64,
    /// 逐帧能量差的均值（dB）。对残余错位不敏感，见 [`Score::energy_err_db`]。
    pub energy_err_db: f64,
    /// 各块对齐延迟的极差（毫秒）。**不是**延迟真的涨了——见 [`Case::buffered_ms_max`]。
    ///
    /// 它唯一的用途是自曝置信度：这个数越大，说明逐块对齐越不可靠，SNRseg 被残余错位压得越低。
    /// 所以高丢包下 SNRseg 应当读作**保真度下界**，而不是精确值。
    pub lag_spread_ms: f64,
    /// 抖动缓冲深度峰值（毫秒），NetEQ 自报。
    ///
    /// 这是**直接**测出来的排队量，和 `lag_drift_ms`（从对齐推出来的）互为佐证：
    /// 两者一起大，说明「延迟真的涨了」，而不是对齐估歪了。
    pub buffered_ms_max: u32,
    /// 各块对齐延迟的均值（样本）。起播缓冲 + 网络延迟的总和。
    pub lag_samples: usize,
    /// 各块对齐延迟的最小值（样本）。
    pub lag_min_samples: usize,
    /// 各块对齐延迟的最大值（样本）。`max − min` 就是时间轴漂移量。
    pub lag_max_samples: usize,
    /// 参与 SNRseg 的帧数。
    pub frames_compared: usize,
    /// 抖动缓冲解码出的帧数。
    pub frames_decoded: u64,
    /// 抖动缓冲报缺帧的次数（还没起播、或彻底断流）。
    pub frames_missing: u64,
    /// 帧长不是 480 的次数（时间伸缩的痕迹）。
    pub frames_off_size: u64,
    /// 音频侧下溢次数。
    pub underruns: u64,
    /// 序列号空洞推断出的丢包数。
    pub packets_lost: u64,
    /// 输出 RMS（dBFS）。PLC 会让能量塌下去，这个数能看出来。
    pub output_rms_dbfs: f64,
    /// 线上码率（bps），含冗余开销。
    pub bitrate_bps: f64,
}

/// 整个矩阵。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Matrix {
    /// 格式版本。指标口径变了就加一。
    pub version: u32,
    /// 每条用例的帧数。
    pub frames: usize,
    /// 随机种子。
    pub seed: u64,
    /// 全部用例。
    pub cases: Vec<Case>,
}

/// 当前矩阵格式版本。
pub const VERSION: u32 = 1;

fn rms_dbfs(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return -120.0;
    }
    let energy: f64 = samples.iter().map(|x| f64::from(*x) * f64::from(*x)).sum();
    let rms = (energy / samples.len() as f64).sqrt();
    if rms <= 0.0 {
        -120.0
    } else {
        20.0 * rms.log10()
    }
}

/// 一个块的得分：把 `reference[start..end]` 与 `output[start+lag..end+lag]` 对齐后算 SNR。
fn block_snr(reference: &[f32], output: &[f32], start: usize, end: usize, lag: usize) -> f64 {
    let n = end.min(output.len().saturating_sub(lag));
    if n <= start {
        return f64::NEG_INFINITY;
    }
    let mut signal = 0.0_f64;
    let mut noise = 0.0_f64;
    for i in start..n {
        let x = f64::from(reference[i]);
        let y = f64::from(output[i + lag]);
        signal += x * x;
        let d = x - y;
        noise += d * d;
    }
    if noise <= 0.0 {
        return 200.0;
    }
    10.0 * (signal / noise).log10()
}

/// 给一个块找最佳延迟：先步长 8 粗搜、再逐样本细搜。`hint` 是上一块的延迟，用来收窄搜索范围。
fn align_block(
    reference: &[f32],
    output: &[f32],
    start: usize,
    end: usize,
    hint: Option<usize>,
) -> usize {
    let max_valid = output.len().saturating_sub(end);
    let (mut low, mut high) = match hint {
        Some(previous) => (
            previous.saturating_sub(LAG_SEARCH_RADIUS),
            previous + LAG_SEARCH_RADIUS,
        ),
        None => (0, MAX_LAG_SAMPLES),
    };
    high = high.min(max_valid).min(MAX_LAG_SAMPLES);
    low = low.min(high);
    // 结尾附近可用范围会被 output 长度掐窄。窗口太窄时搜出来的东西没有意义——实测踩过：
    // 最后一块的搜索区间塌到 0，于是整块按 lag = 0 对齐，把 0 % 丢包的全局 SNR 从「无损」
    // 拉低到 10.9 dB。宁可沿用上一块的延迟，也不要在塌缩的区间里瞎猜。
    if high.saturating_sub(low) < LAG_SEARCH_RADIUS / 4 {
        // 不 clamp 到 max_valid：调用方用 `end.min(output.len() - lag)` 兜住越界。
        return hint.unwrap_or(0);
    }

    let mut best = (low, f64::NEG_INFINITY);
    let mut lag = low;
    while lag <= high {
        let score = block_snr(reference, output, start, end, lag);
        if score > best.1 {
            best = (lag, score);
        }
        lag += 8;
    }
    let coarse = best.0;
    let fine_low = coarse.saturating_sub(8).max(low);
    let fine_high = (coarse + 8).min(high);
    for lag in fine_low..=fine_high {
        let score = block_snr(reference, output, start, end, lag);
        if score > best.1 {
            best = (lag, score);
        }
    }
    best.0
}

/// 对齐 + 打分的完整结果。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Score {
    /// 分段信噪比（dB，限幅 −10…35）。
    pub snrseg_db: f64,
    /// 全局信噪比（dB，不限幅）。
    pub snr_db: f64,
    /// 平均对齐延迟（样本）。
    pub lag_samples: usize,
    /// 各块延迟的最小值（样本）。
    pub lag_min: usize,
    /// 各块延迟的最大值（样本）。
    pub lag_max: usize,
    /// 参与 SNRseg 的帧数。
    pub frames_compared: usize,
    /// 逐帧能量差的均值（dB）。
    ///
    /// 为什么还要这个：SNRseg 比的是**波形**，对齐差几个样本就会塌（1 kHz 在 48 kHz 下偏
    /// 5 个样本就是 37° 相位）。而 NetEQ 丢包后会 `expand` + `accelerate`，块内的时间轴
    /// 本来就不是常数，残余错位躲不掉。帧能量差只看 10 ms 的能量，错位几个样本几乎不影响，
    /// 所以它更能反映「有没有断续/塌陷」这类真实损伤。两个数一起看，结论才稳。
    pub energy_err_db: f64,
}

/// 逐块对齐后再打分。
///
/// **为什么必须分块**：丢包会让 NetEQ 先 `expand`（插一帧）再 `accelerate`（抽掉几毫秒）把
/// 延迟拉回目标值，于是输出时间轴相对参考是**会漂的**。用单一全局延迟去对齐，漂几十毫秒就
/// 全盘错位——实测过：600 帧里只丢 6 个包，全局对齐的 SNRseg 从 34.9 dB 掉到 1.6 dB，
/// 那不是丢包的损伤，是对错了位。分块（200 ms）各估各的延迟，顺带把「漂了多少」变成可报告的
/// 数字（`lag_max − lag_min`）。
pub fn blockwise_score(reference: &[f32], output: &[f32]) -> Score {
    let mut hint: Option<usize> = None;
    let mut lag_sum = 0.0_f64;
    let mut lag_min = usize::MAX;
    let mut lag_max = 0_usize;
    let mut blocks = 0_usize;
    let mut frame_sum = 0.0_f64;
    let mut frames_compared = 0_usize;
    let mut signal_all = 0.0_f64;
    let mut noise_all = 0.0_f64;
    let mut energy_sum = 0.0_f64;
    let mut energy_frames = 0_usize;

    let mut start = SKIP_START_SAMPLES.min(reference.len());
    while start + FRAME_SAMPLES <= reference.len() {
        let end = (start + BLOCK_FRAMES * FRAME_SAMPLES).min(reference.len());
        let lag = align_block(reference, output, start, end, hint);
        hint = Some(lag);

        let n = end.min(output.len().saturating_sub(lag));
        let mut cursor = start;
        while cursor + FRAME_SAMPLES <= n {
            let mut signal = 0.0_f64;
            let mut noise = 0.0_f64;
            let mut measured_signal = 0.0_f64;
            for i in cursor..cursor + FRAME_SAMPLES {
                let x = f64::from(reference[i]);
                let y = f64::from(output[i + lag]);
                signal += x * x;
                measured_signal += y * y;
                let d = x - y;
                noise += d * d;
            }
            signal_all += signal;
            noise_all += noise;
            let mean_square = signal / FRAME_SAMPLES as f64;
            let level_db = if mean_square <= 0.0 {
                -120.0
            } else {
                10.0 * mean_square.log10()
            };
            if level_db > ACTIVE_RMS_DBFS {
                let snr = if noise <= 0.0 {
                    SNRSEG_CEIL_DB
                } else {
                    10.0 * (signal / noise).log10()
                };
                frame_sum += snr.clamp(SNRSEG_FLOOR_DB, SNRSEG_CEIL_DB);
                frames_compared += 1;
                let measured_db = if measured_signal <= 0.0 {
                    -120.0
                } else {
                    10.0 * (measured_signal / FRAME_SAMPLES as f64).log10()
                };
                energy_sum += (level_db - measured_db).abs();
                energy_frames += 1;
            }
            cursor += FRAME_SAMPLES;
        }

        lag_sum += lag as f64;
        lag_min = lag_min.min(lag);
        lag_max = lag_max.max(lag);
        blocks += 1;
        start = end;
    }

    let mean_lag = if blocks == 0 {
        0
    } else {
        (lag_sum / blocks as f64).round() as usize
    };
    Score {
        snrseg_db: if frames_compared == 0 {
            f64::NAN
        } else {
            frame_sum / frames_compared as f64
        },
        snr_db: if noise_all <= 0.0 {
            200.0
        } else {
            10.0 * (signal_all / noise_all).log10()
        },
        lag_samples: mean_lag,
        lag_min: if blocks == 0 { 0 } else { lag_min },
        lag_max,
        frames_compared,
        energy_err_db: if energy_frames == 0 {
            f64::NAN
        } else {
            energy_sum / energy_frames as f64
        },
    }
}

/// 跑一条用例。
pub fn run_case(
    samples: &[f32],
    material: &str,
    loss: f32,
    redundancy: bool,
    seed: u64,
) -> Result<Case, String> {
    let frames = samples.len() / FRAME_SAMPLES;
    if frames == 0 {
        return Err("输入不足一帧（480 样本）".to_owned());
    }

    // 第一遍：编码成包，同时用「编码即解码」得到无损网络的参考信号。
    let mut codec = OpusCodec::with_defaults().map_err(|e| e.to_string())?;
    let mut packet = [0_u8; MAX_PACKET_BYTES];
    let mut packets: Vec<Vec<u8>> = Vec::with_capacity(frames);
    let mut reference: Vec<f32> = Vec::with_capacity(frames * FRAME_SAMPLES);
    for chunk in samples.as_chunks::<FRAME_SAMPLES>().0 {
        let frame: audio_core::Frame = *chunk;
        let len = codec.encode(&frame, &mut packet);
        if len == 0 {
            return Err("编码失败".to_owned());
        }
        packets.push(packet[..len].to_vec());
        let mut decoded = [0.0_f32; FRAME_SAMPLES];
        if !codec.decode(&packet[..len], false, &mut decoded) {
            return Err("参考信号解码失败".to_owned());
        }
        reference.extend_from_slice(&decoded);
    }

    // 第二遍：过网络模拟器 + 接收路径。
    let (mut tx, mut path, mut out) =
        ReceivePath::new(SENDER, 64, 8).map_err(|e| format!("构造接收路径失败：{e}"))?;
    path.set_delay_limits(0, TARGET_DELAY_MS);
    let mut net = NetSim::new(NetSimConfig {
        loss,
        jitter_ms: 0,
        reorder: 0.0,
        seed,
    });

    let copies = if redundancy { 2 } else { 1 };
    let mut output: Vec<f32> = Vec::with_capacity(frames * FRAME_SAMPLES + 4_800);
    let mut wire_bytes = 0_usize;
    let mut buffered_max = 0_u32;
    let mut now = 0_u64;
    let tick_and_pull = |path: &mut ReceivePath,
                         out: &mut audio_core::receive::PcmReceiver,
                         output: &mut Vec<f32>| {
        path.tick();
        let mut frame = [0.0_f32; FRAME_SAMPLES];
        if out.pull_frame(&mut frame) {
            output.extend_from_slice(&frame);
        }
    };

    for (index, payload) in packets.iter().enumerate() {
        now = index as u64 * 10;
        let seq = index as u16;
        let timestamp = (index * FRAME_SAMPLES) as u32;
        for _ in 0..copies {
            let pkt = IncomingPacket::new(SENDER, seq, timestamp, payload);
            for (_, delivered) in net.step(pkt, now) {
                // 环满只可能是上游没跟上，离线工具里直接忽略（发送侧会计数）。
                let _ = tx.send(delivered);
            }
            wire_bytes += payload.len();
        }
        tick_and_pull(&mut path, &mut out, &mut output);
        buffered_max = buffered_max.max(path.buffered_ms());
    }

    // 收尾：把还在路上的取完，再跑 160 ms 让抖动缓冲把尾巴吐干净。
    for _ in 0..16 {
        now += 10;
        for (_, delivered) in net.drain(now) {
            let _ = tx.send(delivered);
        }
        tick_and_pull(&mut path, &mut out, &mut output);
    }

    let stats = path.stats();
    // 收尾那 16 帧是「流结束」的 PLC 垃圾，没有对应的参考信号，别让它污染打分。
    let trimmed = &output[..output.len().min(reference.len())];
    let score = blockwise_score(&reference, trimmed);

    let duration_s = frames as f64 * 0.01;
    Ok(Case {
        material: material.to_owned(),
        loss_configured: f64::from(loss),
        loss_measured: net.measured_loss(),
        redundancy,
        snrseg_db: score.snrseg_db,
        snr_db: score.snr_db,
        energy_err_db: score.energy_err_db,
        lag_spread_ms: (score.lag_max.saturating_sub(score.lag_min)) as f64 / 48.0,
        buffered_ms_max: buffered_max,
        lag_samples: score.lag_samples,
        lag_min_samples: score.lag_min,
        lag_max_samples: score.lag_max,
        frames_compared: score.frames_compared,
        frames_decoded: stats.frames_decoded,
        frames_missing: stats.frames_missing,
        frames_off_size: stats.frames_off_size,
        underruns: out.underruns(),
        packets_lost: stats.packets_lost,
        output_rms_dbfs: rms_dbfs(&output),
        bitrate_bps: wire_bytes as f64 * 8.0 / duration_s,
    })
}

/// 跑整个矩阵。
pub fn collect(
    root: &std::path::Path,
    material: &str,
    frames: usize,
    losses: &[f32],
    redundancy: bool,
    seed: u64,
) -> Result<Matrix, String> {
    let path = root.join(format!("{material}_48k_mono.wav"));
    let samples = wav::read_mono_f32(&path)?;
    let take = (frames * FRAME_SAMPLES).min(samples.len());
    let slice = &samples[..take];
    let mut cases = Vec::with_capacity(losses.len());
    for loss in losses {
        cases.push(run_case(slice, material, *loss, redundancy, seed)?);
    }
    Ok(Matrix {
        version: VERSION,
        frames: take / FRAME_SAMPLES,
        seed,
        cases,
    })
}

/// `testkit loss-matrix`。
pub fn main(args: &[String]) -> Result<(), String> {
    let mut out: Option<String> = None;
    let mut material = DEFAULT_MATERIAL.to_owned();
    let mut frames = DEFAULT_FRAMES;
    let mut losses: Vec<f32> = DEFAULT_LOSS.to_vec();
    let mut seed = DEFAULT_SEED;
    let mut redundancy = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => out = Some(crate::next_value(args, &mut i, "--out")?),
            "--material" => material = crate::next_value(args, &mut i, "--material")?,
            "--frames" => {
                frames = crate::next_value(args, &mut i, "--frames")?
                    .parse()
                    .map_err(|e| format!("--frames 不是整数：{e}"))?;
            }
            "--seed" => {
                seed = crate::next_value(args, &mut i, "--seed")?
                    .parse()
                    .map_err(|e| format!("--seed 不是整数：{e}"))?;
            }
            "--loss" => {
                let raw = crate::next_value(args, &mut i, "--loss")?;
                losses = raw
                    .split(',')
                    .map(|part| {
                        let value: f32 = part
                            .trim()
                            .parse()
                            .map_err(|e| format!("丢包率 {part:?} 解析失败：{e}"))?;
                        if !(0.0..=1.0).contains(&value) {
                            return Err(format!("丢包率 {value} 超出 0–1（用小数，10% 写 0.1）"));
                        }
                        Ok(value)
                    })
                    .collect::<Result<Vec<f32>, String>>()?;
            }
            "--redundancy" => redundancy = true,
            other => return Err(format!("loss-matrix 不认识参数 {other:?}")),
        }
        i += 1;
    }

    let matrix = collect(
        &baseline::default_root(),
        &material,
        frames,
        &losses,
        redundancy,
        seed,
    )?;

    println!(
        "素材 {} / {} 帧 / 种子 {} / 冗余 {}",
        material,
        matrix.frames,
        seed,
        if redundancy { "开" } else { "关" }
    );
    println!(
        "{:>8} {:>8} {:>7} {:>7} {:>7} {:>7} {:>8} {:>8} {:>8}",
        "丢包配置",
        "丢包实测",
        "SNRseg",
        "全局SNR",
        "能量误差",
        "对齐极差",
        "缓冲峰值",
        "输出RMS",
        "码率bps"
    );
    for case in &matrix.cases {
        println!(
            "{:>9.1}% {:>8.2}% {:>7.2} {:>7.2} {:>7.2} {:>7.2} {:>7}ms {:>8.2} {:>8.0}",
            case.loss_configured * 100.0,
            case.loss_measured * 100.0,
            case.snrseg_db,
            case.snr_db,
            case.energy_err_db,
            case.lag_spread_ms,
            case.buffered_ms_max,
            case.output_rms_dbfs,
            case.bitrate_bps
        );
    }

    if let Some(path) = out {
        let json = serde_json::to_string_pretty(&matrix).map_err(|e| e.to_string())?;
        if let Some(parent) = std::path::Path::new(&path).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("建目录 {} 失败：{e}", parent.display()))?;
        }
        std::fs::write(&path, json).map_err(|e| format!("写 {path} 失败：{e}"))?;
        println!("矩阵已写入 {path}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 3 秒素材，够出结论又短到能在 debug 构建里跑。
    fn material() -> Vec<f32> {
        let path = baseline::default_root().join("clean_48k_mono.wav");
        let samples = wav::read_mono_f32(&path).expect("读素材失败");
        let take = (100 * FRAME_SAMPLES).min(samples.len());
        samples[..take].to_vec()
    }

    #[test]
    fn no_loss_is_nearly_transparent() {
        // 自检：链路上没有任何损伤时，输出必须和参考几乎一样。
        // 如果这条红了，问题在测量管线（对齐、抖动缓冲配置、样本流），不在丢包。
        let case = run_case(&material(), "clean", 0.0, false, DEFAULT_SEED).expect("跑用例失败");
        assert_eq!(case.packets_lost, 0, "0 % 丢包却数出丢包");
        assert!(
            case.snrseg_db > 20.0,
            "0 % 丢包的 SNRseg = {:.2} dB，测量管线有问题（对齐 {} 样本）",
            case.snrseg_db,
            case.lag_samples
        );
    }

    #[test]
    fn loss_monotonically_degrades_quality() {
        // 丢得越多越差。不要求严格单调（随机丢包有涨落），但 10 % 必须明显差于 0 %。
        let samples = material();
        let clean = run_case(&samples, "clean", 0.0, false, DEFAULT_SEED).expect("0 %");
        let bad = run_case(&samples, "clean", 0.10, false, DEFAULT_SEED).expect("10 %");
        assert!(
            bad.snrseg_db < clean.snrseg_db - 1.0,
            "10 % 丢包 SNRseg = {:.2} dB，只比 0 % 的 {:.2} dB 低一点点",
            bad.snrseg_db,
            clean.snrseg_db
        );
        assert!(bad.packets_lost > 0, "10 % 丢包却一个都没数出来");
    }

    #[test]
    fn same_seed_is_bit_identical() {
        let samples = material();
        let a = run_case(&samples, "clean", 0.05, false, 7).expect("第一次");
        let b = run_case(&samples, "clean", 0.05, false, 7).expect("第二次");
        assert_eq!(a, b, "同种子必须逐位复现，否则弱网结论不可信");
    }

    #[test]
    fn redundancy_recovers_single_losses() {
        // 每包发两遍：单个丢包会被另一份补上，所以丢包数不变但 SNRseg 应当更高。
        let samples = material();
        let plain = run_case(&samples, "clean", 0.10, false, DEFAULT_SEED).expect("无冗余");
        let dup = run_case(&samples, "clean", 0.10, true, DEFAULT_SEED).expect("有冗余");
        assert!(
            dup.snrseg_db > plain.snrseg_db,
            "冗余没换来质量：有冗余 {:.2} dB ≤ 无冗余 {:.2} dB",
            dup.snrseg_db,
            plain.snrseg_db
        );
        assert!(
            dup.bitrate_bps > plain.bitrate_bps * 1.5,
            "冗余的带宽代价没体现出来：{:.0} vs {:.0} bps",
            dup.bitrate_bps,
            plain.bitrate_bps
        );
    }
}
