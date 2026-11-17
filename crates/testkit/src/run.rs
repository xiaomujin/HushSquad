//! 离线回放（P1-4）：wav → 链路 → 编解码 → wav + 报告。
//!
//! 这个子命令是整条「先测量后优化」纪律的落点：任何链路改动都要能在这里跑出一组数字，
//! 并且热路径分配数必须是 0。数字只在 release 构建下有意义，debug 构建会在报告里自曝。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use audio_core::{
    AudioCodec, Chain, ChainDesc, FRAME_SAMPLES, MAX_PACKET_BYTES, OpusCodec, OpusConfig,
    StageKind, samples_to_ms,
};

use crate::{alloc, report, wav};

/// 编解码选择。
#[derive(Debug, Clone, Copy)]
pub enum CodecSpec {
    /// 不编解码，只跑链路（基线）。
    None,
    /// libopus。
    Opus(OpusConfig),
}

impl CodecSpec {
    /// 报告里用的描述串。
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::None => "none".to_owned(),
            Self::Opus(config) => format!("opus:10ms:{}k", config.bitrate_bps / 1000),
        }
    }
}

/// 一次回放的结果。
#[derive(Debug)]
pub struct Outcome {
    /// 报告。
    pub report: report::Report,
    /// 输出采样（解码后的音频）。
    pub samples: Vec<f32>,
}

/// 解析 `--codec`：`none` 或 `opus[:10ms][:32k]`。
pub fn parse_codec(spec: &str) -> Result<CodecSpec, String> {
    if spec == "none" {
        return Ok(CodecSpec::None);
    }
    let parts: Vec<&str> = spec.split(':').collect();
    if parts.first() != Some(&"opus") {
        return Err(format!("未知 codec {spec:?}（当前只有 none / opus:*）"));
    }
    let mut config = OpusConfig::default();
    if let Some(frame) = parts.get(1)
        && *frame != "10ms"
    {
        return Err(format!("帧长只支持 10ms（技术设计 §6.1），收到 {frame:?}"));
    }
    if let Some(rate) = parts.get(2) {
        let kbps: i32 = rate
            .trim_end_matches('k')
            .parse()
            .map_err(|e| format!("比特率 {rate:?} 解析失败：{e}"))?;
        config.bitrate_bps = kbps * 1000;
    }
    Ok(CodecSpec::Opus(config))
}

/// 跑一段采样。纯内存操作，不碰文件，方便测试直接调用。
pub fn run_samples(
    samples: &[f32],
    desc: &ChainDesc,
    codec: CodecSpec,
    warmup: usize,
) -> Result<Outcome, String> {
    let frames = samples.len() / FRAME_SAMPLES;
    if frames == 0 {
        return Err("输入不足一帧（480 样本）".to_owned());
    }
    // 暖机帧不参与统计：第一批帧里编解码器还没进稳态，混进去会把 p50 拉花。
    let warmup = warmup.min(frames.saturating_sub(1));
    let measured = frames - warmup;

    let mut chain = Chain::from_desc(desc).map_err(|e| e.to_string())?;
    // 逐阶段计时：档位名与 `Chain::from_desc` 装进去的顺序一一对应（`opus` 不是 Stage）。
    let stage_names: Vec<String> = desc
        .stages()
        .iter()
        .filter(|kind| **kind != StageKind::Opus)
        .map(|kind| kind.name().to_owned())
        .collect();
    // 容量预先留够：热路径里 push 不能触发扩容，否则「零分配」断言会误报。
    let mut stage_ms: Vec<Vec<f64>> = stage_names
        .iter()
        .map(|_| Vec::with_capacity(measured))
        .collect();
    let mut frame_stage_ms: Vec<f64> = vec![0.0; stage_names.len()];
    let codec_label = codec.label();
    let mut codec = match codec {
        CodecSpec::None => None,
        CodecSpec::Opus(config) => Some(OpusCodec::new(config).map_err(|e| e.to_string())?),
    };

    // 全部缓冲预分配：热路径里不允许任何分配。
    let mut packet = [0_u8; MAX_PACKET_BYTES];
    let mut frame = [0.0_f32; FRAME_SAMPLES];
    let mut decoded = [0.0_f32; FRAME_SAMPLES];
    let mut chain_ms: Vec<f64> = Vec::with_capacity(measured);
    let mut enc_ms: Vec<f64> = Vec::with_capacity(measured);
    let mut dec_ms: Vec<f64> = Vec::with_capacity(measured);
    let mut out_samples: Vec<f32> = Vec::with_capacity(frames * FRAME_SAMPLES);
    let mut vad_sum = 0.0_f64;
    let mut packet_bytes = 0_usize;

    let allocations_before = alloc::count();

    for (index, chunk) in samples.as_chunks::<FRAME_SAMPLES>().0.iter().enumerate() {
        frame.copy_from_slice(chunk);

        let start = Instant::now();
        let vad = if frame_stage_ms.is_empty() {
            chain.process(&mut frame)
        } else {
            // 每帧清零后逐阶段累加，得到「这一帧各阶段各花了多久」。
            frame_stage_ms.fill(0.0);
            chain.process_profiled(&mut frame, &mut frame_stage_ms)
        };
        let after_chain = Instant::now();

        let encoded = match codec.as_mut() {
            Some(c) => c.encode(&frame, &mut packet),
            None => 0,
        };
        let after_encode = Instant::now();

        match codec.as_mut() {
            Some(c) => {
                if encoded == 0 || !c.decode(&packet[..encoded], false, &mut decoded) {
                    decoded.fill(0.0);
                }
            }
            None => decoded.copy_from_slice(&frame),
        }
        let after_decode = Instant::now();

        if index >= warmup {
            chain_ms.push(elapsed_ms(start, after_chain));
            for (slot, value) in stage_ms.iter_mut().zip(&frame_stage_ms) {
                slot.push(*value);
            }
            if codec.is_some() {
                enc_ms.push(elapsed_ms(after_chain, after_encode));
                dec_ms.push(elapsed_ms(after_encode, after_decode));
            }
            vad_sum += f64::from(vad);
            packet_bytes += encoded;
        }
        out_samples.extend_from_slice(&decoded);
    }

    let allocations_in_hot_path = alloc::count() - allocations_before;

    // 延迟构成：链路各档位自报 + 编解码前瞻。
    let mut per_stage: BTreeMap<String, f64> = BTreeMap::new();
    for kind in desc.stages() {
        per_stage.insert(
            kind.name().to_owned(),
            f64::from(samples_to_ms(kind.latency_samples())),
        );
    }
    if codec.is_some() && !desc.stages().contains(&StageKind::Opus) {
        per_stage.insert("opus".to_owned(), f64::from(samples_to_ms(312)));
    }
    let algorithm_ms = per_stage.values().sum();

    let mut cpu_ms_per_frame: BTreeMap<String, report::Stats> = BTreeMap::new();
    if codec.is_some() {
        cpu_ms_per_frame.insert("opus_enc".to_owned(), report::stats(enc_ms));
        cpu_ms_per_frame.insert("opus_dec".to_owned(), report::stats(dec_ms));
    }
    // 只有真的挂了处理阶段才统计链路耗时；`opus` 是编解码预算标记，不是 Stage。
    if desc.stages().iter().any(|kind| *kind != StageKind::Opus) {
        cpu_ms_per_frame.insert("chain".to_owned(), report::stats(chain_ms));
        // 逐阶段数字：每个阶段自己读两次时钟，含约 20–40 ns 的计时开销，
        // 对微秒级的环节（高通）占比可见，对大环节（AGC2）可忽略。
        for (name, values) in stage_names.iter().zip(stage_ms) {
            cpu_ms_per_frame.insert(name.clone(), report::stats(values));
        }
    }

    let measured_f = measured as f64;
    let packet_bytes_mean = packet_bytes as f64 / measured_f;

    let report = report::Report {
        chain: desc.stages().iter().map(|k| k.name().to_owned()).collect(),
        codec: codec_label,
        latency: report::Latency {
            algorithm_ms,
            per_stage_ms: per_stage,
        },
        cpu_ms_per_frame,
        vad_mean: (vad_sum / measured_f) as f32,
        allocations_in_hot_path,
        wav_out: None,
        frames: measured,
        warmup_frames: warmup,
        profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        packet_bytes_mean,
        // 10 ms 帧 ⇒ 100 pps
        bitrate_bps: packet_bytes_mean * 8.0 * 100.0,
    };

    Ok(Outcome {
        report,
        samples: out_samples,
    })
}

fn elapsed_ms(start: Instant, end: Instant) -> f64 {
    end.duration_since(start).as_secs_f64() * 1000.0
}

/// `testkit run --in <wav> [--out <wav>] [--chain none] [--codec opus:10ms:32k] [--report <json>] [--warmup 50]`
pub fn main(args: &[String]) -> Result<(), String> {
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut report_path: Option<PathBuf> = None;
    let mut chain_spec = "none".to_owned();
    let mut codec_spec = "opus:10ms:32k".to_owned();
    let mut warmup = 50_usize;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--in" => input = Some(PathBuf::from(crate::next_value(args, &mut i, "--in")?)),
            "--out" => output = Some(PathBuf::from(crate::next_value(args, &mut i, "--out")?)),
            "--report" => {
                report_path = Some(PathBuf::from(crate::next_value(args, &mut i, "--report")?));
            }
            "--chain" => chain_spec = crate::next_value(args, &mut i, "--chain")?,
            "--codec" => codec_spec = crate::next_value(args, &mut i, "--codec")?,
            "--warmup" => {
                warmup = crate::next_value(args, &mut i, "--warmup")?
                    .parse()
                    .map_err(|e| format!("--warmup 不是整数：{e}"))?;
            }
            other => return Err(format!("run 不认识参数 {other:?}")),
        }
        i += 1;
    }

    let input = input.ok_or("缺少 --in <wav>")?;
    let desc = ChainDesc::parse(&chain_spec).map_err(|e| e.to_string())?;
    let codec = parse_codec(&codec_spec)?;

    let samples = wav::read_mono_f32(&input)?;
    let mut outcome = run_samples(&samples, &desc, codec, warmup)?;

    if let Some(path) = &output {
        wav::write_mono_f32(path, &outcome.samples)?;
        outcome.report.wav_out = Some(path.display().to_string());
    }

    if outcome.report.allocations_in_hot_path != 0 {
        return Err(format!(
            "热路径发生了 {} 次分配（必须为 0）：检查 process/encode/decode 里是否有 Vec 扩容、format!、Box::new",
            outcome.report.allocations_in_hot_path
        ));
    }

    if outcome.report.profile == "debug" {
        eprintln!("警告：这是 debug 构建，耗时数字没有参考价值。用 --release 重跑。");
    }

    print_summary(&outcome.report);

    match &report_path {
        Some(path) => {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("建目录 {} 失败：{e}", parent.display()))?;
            }
            std::fs::write(path, outcome.report.to_json())
                .map_err(|e| format!("写报告 {} 失败：{e}", path.display()))?;
            println!("报告已写入 {}", path.display());
        }
        None => println!("{}", outcome.report.to_json()),
    }
    Ok(())
}

fn print_summary(report: &report::Report) {
    println!(
        "链路 {} / codec {} / 构建 {} / 统计 {} 帧（暖机 {}）",
        if report.chain.is_empty() {
            "none".to_owned()
        } else {
            report.chain.join(",")
        },
        report.codec,
        report.profile,
        report.frames,
        report.warmup_frames
    );
    println!(
        "算法延迟 {:.2} ms，VAD 均值 {:.3}",
        report.latency.algorithm_ms, report.vad_mean
    );
    for (name, stats) in &report.cpu_ms_per_frame {
        println!(
            "  {:<10} p50 {:>7.3}  p95 {:>7.3}  max {:>7.3} ms（p95 占每帧预算 10 ms 的 {:.2}%）",
            name,
            stats.p50,
            stats.p95,
            stats.max,
            stats.p95 / 10.0 * 100.0
        );
    }
    println!(
        "包长均值 {:.1} B，线上码率约 {:.0} bps",
        report.packet_bytes_mean, report.bitrate_bps
    );
    println!("热路径分配 {} 次", report.allocations_in_hot_path);
}

#[allow(dead_code)]
fn _duration_type_check(_d: Duration) {}
