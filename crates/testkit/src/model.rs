//! 延迟模型自检（P1-6）：把技术设计附录 A 的预算表用代码复算一遍。
//!
//! 为什么值得做：后面每改一次链路，这个数字自动更新，人就不会凭感觉争论延迟。
//!
//! 两件事：
//! 1. 复算附录 A 的三档预测（L0/L1/L2），与设计文档给的 p50/p95 对照，偏差一目了然；
//! 2. 把**当前链路**逐行代进表里，得到它自己的 p50/p95 —— 换档位时数字自动跟着变。

use audio_core::{ChainDesc, StageKind};

/// 附录 A 的一行。
struct Row {
    label: &'static str,
    l0: (f64, f64),
    l1: (f64, f64),
    l2: (f64, f64),
}

const fn row(label: &'static str, l0: (f64, f64), l1: (f64, f64), l2: (f64, f64)) -> Row {
    Row { label, l0, l1, l2 }
}

/// 技术设计 附录 A（v0.2）逐行抄录。改表必须同时改这里，否则模型会悄悄过期。
const ROWS: [Row; 11] = [
    row(
        "1 采集缓冲（设备/驱动）",
        (3.0, 5.0),
        (5.0, 10.0),
        (5.0, 10.0),
    ),
    row(
        "2 帧成组（10 ms 帧，取均值）",
        (0.0, 5.0),
        (0.0, 10.0),
        (0.0, 10.0),
    ),
    row("3 降噪", (0.0, 0.0), (10.0, 10.0), (30.0, 30.0)),
    row("4 AGC2 + 限幅", (0.0, 1.0), (0.0, 2.0), (0.0, 2.0)),
    row("5 Opus 编码前瞻", (6.5, 6.5), (6.5, 6.5), (6.5, 6.5)),
    row("6 发送排队（pacing）", (0.0, 1.0), (0.0, 2.0), (0.0, 2.0)),
    row(
        "7 网络单程（客户端→服务器）",
        (3.0, 8.0),
        (8.0, 20.0),
        (8.0, 20.0),
    ),
    row("8 服务器 fan-out", (0.5, 1.0), (0.5, 2.0), (0.5, 2.0)),
    row("9 抖动缓冲（稳态）", (3.0, 5.0), (10.0, 15.0), (15.0, 20.0)),
    row("10 解码 + 混音", (0.3, 0.3), (0.3, 0.3), (0.3, 0.3)),
    row(
        "11 播放缓冲（设备/驱动）",
        (3.0, 5.0),
        (5.0, 10.0),
        (5.0, 10.0),
    ),
];

/// 三档预测：抖动余量（p95 − p50）与设计文档给的参考值。
struct Profile {
    name: &'static str,
    jitter: f64,
    doc_p50: f64,
    doc_p95: f64,
}

const PROFILES: [Profile; 3] = [
    Profile {
        name: "L0 无降噪",
        jitter: 10.0,
        doc_p50: 30.0,
        doc_p95: 40.0,
    },
    Profile {
        name: "L1 RNNoise",
        jitter: 20.0,
        doc_p50: 65.0,
        doc_p95: 85.0,
    },
    Profile {
        name: "L2 DFN3",
        jitter: 20.0,
        doc_p50: 90.0,
        doc_p95: 110.0,
    },
];

fn range_of(row: &Row, profile: usize) -> (f64, f64) {
    match profile {
        0 => row.l0,
        1 => row.l1,
        _ => row.l2,
    }
}

/// `testkit latency-model [--chain dfn3,agc2,opus] [--codec opus:10ms]`
pub fn main(args: &[String]) -> Result<(), String> {
    let mut chain_spec = "dfn3,agc2,opus".to_owned();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--chain" => chain_spec = super::next_value(args, &mut i, "--chain")?,
            "--codec" => {
                // 只做形状校验：模型只关心有没有 Opus 的 6.5 ms 前瞻。
                let codec = super::next_value(args, &mut i, "--codec")?;
                if codec != "none" && !codec.starts_with("opus") {
                    return Err(format!("未知 codec {codec:?}（当前只有 none / opus:*）"));
                }
            }
            other => return Err(format!("latency-model 不认识参数 {other:?}")),
        }
        i += 1;
    }

    let desc = ChainDesc::parse(&chain_spec).map_err(|e| e.to_string())?;

    // ---- 1. 复算附录 A 三档，与文档数字对照 ----
    println!("附录 A 复算（各环节区间取中值；文档值取整）：");
    println!(
        "{:<14} {:>8} {:>8} {:>10} {:>10} {:>8}",
        "档位", "预测 p50", "预测 p95", "文档 p50", "文档 p95", "Δp50"
    );
    for (index, profile) in PROFILES.iter().enumerate() {
        let p50: f64 = ROWS
            .iter()
            .map(|item| {
                let (low, high) = range_of(item, index);
                (low + high) / 2.0
            })
            .sum();
        println!(
            "{:<14} {:>8.2} {:>8.2} {:>10.1} {:>10.1} {:>+8.2}",
            profile.name,
            p50,
            p50 + profile.jitter,
            profile.doc_p50,
            profile.doc_p95,
            p50 - profile.doc_p50
        );
    }

    // ---- 2. 把当前链路代进表里 ----
    let has = |kind: StageKind| desc.stages().contains(&kind);
    let denoise = if has(StageKind::Dfn3) {
        30.0
    } else if has(StageKind::Rnn) {
        10.0
    } else {
        0.0
    };
    let agc = if has(StageKind::Agc2) { 1.0 } else { 0.0 };
    let opus = if has(StageKind::Opus) { 6.5 } else { 0.0 };
    let profile_index = if denoise == 30.0 {
        2
    } else if denoise == 10.0 {
        1
    } else {
        0
    };

    println!();
    println!(
        "链路 {desc}（算法延迟 {:.2} ms）逐行代进 {}：",
        desc.total_latency_ms(),
        PROFILES[profile_index].name
    );
    println!("  {:<30} {:>12} {:>10}", "环节", "附录 A 区间", "代入值");
    let mut p50 = 0.0_f64;
    for (row_index, item) in ROWS.iter().enumerate() {
        let (low, high) = range_of(item, profile_index);
        let value = match row_index {
            2 => denoise,
            3 => agc,
            4 => opus,
            _ => (low + high) / 2.0,
        };
        p50 += value;
        println!(
            "  {:<30} {:>5.1}–{:<6.1} {:>10.2}",
            item.label, low, high, value
        );
    }
    println!("  {:<30} {:>12} {:>10.2}", "合计", "", p50);
    println!();
    println!(
        "该链路预测：p50 {:.1} ms，p95 {:.1} ms（抖动余量 {:.0} ms）",
        p50,
        p50 + PROFILES[profile_index].jitter,
        PROFILES[profile_index].jitter
    );

    println!();
    println!("注：");
    println!(
        "  - 第 2 行「帧成组」与第 1 行「采集缓冲」在 480 样本块投递时部分重叠，实测以物理环回为准（附录 A 注 1）；"
    );
    println!("  - 预测 p50 = 各环节区间中值之和，p95 = p50 + 抖动余量（L0 10 ms，L1/L2 20 ms）；");
    println!("  - 这些是**模型预测**，不是实测。实测数字只进 docs/measurements.md。");
    Ok(())
}
