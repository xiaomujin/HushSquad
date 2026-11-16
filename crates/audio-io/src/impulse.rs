//! 环回延迟测量：已知脉冲串 + 归一化互相关（P2-2 的 `--impulse`）。
//!
//! # 方法与其局限（写在这里，免得以后把数字当真值）
//!
//! 播放一条**周期**脉冲串（默认 500 ms 一个），同时录音；把录音和发射序列做互相关，
//! 峰值位置就是「发射序列的第 0 个样本」到「录音里的第 0 个样本」的位移。
//!
//! 这个位移包含三部分：
//! 1. 播放侧环形缓冲的占用量（我们知道自己推了多少、设备取走了多少）；
//! 2. 设备/驱动的采集与播放缓冲（**不知道**，正是要测的东西）；
//! 3. 声学路径（外放→麦克风时才有意义）。
//!
//! 因此输出里必须同时给出 `uncertainty_ms`：它由「播放环形缓冲占用 + 一个回调缓冲」估出来。
//! 想要更准，就得用 cpal 回调时间戳（`InputCallbackInfo::timestamp().capture` 与
//! `OutputCallbackInfo::timestamp().playback`）跨流比较——那是 P2-3 的后续，不是现在。

/// 一次延迟估计。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DelayEstimate {
    /// 互相关峰对应的位移（样本）。
    pub lag_samples: usize,
    /// 归一化互相关峰值（0–1，越大越可信）。
    pub correlation: f32,
    /// 换算成毫秒。
    pub delay_ms: f64,
    /// 录音峰值（线性）。麦克风被静音时这个数会接近 0，是排查「测不出延迟」的第一线索。
    pub recorded_peak: f32,
    /// 全局最高峰的位置（可能是反射/噪声伪峰，用于对照）。
    pub peak_lag_samples: usize,
    /// 全局最高峰的相关度。
    pub peak_correlation: f32,
}

/// 生成周期脉冲串：每 `period` 个样本放一个 1 ms 的指数衰减脉冲。
#[must_use]
pub fn click_train(samples: usize, period: usize, sample_rate: u32, amplitude: f32) -> Vec<f32> {
    let mut out = vec![0.0_f32; samples];
    if period == 0 {
        return out;
    }
    let click_len = (sample_rate as usize / 1_000).max(1); // 1 ms
    let mut position = 0;
    while position + click_len < samples {
        for i in 0..click_len {
            let envelope = (-(i as f32) / (click_len as f32 / 4.0)).exp();
            // 半正弦载波，能量集中在 0–2 kHz，避开扬声器与麦克风最差的低频端
            let carrier = (std::f32::consts::PI * i as f32 / click_len as f32).sin();
            out[position + i] += amplitude * envelope * carrier;
        }
        position += period;
    }
    out
}

/// 估算「发射序列第 0 个样本」到「录音里对应脉冲」的位移。
///
/// # 算法
///
/// 脉冲串是我们自己生成的，所以它在发射序列里的位置是**已知的**：
/// `first, first+P, first+2P, …`（P = `max_lag`，传一个脉冲周期）。
/// 于是只需要对每个候选位移 lag 打一个分：把前几个脉冲的短窗（3 ms）跟录音里 `脉冲位置 + lag` 的短窗做归一化相关，
/// 多个脉冲取平均。
///
/// 代价：`周期 × 脉冲数 × 窗长` = 24 000 × 8 × 144 ≈ 2 800 万次乘加，毫秒级；
/// 而全量互相关是 115 亿次，几十秒——这是「能用」和「不能用」的差别。
///
/// 用多个脉冲而不是「找第一个越阈样本」，是因为实测里 0.2 振幅的脉冲会被房间噪声淹没，
/// 第一个越阈样本几乎总是噪声；多脉冲平均能把 6 dB 左右的信噪比挖出来。
/// 相关度曲线（按 lag 索引），用于诊断「为什么没找到峰」。
///
/// 只在排查时调用：长度 `max_lag + 1`，每个元素是各脉冲短窗归一化相关度的平均。
#[must_use]
pub fn correlation_curve(
    recorded: &[f32],
    emitted: &[f32],
    max_lag: usize,
    sample_rate: u32,
) -> Vec<f32> {
    score_curve(recorded, emitted, max_lag, sample_rate).unwrap_or_default()
}

#[must_use]
pub fn estimate_delay(
    recorded: &[f32],
    emitted: &[f32],
    max_lag: usize,
    sample_rate: u32,
) -> Option<DelayEstimate> {
    if sample_rate == 0 || max_lag == 0 {
        return None;
    }
    let first_emitted = first_peak(emitted)?;
    // 3 ms 的比对窗：覆盖 1 ms 脉冲 + 扬声器与房间的早期反射
    let window = ((sample_rate as usize / 1_000) * 3).max(32);

    // 最多用 8 个脉冲；少于 2 个就退回单脉冲（相关度会虚高，但总比没有好）。
    let mut clicks: Vec<usize> = Vec::with_capacity(8);
    let mut position = first_emitted;
    while clicks.len() < 8 && position + window <= emitted.len() {
        clicks.push(position);
        position += max_lag;
    }
    if clicks.is_empty() {
        return None;
    }

    let scores = score_curve(recorded, emitted, max_lag, sample_rate)?;

    // 全局最高峰只当诊断：它经常是房间反射或噪声，实测在 0.4 左右就会出现 200 ms 的伪峰。
    let (peak_lag, peak_score) = scores.iter().enumerate().fold(
        (0_usize, f32::MIN),
        |acc, (i, s)| {
            if *s > acc.1 { (i, *s) } else { acc }
        },
    );

    // 取**最早**出现的强峰：声音总是先到直达声，再有多径反射。
    let threshold = (peak_score * 0.75).max(0.35);
    let mut chosen = None;
    for lag in 1..scores.len().saturating_sub(1) {
        let score = scores[lag];
        if score >= threshold && score >= scores[lag - 1] && score >= scores[lag + 1] {
            chosen = Some((lag, score));
            break;
        }
    }
    let (lag, correlation) = chosen.unwrap_or((peak_lag, peak_score));

    // 相关度太低说明「找到的峰」只是噪声：宁可不给数字，也不给假数字。
    if correlation < 0.35 {
        return None;
    }

    Some(DelayEstimate {
        lag_samples: lag,
        correlation,
        delay_ms: lag as f64 * 1000.0 / f64::from(sample_rate),
        recorded_peak: recorded.iter().fold(0.0_f32, |acc, s| acc.max(s.abs())),
        peak_lag_samples: peak_lag,
        peak_correlation: peak_score,
    })
}

/// 找出信号里所有脉冲的位置（越阈后在其后 1 ms 内取峰值）。
///
/// 取「峰」而不是「首个越阈样本」：越阈点取决于振幅与噪声，峰是波形特征，抖动小一个量级。
#[must_use]
pub fn click_positions(signal: &[f32], threshold: f32, min_spacing: usize) -> Vec<usize> {
    let mut positions = Vec::new();
    let search = 48.min(signal.len()); // 1 ms
    let mut index = 0;
    while index < signal.len() {
        if signal[index].abs() <= threshold {
            index += 1;
            continue;
        }
        let end = (index + search).min(signal.len());
        let mut best = index;
        let mut best_value = signal[index].abs();
        for (offset, sample) in signal[index..end].iter().enumerate() {
            if sample.abs() > best_value {
                best_value = sample.abs();
                best = index + offset;
            }
        }
        positions.push(best);
        index = best + min_spacing.max(1);
    }
    positions
}

/// 逐 lag 打分：对每个候选位移，把前几个脉冲的短窗跟录音对齐，算归一化相关度再平均。
fn score_curve(
    recorded: &[f32],
    emitted: &[f32],
    max_lag: usize,
    sample_rate: u32,
) -> Option<Vec<f32>> {
    if sample_rate == 0 || max_lag == 0 {
        return None;
    }
    let first_emitted = first_peak(emitted)?;
    let window = ((sample_rate as usize / 1_000) * 3).max(32);

    let mut clicks: Vec<usize> = Vec::with_capacity(8);
    let mut position = first_emitted;
    while clicks.len() < 8 && position + window <= emitted.len() {
        clicks.push(position);
        position += max_lag;
    }
    if clicks.is_empty() {
        return None;
    }

    let mut scores = vec![f32::MIN; max_lag + 1];
    for (lag, slot) in scores.iter_mut().enumerate() {
        let mut score = 0.0_f32;
        let mut used = 0_usize;
        for &click in &clicks {
            let start = click + lag;
            if start + window > recorded.len() {
                break;
            }
            let mut dot = 0.0_f64;
            let mut energy_emitted = 0.0_f64;
            let mut energy_recorded = 0.0_f64;
            for i in 0..window {
                let a = f64::from(emitted[click + i]);
                let b = f64::from(recorded[start + i]);
                dot += a * b;
                energy_emitted += a * a;
                energy_recorded += b * b;
            }
            if energy_emitted <= f64::EPSILON || energy_recorded <= f64::EPSILON {
                continue;
            }
            score += (dot / (energy_emitted.sqrt() * energy_recorded.sqrt())) as f32;
            used += 1;
        }
        if used > 0 {
            *slot = score / used as f32;
        }
    }
    Some(scores)
}

/// 第一个超过「峰值 20%」的样本位置。
///
/// 绝对门限取 −60 dBFS（1e-3）：虚拟声卡与静音麦克风只有底噪，绝不能被当成脉冲。
fn first_peak(signal: &[f32]) -> Option<usize> {
    let peak = signal.iter().fold(0.0_f32, |acc, s| acc.max(s.abs()));
    if peak < 1e-3 {
        return None;
    }
    let threshold = peak * 0.2;
    signal.iter().position(|s| s.abs() > threshold)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: u32 = 48_000;

    #[test]
    fn click_train_has_expected_period() {
        let period = 4_800; // 100 ms
        let train = click_train(FS as usize, period, FS, 0.5);
        let peaks: Vec<usize> = train
            .iter()
            .enumerate()
            .filter(|(_, s)| s.abs() > 0.05)
            .map(|(i, _)| i)
            .collect();
        assert!(!peaks.is_empty());
        // 半正弦载波从 0 起，所以第一个越阈样本落在脉冲开头几毫秒内，而不是正好 0。
        assert!(
            peaks[0] < 48,
            "第一个脉冲应在开头 1 ms 内，实际 {}",
            peaks[0]
        );
        // 第二个脉冲起点应在 period 附近（脉冲本身有 1 ms 长度）
        assert!(
            peaks.iter().any(|i| (period..period + 48).contains(i)),
            "没找到第二个脉冲：{peaks:?}"
        );
    }

    #[test]
    fn estimate_delay_recovers_known_offset() {
        let period = 24_000; // 500 ms
        let emitted = click_train(FS as usize, period, FS, 0.8);
        let delay = 123_usize;

        // 录音 = 延迟 + 噪声 + 衰减
        let mut recorded = vec![0.0_f32; emitted.len()];
        for i in 0..emitted.len() - delay {
            recorded[i + delay] = 0.4 * emitted[i];
        }
        let mut noise = 0.0_f32;
        for sample in &mut recorded {
            noise = 0.98 * noise + 0.02 * ((*sample * 7919.0).sin());
            *sample += 0.002 * noise;
        }

        let estimate = estimate_delay(&recorded, &emitted, period, FS).expect("应当找到峰值");
        assert_eq!(estimate.lag_samples, delay);
        assert!(
            estimate.correlation > 0.9,
            "相关度太低：{}",
            estimate.correlation
        );
        assert!(
            (estimate.delay_ms - 2.5625).abs() < 1e-3,
            "{} ms",
            estimate.delay_ms
        );
    }

    #[test]
    fn silence_has_no_estimate() {
        let emitted = click_train(1_000, 100, FS, 0.5);
        let recorded = vec![0.0_f32; 1_000];
        assert!(estimate_delay(&recorded, &emitted, 500, FS).is_none());
    }
}
