//! 报告结构（P1-4 固定 schema，`docs/measurements.md` 直接粘）。
//!
//! 字段名一旦定下就不要改：历史报告要能和新报告并排比较，否则「趋势」这条就断了。

use std::collections::BTreeMap;

use serde::Serialize;

/// 一帧处理耗时的分位数（毫秒）。
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Stats {
    /// 中位数。
    pub p50: f64,
    /// 95 分位。
    pub p95: f64,
    /// 最大值。`measurements.md` 的格式要求 p50/p95/max 三列都给，缺 max 就得回头补测。
    pub max: f64,
}

/// 延迟构成（毫秒）。
#[derive(Debug, Clone, Serialize)]
pub struct Latency {
    /// 链路算法延迟合计：各 stage 自报延迟 + 编解码前瞻。
    pub algorithm_ms: f64,
    /// 逐环节延迟，按档位名索引。
    pub per_stage_ms: BTreeMap<String, f64>,
}

/// 一次离线回放的完整报告。
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// 链路档位（`[]` 表示直通）。
    pub chain: Vec<String>,
    /// 编解码描述，如 `opus:10ms:32k`。
    pub codec: String,
    /// 延迟构成。
    pub latency: Latency,
    /// 每帧耗时（毫秒），键是环节名。
    pub cpu_ms_per_frame: BTreeMap<String, Stats>,
    /// 整段素材的平均 VAD 概率。
    pub vad_mean: f32,
    /// 热路径内的分配次数。**必须为 0**。
    pub allocations_in_hot_path: usize,
    /// 输出 wav 路径（`--out` 未给时为 `null`）。
    pub wav_out: Option<String>,

    // ---- 以下是 P1 追加的字段：schema 只增不改 ----
    /// 参与统计的帧数（不含 warmup）。
    pub frames: usize,
    /// 被丢弃的暖机帧数。
    pub warmup_frames: usize,
    /// `release` 或 `debug`。debug 的数字没有参考价值，报告里必须自曝。
    pub profile: &'static str,
    /// 编码包长均值（字节）。
    pub packet_bytes_mean: f64,
    /// 实测线上码率（bit/s，按 100 pps 折算）。
    pub bitrate_bps: f64,
}

impl Report {
    /// 序列化成带缩进的 JSON（人要看，也要能直接粘进文档）。
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }
}

/// 从一组毫秒值算 p50/p95。
///
/// 只在测量循环**之外**调用：内部会排序，不能混进热路径。
#[must_use]
pub fn stats(mut values: Vec<f64>) -> Stats {
    if values.is_empty() {
        return Stats {
            p50: 0.0,
            p95: 0.0,
            max: 0.0,
        };
    }
    values.sort_unstable_by(f64::total_cmp);
    Stats {
        p50: percentile(&values, 0.50),
        p95: percentile(&values, 0.95),
        max: values[values.len() - 1],
    }
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    let last = sorted.len() - 1;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let index = ((last as f64) * q).round() as usize;
    sorted[index.min(last)]
}
