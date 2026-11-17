//! 离线回归基线（P3-5）：**只存指标，不存 wav**。
//!
//! 为什么存指标而不是音频：wav 没法进 git diff，二进制一多就没人看；而「这条链路在这段素材上
//! 输出多少 dBFS、发多少字节、VAD 均值多少、算法延迟多少、热路径分配几次」这几个数字，任何一个
//! 变了都说明 DSP 或链路被改动了。
//!
//! 存进去的指标全部是**确定性**的（同一份输入、同一份代码，逐位可复现），所以它能在 `cargo test`
//! 里当断言用；CPU 耗时是噪声，故意不进来（那类回归用 `measurements.md` 的趋势看）。
//!
//! 用法：
//! - 有意改动 DSP 后，重新生成：`cargo run -p testkit -- baseline --out testdata/baseline.json`
//! - 平时由 `crates/testkit/tests/baseline.rs` 自动比对，超差即失败。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::run::{parse_codec, run_samples};
use crate::{report, wav};

/// 基线覆盖的素材（`testdata/synthetic/` 下的文件）。
pub const MATERIALS: [&str; 6] = ["clean", "babble", "fan", "keyboard", "chair", "noise_only"];

/// 基线覆盖的链路。前三条单档用来定位回归，最后一条是 L1 档的完整链路。
pub const CHAINS: [&str; 4] = ["none", "highpass", "rnn", "highpass,rnn,agc2"];

/// 每条用例取多少帧（3 s）。够长到能反映稳态，又短到能在 `cargo test` 里跑完。
pub const FRAMES: usize = 300;

/// 基线文件格式版本。指标口径变了就加一，旧基线会被明确拒绝而不是悄悄放过。
pub const VERSION: u32 = 1;

/// 一条用例的指标。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Case {
    /// 素材名（不含路径与后缀）。
    pub material: String,
    /// 链路描述。
    pub chain: String,
    /// 输出 RMS（dBFS）。DSP 改动最直接的症状就是这个数变了。
    pub rms_dbfs: f64,
    /// 编码后平均包长（字节）。比特率/编码器改动会体现在这里。
    pub packet_bytes_mean: f64,
    /// 平均 VAD 概率。降噪器或 VAD 的判决变了会体现在这里。
    pub vad_mean: f64,
    /// 链路算法延迟（毫秒，含 Opus 前瞻）。延迟预算的唯一记账口径。
    pub algorithm_ms: f64,
    /// 热路径分配次数，必须为 0。
    pub allocations_in_hot_path: usize,
}

/// 基线文件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    /// 格式版本。
    pub version: u32,
    /// 每条用例的帧数。
    pub frames: usize,
    /// 全部用例。
    pub cases: Vec<Case>,
}

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

/// 合成素材的默认位置。用 `CARGO_MANIFEST_DIR` 拼绝对路径，避免依赖当前工作目录
/// （`cargo run` / `cargo test` 的 CWD 不一样，相对路径会踩空）。
#[must_use]
pub fn default_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(crate::SYNTHETIC_DIR)
}

/// 跑一遍全部用例，产出基线。
pub fn collect(root: &Path) -> Result<Baseline, String> {
    let codec = parse_codec("opus:10ms:32k")?;
    let mut cases = Vec::with_capacity(MATERIALS.len() * CHAINS.len());
    for material in MATERIALS {
        let path = root.join(format!("{material}_48k_mono.wav"));
        let samples = wav::read_mono_f32(&path)?;
        let take = (FRAMES * audio_core::FRAME_SAMPLES).min(samples.len());
        let slice = &samples[..take];
        for chain in CHAINS {
            let desc = audio_core::ChainDesc::parse(chain).map_err(|e| e.to_string())?;
            let outcome = run_samples(slice, &desc, codec, 0)?;
            cases.push(Case {
                material: (*material).to_owned(),
                chain: (*chain).to_owned(),
                rms_dbfs: rms_dbfs(&outcome.samples),
                packet_bytes_mean: outcome.report.packet_bytes_mean,
                vad_mean: f64::from(outcome.report.vad_mean),
                algorithm_ms: outcome.report.latency.algorithm_ms,
                allocations_in_hot_path: outcome.report.allocations_in_hot_path,
            });
        }
    }
    Ok(Baseline {
        version: VERSION,
        frames: FRAMES,
        cases,
    })
}

/// 一条超差。
#[derive(Debug, Clone, PartialEq)]
pub struct Violation {
    /// 用例标识（`素材/链路`）。
    pub case: String,
    /// 指标名。
    pub metric: &'static str,
    /// 基线值。
    pub expected: f64,
    /// 本次值。
    pub actual: f64,
    /// 允许的绝对偏差。
    pub tolerance: f64,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} 的 {} 从 {:.4} 漂到 {:.4}（允许 ±{:.4}）",
            self.case, self.metric, self.expected, self.actual, self.tolerance
        )
    }
}

/// 各指标的允许偏差。定得紧一点：这些数字本来就是确定性的，松了就没法当回归闸门。
pub const TOL_RMS_DB: f64 = 0.25;
/// 包长允许 ±3%（编码器小版本升级会动这个数）。
pub const TOL_PACKET_BYTES: f64 = 0.03;
/// VAD 均值允许 ±0.02。
pub const TOL_VAD: f64 = 0.02;

/// 比对基线，返回全部超差项（空 = 通过）。
#[must_use]
pub fn compare(baseline: &Baseline, current: &Baseline) -> Vec<Violation> {
    let mut violations = Vec::new();
    if baseline.version != current.version {
        violations.push(Violation {
            case: "基线文件".to_owned(),
            metric: "version",
            expected: f64::from(baseline.version),
            actual: f64::from(current.version),
            tolerance: 0.0,
        });
        return violations;
    }
    for expected in &baseline.cases {
        let key = format!("{}/{}", expected.material, expected.chain);
        let Some(actual) = current
            .cases
            .iter()
            .find(|c| c.material == expected.material && c.chain == expected.chain)
        else {
            violations.push(Violation {
                case: key,
                metric: "missing",
                expected: 0.0,
                actual: 0.0,
                tolerance: 0.0,
            });
            continue;
        };
        let mut push = |metric: &'static str, expected: f64, actual: f64, tolerance: f64| {
            if (expected - actual).abs() > tolerance {
                violations.push(Violation {
                    case: key.clone(),
                    metric,
                    expected,
                    actual,
                    tolerance,
                });
            }
        };
        push("rms_dbfs", expected.rms_dbfs, actual.rms_dbfs, TOL_RMS_DB);
        push(
            "packet_bytes_mean",
            expected.packet_bytes_mean,
            actual.packet_bytes_mean,
            expected.packet_bytes_mean * TOL_PACKET_BYTES,
        );
        push("vad_mean", expected.vad_mean, actual.vad_mean, TOL_VAD);
        // 算法延迟必须**完全**一致：差一毫秒都是预算表失真。
        push(
            "algorithm_ms",
            expected.algorithm_ms,
            actual.algorithm_ms,
            0.0,
        );
        // 分配次数必须为 0，且不允许「比基线更多」。
        if actual.allocations_in_hot_path != 0 {
            violations.push(Violation {
                case: key.clone(),
                metric: "allocations_in_hot_path",
                expected: 0.0,
                actual: actual.allocations_in_hot_path as f64,
                tolerance: 0.0,
            });
        }
    }
    violations
}

/// `testkit baseline --out <json>` 或 `--check <json>`。
pub fn main(args: &[String]) -> Result<(), String> {
    let mut out: Option<String> = None;
    let mut check: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => out = Some(crate::next_value(args, &mut i, "--out")?),
            "--check" => check = Some(crate::next_value(args, &mut i, "--check")?),
            other => return Err(format!("baseline 不认识参数 {other:?}")),
        }
        i += 1;
    }
    let current = collect(&default_root())?;
    if let Some(path) = out {
        let json = serde_json::to_string_pretty(&current).map_err(|e| e.to_string())?;
        std::fs::write(&path, json).map_err(|e| format!("写 {path} 失败：{e}"))?;
        println!("基线已写入 {path}（{} 条用例）", current.cases.len());
    }
    if let Some(path) = check {
        let text = std::fs::read_to_string(&path).map_err(|e| format!("读 {path} 失败：{e}"))?;
        let baseline: Baseline = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let violations = compare(&baseline, &current);
        if violations.is_empty() {
            println!("基线一致（{} 条用例）", baseline.cases.len());
        } else {
            for violation in &violations {
                println!("超差：{violation}");
            }
            return Err(format!("{} 项超差", violations.len()));
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn _report_type_check(_r: &report::Report) {}
