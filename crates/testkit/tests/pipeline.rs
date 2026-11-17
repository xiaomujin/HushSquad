//! P1-4 的门禁：热路径零分配、报告字段自洽、未实现档位明确报错。

use std::f32::consts::PI;

use audio_core::{ChainDesc, FRAME_SAMPLES, OpusConfig, SAMPLE_RATE};
use testkit::run::{CodecSpec, parse_codec, run_samples};

fn material(frames: usize) -> Vec<f32> {
    (0..frames * FRAME_SAMPLES)
        .map(|n| {
            let t = n as f32 / SAMPLE_RATE as f32;
            0.3 * (2.0 * PI * 440.0 * t).sin()
        })
        .collect()
}

#[test]
fn hot_path_allocates_nothing() {
    let samples = material(200);
    let desc = ChainDesc::parse("opus").unwrap();
    let outcome =
        run_samples(&samples, &desc, CodecSpec::Opus(OpusConfig::default()), 20).expect("回放失败");

    assert_eq!(
        outcome.report.allocations_in_hot_path, 0,
        "热路径分配 {} 次：检查 process/encode/decode 里有没有 Vec 扩容、format!、Box::new",
        outcome.report.allocations_in_hot_path
    );
    assert_eq!(outcome.report.frames, 180);
    assert_eq!(outcome.report.warmup_frames, 20);
    assert_eq!(outcome.report.codec, "opus:10ms:32k");
    assert_eq!(outcome.report.latency.algorithm_ms, 6.5);
    assert!(outcome.report.cpu_ms_per_frame.contains_key("opus_enc"));
    assert!(outcome.report.cpu_ms_per_frame.contains_key("opus_dec"));
    // `opus` 是编解码预算标记，不是 Stage，不该出现 chain 耗时行
    assert!(!outcome.report.cpu_ms_per_frame.contains_key("chain"));
    assert_eq!(outcome.samples.len(), samples.len());
}

#[test]
fn passthrough_without_codec_is_identity() {
    let samples = material(10);
    let desc = ChainDesc::parse("none").unwrap();
    let outcome = run_samples(&samples, &desc, CodecSpec::None, 0).expect("回放失败");

    assert_eq!(outcome.samples, samples, "无编解码时必须逐样本原样输出");
    assert_eq!(outcome.report.codec, "none");
    assert_eq!(outcome.report.latency.algorithm_ms, 0.0);
    assert_eq!(outcome.report.allocations_in_hot_path, 0);
    assert!(outcome.report.cpu_ms_per_frame.is_empty());
}

#[test]
fn unimplemented_stage_is_rejected_with_hint() {
    let samples = material(10);
    let desc = ChainDesc::parse("dfn3").unwrap();
    let err = run_samples(&samples, &desc, CodecSpec::None, 0).unwrap_err();
    assert!(err.contains("dfn3"), "报错应指明哪个档位没实现：{err}");
}

#[test]
fn implemented_stages_report_per_stage_cost() {
    // P3-1：高通与 AGC2 已实现，报告里必须逐阶段给耗时，否则「先测量后优化」无从下手。
    let samples = material(20);
    let desc = ChainDesc::parse("highpass,rnn,agc2").unwrap();
    let outcome = run_samples(&samples, &desc, CodecSpec::None, 0).expect("回放失败");
    let cpu = &outcome.report.cpu_ms_per_frame;
    for key in ["chain", "highpass", "rnn", "agc2"] {
        assert!(
            cpu.contains_key(key),
            "缺少 {key} 的耗时行：{:?}",
            cpu.keys().collect::<Vec<_>>()
        );
    }
    assert_eq!(
        outcome.report.allocations_in_hot_path, 0,
        "处理链热路径出现了分配"
    );
    // 链路总 p95 不该小于任一单阶段的 p95（允许 1 µs 的计时抖动）。
    let chain_p95 = cpu["chain"].p95;
    for key in ["highpass", "rnn", "agc2"] {
        assert!(
            chain_p95 + 1e-3 >= cpu[key].p95,
            "{key} 的 p95 {} 超过链路总 p95 {chain_p95}",
            cpu[key].p95
        );
    }
}

#[test]
fn codec_spec_is_parsed_strictly() {
    assert!(matches!(parse_codec("none"), Ok(CodecSpec::None)));
    assert!(matches!(parse_codec("opus"), Ok(CodecSpec::Opus(_))));
    match parse_codec("opus:10ms:64k") {
        Ok(CodecSpec::Opus(config)) => assert_eq!(config.bitrate_bps, 64_000),
        other => panic!("解析 opus:10ms:64k 失败：{other:?}"),
    }
    // 5 ms 帧会破坏 10 ms 链路粒度，必须拒绝（技术设计 §6.1）
    assert!(parse_codec("opus:5ms:32k").is_err());
    assert!(parse_codec("rnn").is_err());
}
