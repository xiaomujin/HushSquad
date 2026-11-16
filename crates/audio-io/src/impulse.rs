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
/// # 为什么不做全量互相关
///
/// 10 秒 48 kHz 素材 × 24 000 个 lag = 115 亿次乘加，跑一次几十秒，还不一定能放进实时使用。
/// 而脉冲串是**已知**的：第一个脉冲在发射序列里的位置是固定的，
/// 所以只要在录音里找到第一个越阈样本，两者相减就是位移，O(n)。
///
/// `max_lag` 传一个脉冲周期：位移超过一个周期就意味着漏掉了第一个脉冲，结果有歧义，直接返回 `None`。
/// 剩下的可信度由 `correlation` 给出——它只在首个脉冲附近的窗口里算（几毫秒），代价可以忽略。
#[must_use]
pub fn estimate_delay(
    recorded: &[f32],
    emitted: &[f32],
    max_lag: usize,
    sample_rate: u32,
) -> Option<DelayEstimate> {
    if sample_rate == 0 {
        return None;
    }
    let first_recorded = first_peak(recorded)?;
    let first_emitted = first_peak(emitted)?;
    if first_recorded < first_emitted {
        return None;
    }
    let lag = first_recorded - first_emitted;
    if lag > max_lag {
        // 超过一个周期：多半是漏了第一个脉冲，或者脉冲根本没发出去。
        return None;
    }

    // 首个脉冲附近 43 ms 的窗口（覆盖 1 ms 脉冲 + 扬声器/房间尾巴）
    let window = 2_048
        .min(recorded.len() - first_recorded)
        .min(emitted.len() - first_emitted);
    if window == 0 {
        return None;
    }
    let mut dot = 0.0_f64;
    let mut energy_emitted = 0.0_f64;
    let mut energy_recorded = 0.0_f64;
    for i in 0..window {
        let a = f64::from(emitted[first_emitted + i]);
        let b = f64::from(recorded[first_recorded + i]);
        dot += a * b;
        energy_emitted += a * a;
        energy_recorded += b * b;
    }
    if energy_emitted <= f64::EPSILON || energy_recorded <= f64::EPSILON {
        return None;
    }
    let correlation = (dot / (energy_emitted.sqrt() * energy_recorded.sqrt())) as f32;
    // 相关度太低说明「找到的第一个峰」只是噪声：宁可不给数字，也不给假数字。
    if correlation < 0.3 {
        return None;
    }

    Some(DelayEstimate {
        lag_samples: lag,
        correlation,
        delay_ms: lag as f64 * 1000.0 / f64::from(sample_rate),
        recorded_peak: recorded.iter().fold(0.0_f32, |acc, s| acc.max(s.abs())),
    })
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
