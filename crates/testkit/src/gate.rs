//! 门控回放（ADR-0015）：一段 wav 喂进链路，数一数门开了多久。
//!
//! 这个子命令存在的理由，是「门限 0.5 / −50 dBFS / 30 ms 预缓冲」这三个默认值
//! **必须能被复现**。凭感觉定下来的门限，在别人的麦克风和别人的房间里就是另一个结果；
//! 所以判据做成可回放的：拿同一段素材，改一个数字，看开门比例怎么动。
//!
//! ```text
//! testkit gate --in testdata/synthetic/clean_48k_mono.wav
//! testkit gate --in testdata/synthetic/fan_48k_mono.wav --vad-floor 0.5
//! ```
//!
//! 输出的四个数各有各的用处：
//!
//! - **开门比例**：语音素材应当高、噪声素材应当低。低了说明切字，高了说明没挡住噪声；
//! - **开门次数**：一句话里被切碎成十几段就是滞回没调好（听感是「一顿一顿的」）；
//! - **保活数**：门关着时每 200 ms 一个静音标记，它会被对方算成丢包，所以这个数
//!   就是「假丢包的上限」；
//! - **省下的包**：门控的唯一收益，其余全是代价。

use std::path::PathBuf;

use audio_core::gate::{GateConfig, TxGate, VoiceMode, peak_db};
use audio_core::{Chain, ChainDesc, FRAME_SAMPLES};

use crate::wav;

/// 与 `apps/client/src/app.rs` 保持一致：门关着时每 20 帧（200 ms）一个保活标记。
const KEEPALIVE_FRAMES: u32 = 20;

/// `testkit gate --in <wav> [--chain <desc>] [--threshold -50] [--vad-floor 0.5] [--hold 30] [--preroll 3]`
pub fn main(args: &[String]) -> Result<(), String> {
    let mut input: Option<PathBuf> = None;
    let mut chain_spec = "highpass,rnn,agc2".to_owned();
    let mut threshold_db = -50.0_f32;
    let mut vad_floor = 0.5_f32;
    let mut hold_frames = 30_u32;
    let mut preroll = 3_u32;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--in" => input = Some(PathBuf::from(crate::next_value(args, &mut i, "--in")?)),
            "--chain" => chain_spec = crate::next_value(args, &mut i, "--chain")?,
            "--threshold" => {
                threshold_db = crate::next_value(args, &mut i, "--threshold")?
                    .parse()
                    .map_err(|e| format!("--threshold 不是数字：{e}"))?;
            }
            "--vad-floor" => {
                vad_floor = crate::next_value(args, &mut i, "--vad-floor")?
                    .parse()
                    .map_err(|e| format!("--vad-floor 不是数字：{e}"))?;
            }
            "--hold" => {
                hold_frames = crate::next_value(args, &mut i, "--hold")?
                    .parse()
                    .map_err(|e| format!("--hold 不是帧数：{e}"))?;
            }
            "--preroll" => {
                preroll = crate::next_value(args, &mut i, "--preroll")?
                    .parse()
                    .map_err(|e| format!("--preroll 不是帧数：{e}"))?;
            }
            other => return Err(format!("gate 不认识参数 {other:?}")),
        }
        i += 1;
    }

    let input = input.ok_or("缺少 --in <wav>")?;
    let desc = ChainDesc::parse(&chain_spec).map_err(|e| e.to_string())?;
    let samples = wav::read_mono_f32(&input)?;
    let stats = analyse(
        &samples,
        &desc,
        GateConfig {
            mode: VoiceMode::Vad,
            threshold_db,
            attack_frames: 2,
            hold_frames,
            vad_floor,
        },
        preroll,
    )?;

    let seconds = stats.frames as f64 * f64::from(audio_core::FRAME_MS) / 1000.0;
    println!(
        "{}（{:.1} s，{} 帧）／链路 {}",
        input.display(),
        seconds,
        stats.frames,
        if desc.stages().is_empty() {
            "none".to_owned()
        } else {
            desc.stages()
                .iter()
                .map(|kind| kind.name())
                .collect::<Vec<_>>()
                .join(",")
        }
    );
    println!(
        "判据：电平 ≥ {threshold_db:.0} dBFS 且 VAD ≥ {vad_floor:.2}（滞回 2 帧开 / {hold_frames} 帧关）"
    );
    println!(
        "  VAD 均值 {:.3}；VAD ≥ {vad_floor:.2} 的帧占 {:.1}%",
        stats.vad_mean,
        stats.vad_above * 100.0
    );
    println!(
        "  门开 {:.1}%（{} 帧），开门 {} 次，平均每次开 {:.0} ms",
        stats.open_ratio * 100.0,
        stats.open_frames,
        stats.openings,
        if stats.openings == 0 {
            0.0
        } else {
            seconds * 1000.0 * stats.open_ratio / stats.openings as f64
        }
    );
    println!(
        "  上行包：常开 {} → 门控 {}（省 {:.1}%）；其中保活 {} 个、补发 {} 个",
        stats.frames,
        stats.packets,
        (1.0 - stats.packets as f64 / stats.frames as f64) * 100.0,
        stats.keepalives,
        stats.openings * preroll as usize
    );
    Ok(())
}

/// 一次回放的结果。
#[derive(Debug, Clone, Copy, Default)]
pub struct GateStats {
    /// 总帧数。
    pub frames: usize,
    /// 门开着的帧数。
    pub open_frames: usize,
    /// 开门次数（关闭 → 打开的次数）。
    pub openings: usize,
    /// 门关着时发出去的保活标记数。
    pub keepalives: usize,
    /// 实际会发出去的包数（开门帧 + 补发 + 保活）。
    pub packets: usize,
    /// VAD 概率均值。
    pub vad_mean: f64,
    /// VAD ≥ 判据下限的帧占比。
    pub vad_above: f64,
    /// 开门比例（0–1）。
    pub open_ratio: f64,
}

/// 跑一段采样，返回门控统计。纯内存，测试可以直接调。
pub fn analyse(
    samples: &[f32],
    desc: &ChainDesc,
    config: GateConfig,
    preroll: u32,
) -> Result<GateStats, String> {
    let mut chain = Chain::from_desc(desc).map_err(|e| e.to_string())?;
    let mut gate = TxGate::new(config);
    let mut frame = [0.0_f32; FRAME_SAMPLES];
    let mut stats = GateStats::default();
    let mut vad_sum = 0.0_f64;
    let mut quiet = 0_u32;

    for chunk in samples.as_chunks::<FRAME_SAMPLES>().0 {
        frame.copy_from_slice(chunk);
        let vad = chain.process(&mut frame);
        vad_sum += f64::from(vad);
        if vad >= config.vad_floor {
            stats.vad_above += 1.0;
        }
        let step = gate.step(peak_db(&frame), vad);
        if step.send {
            stats.open_frames += 1;
            quiet = 0;
            if step.opened {
                stats.openings += 1;
            }
        } else {
            quiet += 1;
            if quiet >= KEEPALIVE_FRAMES {
                quiet = 0;
                stats.keepalives += 1;
            }
        }
        stats.frames += 1;
    }

    let frames = stats.frames.max(1) as f64;
    stats.vad_mean = vad_sum / frames;
    stats.vad_above /= frames;
    stats.open_ratio = stats.open_frames as f64 / frames;
    stats.packets = stats.open_frames + stats.openings * preroll as usize + stats.keepalives;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 前半段「说话」（1 kHz 正弦 + VAD 1.0），后半段全静音。
    fn speech_then_silence() -> Vec<f32> {
        let mut samples = Vec::with_capacity(FRAME_SAMPLES * 200);
        for index in 0..200 * FRAME_SAMPLES {
            if index < 100 * FRAME_SAMPLES {
                let phase = index as f32 * std::f32::consts::TAU * 1000.0 / 48_000.0;
                samples.push(0.25 * phase.sin());
            } else {
                samples.push(0.0);
            }
        }
        samples
    }

    #[test]
    fn the_gate_closes_on_the_silent_half() {
        // `none` 链路没有 VAD（恒为 1.0），所以这条测的是能量判据：
        // 前半段有声要发，后半段全零要关（关了之后只剩保活）。
        let desc = ChainDesc::parse("none").unwrap();
        let stats = analyse(&speech_then_silence(), &desc, GateConfig::default(), 3).unwrap();
        assert_eq!(stats.frames, 200);
        assert!(
            (stats.open_ratio - 0.5).abs() < 0.2,
            "开门比例应当在一半左右，实际 {}",
            stats.open_ratio
        );
        assert!(stats.openings >= 1, "应当至少开过一次门");
        // 后半段 100 帧静音，但门要等 hold（30 帧）才关，所以真正关着的只有 ~70 帧：
        // 70 / 20 = 3 个保活。这个数字正好说明 hold 会吃掉一部分「本来能省」的包。
        assert!(
            (3..=4).contains(&stats.keepalives),
            "保活数量不对：{}",
            stats.keepalives
        );
    }

    #[test]
    fn preroll_and_keepalives_are_counted_in_the_packet_total() {
        let desc = ChainDesc::parse("none").unwrap();
        let stats = analyse(&speech_then_silence(), &desc, GateConfig::default(), 3).unwrap();
        assert_eq!(
            stats.packets,
            stats.open_frames + stats.openings * 3 + stats.keepalives,
            "包数账要能对上"
        );
    }
}
