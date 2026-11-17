//! 环回延迟测量：已知脉冲串 + 折叠匹配滤波（P2-2 的 `--impulse`）。
//!
//! # 为什么用匹配滤波，而不是「3 ms 窗 + 越阈找峰」
//!
//! 旧实现在每个候选位移上取 3 ms 窗做归一化相关，再按「最早一个超过全局峰 75% 的局部极大」定位。
//! 1 ms 的脉冲放进 3 ms 窗里，有 2/3 是静音（对相关度只有噪声贡献），相关度被稀释到 0.4–0.8；
//! 而「最早超过 75%」这条规则在多径反射下会在几个几乎等高的峰之间跳：
//! 实测同一台机器连续三次给出 11.8 / 4.0 / 9.5 ms——差 7.8 ms，比要测的量本身还大。
//!
//! 现在改成标准匹配滤波：
//!
//! 1. 模板就是**我们自己发出去的那 1 ms 脉冲**（不假设它的形状，也不需要知道扬声器/麦克风的响应）；
//! 2. 多个脉冲按周期**折叠**（相干叠加）之后再过一遍滤波，噪声降 √N，
//!    成本也从 `周期 × 脉冲数 × 窗长` 降到 `周期 × 窗长`；
//! 3. 峰值用抛物线插值细化到亚样本（1 样本 = 0.021 ms）；
//! 4. 定位规则改成「最早一个达到全局峰 90% 的局部极大」——直达声永远先到，多径只会更晚；
//! 5. 顺带输出**逐脉冲各自最佳位移的极差**（`click_spread_ms`）：这是单次测量内部的
//!    重复性证据，不用连跑十次就能看出结果稳不稳。
//!
//! # 仍然包含的未知量
//!
//! 测出来的位移 = 播放侧环形缓冲占用 + 设备缓冲 + 声学路径。前两项我们只知道自己推了多少、
//! 设备取走了多少（见 `apps/loopback` 的 `pump_impulse`），
//! 所以绝对值只能读作「这台机器的环回延迟」，不能读作「D1 的理论值」；
//! 要拆开得靠共享时钟（WASAPI `IAudioClock`），那是后续工作。

/// 参与折叠的脉冲数上限。再多也降不了多少噪声，反而扩大「末脉冲压到录音尾巴」的损失。
const MAX_CLICKS: usize = 8;

/// 接受一次测量的最低检测信噪比（峰值 ÷ 曲线中位数）。
///
/// 实测干净录音能到 1500×（约 63 dB），而纯噪声只有 3–5×；
/// 取 20×（26 dB）留足余量，同时挡掉「只测到底噪」的情况。
const MIN_DETECTION_SNR: f32 = 20.0;

/// 一次延迟估计。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DelayEstimate {
    /// 折叠匹配滤波的整数峰位置（样本）。
    pub lag_samples: usize,
    /// 抛物线插值后的亚样本位移。
    pub fractional_lag: f64,
    /// 换算成毫秒。用 `fractional_lag` 而不是整数峰：整数峰的分辨率是 0.021 ms，
    /// 单看它会掩盖亚样本级的变化。
    pub delay_ms: f64,
    /// 峰值归一化相关度（0–1，越大越可信）。
    pub correlation: f32,
    /// 录音峰值（线性）。麦克风被静音时这个数会接近 0，是排查「测不出延迟」的第一线索。
    pub recorded_peak: f32,
    /// 全局最高峰的位置（可能是反射/噪声伪峰，用于对照）。
    pub peak_lag_samples: usize,
    /// 全局最高峰的相关度。
    pub peak_correlation: f32,
    /// 实际参与折叠的脉冲数。
    pub clicks: usize,
    /// 检测信噪比：峰值 ÷ 整条曲线 `|score|` 的中位数。
    ///
    /// 这个数才是「有没有测到」的判据。归一化相关度（`correlation`）对幅度不敏感，
    /// 在低频漂移为主的录音里会饱和到 0.9 以上——实测踩过，见模块注释。
    pub detection_snr: f32,
    /// 发射序列里第一个脉冲的起点（样本），供时间戳通道换算播放时刻。
    pub emitted_click_index: usize,
    /// 录音里对应脉冲的起点（样本），供时间戳通道换算采集时刻。
    pub recorded_click_index: usize,
    /// 逐脉冲各自最佳位移的极差（毫秒）。这是**单次测量内部**的重复性；
    /// 大于 1 ms 说明录音里的脉冲不是同一个直达声，结果不能签。
    pub click_spread_ms: f64,
}

/// 生成周期脉冲串：每 `period` 个样本放一个 1 ms 的指数衰减脉冲。
#[must_use]
pub fn click_train(samples: usize, period: usize, sample_rate: u32, amplitude: f32) -> Vec<f32> {
    let mut out = vec![0.0_f32; samples];
    if period == 0 {
        return out;
    }
    let click_len = click_len(sample_rate);
    let mut position = 0;
    while position + click_len < samples {
        for i in 0..click_len {
            // 汉宁包络 × 整周期正弦：波形对称且**严格零均值**。
            //
            // 第一版用「半正弦 × 指数衰减」，是单极性波形（含很大的直流分量）：
            // 录音里任何缓慢漂移都能和它的直流部分对上，归一化相关在静音区就能到 0.9 以上，
            // 实测把底噪判成了「4 ms 的延迟」。对称零均值波形的自相关主瓣也更窄（0.5 ms），
            // 峰值位置对 1 样本的平移不再有系统性偏移。
            let phase = std::f32::consts::TAU * i as f32 / click_len as f32;
            let envelope = 0.5 - 0.5 * phase.cos();
            out[position + i] += amplitude * envelope * phase.sin();
        }
        position += period;
    }
    out
}

/// 一个脉冲的波形长度（样本）：1 ms。
#[must_use]
pub fn click_len(sample_rate: u32) -> usize {
    (sample_rate as usize / 1_000).max(1)
}

/// 相关度曲线（按 lag 索引），用于诊断「为什么没找到峰」。
///
/// 只在排查时调用：长度 `valid_max + 1`，元素是折叠后匹配滤波的归一化相关度。
#[must_use]
pub fn correlation_curve(
    recorded: &[f32],
    emitted: &[f32],
    max_lag: usize,
    sample_rate: u32,
) -> Vec<f32> {
    analyze(recorded, emitted, max_lag, sample_rate)
        .map(|curve| curve.scores)
        .unwrap_or_default()
}

/// 估算「发射序列第 0 个样本」到「录音里对应脉冲」的位移。
#[must_use]
pub fn estimate_delay(
    recorded: &[f32],
    emitted: &[f32],
    max_lag: usize,
    sample_rate: u32,
) -> Option<DelayEstimate> {
    let curve = analyze(recorded, emitted, max_lag, sample_rate)?;
    let (peak_lag, peak_score) = global_peak(&curve.scores);
    if peak_score <= 0.0 {
        return None;
    }
    let (lag, _) =
        earliest_strong_peak(&curve.scores, peak_score).unwrap_or((peak_lag, peak_score));

    // 判据是**检测信噪比**，不是相关度：相关度对幅度不敏感，会被低频漂移带偏（见模块注释）。
    let noise = median_abs(&curve.scores);
    let detection_snr = if noise > 0.0 {
        peak_score / noise
    } else {
        f32::MAX
    };
    if detection_snr < MIN_DETECTION_SNR {
        return None;
    }

    let fractional = refine(&curve.scores, lag);
    let spread_samples = click_spread(
        recorded,
        &curve.onsets,
        &curve.template,
        lag,
        spread_radius(sample_rate),
        curve.scores.len() - 1,
    );
    Some(DelayEstimate {
        lag_samples: lag,
        fractional_lag: fractional,
        delay_ms: fractional * 1000.0 / f64::from(sample_rate),
        correlation: local_correlation(recorded, &curve, lag),
        recorded_peak: recorded.iter().fold(0.0_f32, |acc, s| acc.max(s.abs())),
        peak_lag_samples: peak_lag,
        peak_correlation: local_correlation(recorded, &curve, peak_lag),
        clicks: curve.onsets.len(),
        detection_snr,
        emitted_click_index: curve.onsets[0],
        recorded_click_index: curve.onsets[0] + lag,
        click_spread_ms: spread_samples as f64 * 1000.0 / f64::from(sample_rate),
    })
}

/// 一次分析的中间产物：折叠后的相关度曲线、各脉冲起点、模板本身。
struct Curve {
    scores: Vec<f32>,
    onsets: Vec<usize>,
    template: Vec<f32>,
    template_energy: f64,
}

/// 折叠 + 匹配滤波。
///
/// 折叠的关键是**所有 lag 都要求全部脉冲参与**：只要最后一个脉冲的窗还在录音范围内，
/// 前面的脉冲必然也在（起点更小），所以可用的 lag 上界由最后一个脉冲决定。
fn analyze(recorded: &[f32], emitted: &[f32], max_lag: usize, sample_rate: u32) -> Option<Curve> {
    if sample_rate == 0 || max_lag == 0 {
        return None;
    }
    let template_len = click_len(sample_rate);
    let onsets = click_onsets(emitted, max_lag, template_len, MAX_CLICKS)?;
    let first = onsets[0];
    // 去掉模板的直流分量：波形本身已经是零均值的（汉宁 × 整周期正弦），这一步是保险——
    // 万一日后换了脉冲形状，直流偏置不至于又悄悄把判据压掉一个数量级。
    // 不去掉的话「一段常数」会和模板的直流部分对齐，把检测信噪比压掉一个数量级。
    let mut template = emitted.get(first..first + template_len)?.to_vec();
    let template_mean = template.iter().map(|s| f64::from(*s)).sum::<f64>() / template.len() as f64;
    for sample in &mut template {
        *sample -= template_mean as f32;
    }
    let template_energy: f64 = template.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
    if template_energy <= f64::EPSILON {
        return None;
    }

    let last = *onsets.last()?;
    let valid_max = recorded.len().checked_sub(last + template_len)?;
    if valid_max == 0 {
        return None;
    }
    let span = valid_max.min(max_lag) + template_len;

    let mut folded = vec![0.0_f32; span];
    for &onset in &onsets {
        let slice = recorded.get(onset..onset + span)?;
        for (slot, sample) in folded.iter_mut().zip(slice) {
            *slot += *sample;
        }
    }

    // 折叠结果也去直流：直流不携带任何延迟信息，留着只会抬高噪声底。
    let folded_mean = folded.iter().map(|s| f64::from(*s)).sum::<f64>() / folded.len() as f64;
    for sample in &mut folded {
        *sample -= folded_mean as f32;
    }

    // 能量加权（而不是逐窗归一化）是关键：录音里的低频漂移与脉冲同频段、只低 30 dB，
    // 逐窗归一化会把任何平滑窗都判成 0.9 以上的「脉冲」。除以整条录音的能量范数之后，
    // 分数直接反映「这一窗有多少能量与模板匹配」，噪声窗自然掉到 1e-4 量级。
    let norm = template_energy.sqrt();
    let signal_norm: f64 = folded
        .iter()
        .map(|s| f64::from(*s) * f64::from(*s))
        .sum::<f64>()
        .sqrt();
    if signal_norm <= f64::EPSILON {
        return None;
    }
    let mut scores = Vec::with_capacity(span - template_len + 1);
    for window in folded.windows(template_len) {
        scores.push((centered_dot(&template, window) / (norm * signal_norm)) as f32);
    }
    Some(Curve {
        scores,
        onsets,
        template,
        template_energy,
    })
}

/// 脉冲起点：第一个绝对幅度超过 −120 dBFS 的样本。
///
/// 用绝对门限而不是「峰值的百分比」：发射序列是我们自己生成的，脉冲之外是**精确的 0**，
/// 所以这里拿到的是脉冲真正的起点，不随振幅漂移。
fn click_onsets(
    emitted: &[f32],
    period: usize,
    template_len: usize,
    limit: usize,
) -> Option<Vec<usize>> {
    if period == 0 {
        return None;
    }
    let first = emitted.iter().position(|s| s.abs() > 1e-6)?;
    let mut onsets = Vec::with_capacity(limit);
    let mut position = first;
    while onsets.len() < limit && position + template_len <= emitted.len() {
        onsets.push(position);
        position += period;
    }
    if onsets.is_empty() {
        None
    } else {
        Some(onsets)
    }
}

/// 去掉窗口均值之后再点乘。
///
/// 窗口的直流分量不携带任何延迟信息；不去掉的话「一段常数」会和模板的残余直流对上，
/// 而且模板与窗口的均值不一致时还会让峰值平移一个样本。
fn centered_dot(template: &[f32], window: &[f32]) -> f64 {
    let count = window.len() as f64;
    let mean = window.iter().map(|s| f64::from(*s)).sum::<f64>() / count;
    template
        .iter()
        .zip(window)
        .map(|(t, s)| f64::from(*t) * (f64::from(*s) - mean))
        .sum()
}

/// `|score|` 的中位数：整条曲线的「噪声底」。拿它做分母，得到与幅度无关的检测信噪比。
fn median_abs(scores: &[f32]) -> f32 {
    if scores.is_empty() {
        return 0.0;
    }
    let mut values: Vec<f32> = scores.iter().map(|score| score.abs()).collect();
    values.sort_by(f32::total_cmp);
    values[values.len() / 2]
}

/// 该 lag 处「波形有多像模板」的归一化相关度（0–1）。
///
/// 只作质量参考，不作判据：它对幅度不敏感，低频漂移也能拿到 0.9 以上。
fn local_correlation(recorded: &[f32], curve: &Curve, lag: usize) -> f32 {
    let start = curve.onsets[0] + lag;
    let Some(window) = recorded.get(start..start + curve.template.len()) else {
        return 0.0;
    };
    let energy: f64 = window.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
    if energy <= f64::EPSILON {
        return 0.0;
    }
    (centered_dot(&curve.template, window) / (curve.template_energy.sqrt() * energy.sqrt())) as f32
}

fn global_peak(scores: &[f32]) -> (usize, f32) {
    scores
        .iter()
        .enumerate()
        .fold((0_usize, f32::MIN), |acc, (index, score)| {
            if *score > acc.1 { (index, *score) } else { acc }
        })
}

/// 取**最早**出现的强峰：声音总是先到直达声，再有多径反射。
///
/// 「强」的门限是全局峰的 90%。旧实现用 75%，在反射比直达声略弱时会跳到反射峰上；
/// 匹配滤波把主瓣压窄到 1–2 个样本之后，0.9 这条线不再有漏掉直达声的风险。
fn earliest_strong_peak(scores: &[f32], peak_score: f32) -> Option<(usize, f32)> {
    let threshold = peak_score * 0.9;
    if scores.first().is_some_and(|score| *score >= threshold) {
        return Some((0, scores[0]));
    }
    for lag in 1..scores.len().saturating_sub(1) {
        let score = scores[lag];
        if score >= threshold && score >= scores[lag - 1] && score >= scores[lag + 1] {
            return Some((lag, score));
        }
    }
    None
}

/// 抛物线插值：用峰两侧的样本把位移细化到亚样本。
fn refine(scores: &[f32], lag: usize) -> f64 {
    if lag == 0 || lag + 1 >= scores.len() {
        return lag as f64;
    }
    let left = f64::from(scores[lag - 1]);
    let mid = f64::from(scores[lag]);
    let right = f64::from(scores[lag + 1]);
    let denominator = left - 2.0 * mid + right;
    if denominator.abs() < 1e-12 {
        return lag as f64;
    }
    let delta = 0.5 * (left - right) / denominator;
    // 抛物线顶点落在相邻样本之外说明三点不构成单峰（噪声），老实用整数峰。
    if delta.is_finite() && delta.abs() <= 0.5 {
        lag as f64 + delta
    } else {
        lag as f64
    }
}

/// 逐脉冲扫描的半径（样本）：5 ms。
///
/// 只需覆盖「同一脉冲在不同次测量里的抖动」，不需要覆盖整条曲线；
/// 半径给大了反而会把反射峰扫进来，把 `click_spread_ms` 变成假警报。
fn spread_radius(sample_rate: u32) -> usize {
    (sample_rate as usize / 200).max(1)
}

/// 逐脉冲各自的最佳位移极差（样本）。
fn click_spread(
    recorded: &[f32],
    onsets: &[usize],
    template: &[f32],
    lag: usize,
    radius: usize,
    valid_max: usize,
) -> usize {
    let mut best: Vec<usize> = Vec::with_capacity(onsets.len());
    for &onset in onsets {
        let low = lag.saturating_sub(radius);
        let high = (lag + radius).min(valid_max);
        let mut best_lag = lag;
        let mut best_score = f32::MIN;
        for candidate in low..=high {
            let start = onset + candidate;
            let Some(window) = recorded.get(start..start + template.len()) else {
                continue;
            };
            let score = centered_dot(template, window) as f32;
            if score > best_score {
                best_score = score;
                best_lag = candidate;
            }
        }
        best.push(best_lag);
    }
    if best.len() < 2 {
        return 0;
    }
    let low = best.iter().copied().min().unwrap_or(0);
    let high = best.iter().copied().max().unwrap_or(0);
    high - low
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: u32 = 48_000;

    /// 确定性伪随机噪声（不用外部 crate，测试要能复现）。
    fn noise(seed: u64) -> f32 {
        let mut x = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
        x ^= x >> 29;
        ((x >> 40) as f32 / 8_388_608.0) - 1.0
    }

    /// 录音 = 延迟 + 衰减 + 一条反射 + 噪声。
    fn simulate(
        emitted: &[f32],
        delay: usize,
        reflection: Option<(usize, f32)>,
        level: f32,
    ) -> Vec<f32> {
        let mut recorded = vec![0.0_f32; emitted.len()];
        for (i, sample) in emitted.iter().enumerate() {
            let target = i + delay;
            if target < recorded.len() {
                recorded[target] += level * sample;
            }
            if let Some((offset, gain)) = reflection {
                let echo = target + offset;
                if echo < recorded.len() {
                    recorded[echo] += gain * level * sample;
                }
            }
        }
        for (index, sample) in recorded.iter_mut().enumerate() {
            *sample += 0.004 * noise(index as u64 + 17);
        }
        recorded
    }

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
        // 正弦从 0 起，所以第一个越阈样本落在脉冲开头几毫秒内，而不是正好 0。
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

    /// 3 秒素材 + 500 ms 周期 = 6 个脉冲，与 `--impulse --seconds 3` 的真实配置一致。
    const TRAIN_SAMPLES: usize = 3 * FS as usize;
    const PERIOD: usize = 24_000;

    #[test]
    fn estimate_delay_recovers_known_offset() {
        let period = PERIOD;
        let emitted = click_train(TRAIN_SAMPLES, period, FS, 0.8);
        let delay = 123_usize;
        let recorded = simulate(&emitted, delay, None, 0.4);

        let estimate = estimate_delay(&recorded, &emitted, period, FS).expect("应当找到峰值");
        // 1 ms 的窄带脉冲其自相关主瓣宽约 1 ms，整数峰在 ±1 样本内本身就有歧义（0.021 ms），
        // 精确位置靠抛物线插值给出——所以判据看 fractional_lag，不看整数峰。
        assert!(
            (estimate.fractional_lag - delay as f64).abs() < 0.5,
            "位移偏太多：{}（期望 {delay}）",
            estimate.fractional_lag
        );
        assert!(
            estimate.correlation > 0.9,
            "相关度太低：{}",
            estimate.correlation
        );
        assert_eq!(estimate.clicks, 6, "3 秒 / 500 ms 应有 6 个脉冲");
        assert!(
            estimate.click_spread_ms <= 0.05,
            "干净仿真里逐脉冲极差应当接近 0：{}",
            estimate.click_spread_ms
        );
    }

    /// 这是 P2-3 的判据：同一条录音重复估 20 次（只换噪声种子），结果必须落在 ±1 样本内。
    #[test]
    fn estimate_is_repeatable_across_noise() {
        let period = PERIOD;
        let emitted = click_train(TRAIN_SAMPLES, period, FS, 0.8);
        let delay = 311_usize;
        let mut lags = Vec::new();
        for seed in 0..20_u64 {
            let mut recorded = simulate(&emitted, delay, Some((420, 0.55)), 0.3);
            for (index, sample) in recorded.iter_mut().enumerate() {
                *sample += 0.01 * noise(index as u64 + seed * 9_973);
            }
            let estimate = estimate_delay(&recorded, &emitted, period, FS).expect("应当找到峰值");
            lags.push(estimate.lag_samples);
        }
        let low = *lags.iter().min().unwrap();
        let high = *lags.iter().max().unwrap();
        assert!(
            high - low <= 1,
            "重复性不够：极差 {} 样本（{lags:?}）",
            high - low
        );
        assert!(
            (low as i64 - delay as i64).abs() <= 2,
            "系统偏差太大：{} vs {delay}（{lags:?}）",
            low
        );
    }

    /// 反射比直达声弱但很接近时，不能挑到反射峰上——那正是旧实现 ±8 ms 抖动的来源。
    #[test]
    fn direct_path_wins_over_strong_reflection() {
        let period = PERIOD;
        let emitted = click_train(TRAIN_SAMPLES, period, FS, 0.8);
        let delay = 200_usize;
        // 反射晚 4 ms、只弱 10%：相关度几乎等高，靠「最早」这条规则区分。
        let recorded = simulate(&emitted, delay, Some((192, 0.9)), 0.5);
        let estimate = estimate_delay(&recorded, &emitted, period, FS).expect("应当找到峰值");
        assert!(
            (estimate.fractional_lag - delay as f64).abs() < 1.0,
            "挑到了反射峰：位移 {}（期望 {delay}，反射在 {}）",
            estimate.fractional_lag,
            delay + 192
        );
    }

    /// 亚样本位移（123.5 样本）也要能读出来：整数峰不足以表达 0.01 ms 级的差别。
    #[test]
    fn sub_sample_delay_is_interpolated() {
        let period = PERIOD;
        let emitted = click_train(TRAIN_SAMPLES, period, FS, 0.8);
        // 用线性插值造出半样本位移
        let delay = 123_usize;
        let mut recorded = vec![0.0_f32; emitted.len()];
        for i in 0..emitted.len() - delay - 1 {
            recorded[i + delay] += 0.5 * emitted[i];
            recorded[i + delay + 1] += 0.5 * emitted[i];
        }
        let estimate = estimate_delay(&recorded, &emitted, period, FS).expect("应当找到峰值");
        assert!(
            (estimate.fractional_lag - (delay as f64 + 0.5)).abs() < 0.2,
            "亚样本插值不准：{}",
            estimate.fractional_lag
        );
    }

    /// 录音里叠一段比脉冲低 28 dB 的低频漂移（直流 + 1 Hz 起伏 + 47 Hz 嗡声），
    /// 这正是硬件上真实录音的样子：逐窗归一化相关会被它带偏，能量加权不会。
    ///
    /// 已知极限：如果漂移是**与脉冲同频段的强单音**（例如 380 Hz、只低 20 dB），
    /// 检测信噪比会掉到阈值以下——此时工具拒绝给数（`None`），而不是给一个错数。
    #[test]
    fn low_frequency_drift_does_not_fake_a_click() {
        let emitted = click_train(TRAIN_SAMPLES, PERIOD, FS, 0.8);
        let delay = 900_usize;
        let mut recorded = simulate(&emitted, delay, None, 0.3);
        for (index, sample) in recorded.iter_mut().enumerate() {
            let t = index as f32 / FS as f32;
            *sample += 0.020;
            *sample += 0.010 * (std::f32::consts::TAU * 1.0 * t).sin();
            *sample += 0.010 * (std::f32::consts::TAU * 47.0 * t).sin();
        }
        let estimate = estimate_delay(&recorded, &emitted, PERIOD, FS).expect("应当找到峰值");
        assert!(
            (estimate.fractional_lag - delay as f64).abs() < 0.5,
            "挑到漂移上了：位移 {}（期望 {delay}，全局峰 {}）",
            estimate.fractional_lag,
            estimate.peak_lag_samples
        );
        assert!(
            estimate.detection_snr > MIN_DETECTION_SNR,
            "检测信噪比太低：{}",
            estimate.detection_snr
        );
    }

    /// 只有漂移、没有脉冲时必须拒绝给数：宁可不给，也不给假数字。
    #[test]
    fn drift_alone_is_rejected() {
        let emitted = click_train(TRAIN_SAMPLES, PERIOD, FS, 0.8);
        let mut recorded = vec![0.0_f32; TRAIN_SAMPLES];
        for (index, sample) in recorded.iter_mut().enumerate() {
            let t = index as f32 / FS as f32;
            *sample += 0.020;
            *sample += 0.010 * (std::f32::consts::TAU * 1.0 * t).sin();
            *sample += 0.010 * (std::f32::consts::TAU * 47.0 * t).sin();
            *sample += 0.002 * noise(index as u64 + 5);
        }
        assert!(
            estimate_delay(&recorded, &emitted, PERIOD, FS).is_none(),
            "纯漂移不该被判成脉冲"
        );
    }

    #[test]
    fn silence_has_no_estimate() {
        let emitted = click_train(1_000, 100, FS, 0.5);
        let recorded = vec![0.0_f32; 1_000];
        assert!(estimate_delay(&recorded, &emitted, 500, FS).is_none());
    }
}
