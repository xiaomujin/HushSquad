//! 桌面环回工具（P2-2）：采集 → `rtrb` → 播放，可选串 Opus；`--impulse` 测物理环回延迟。
//!
//! 为什么必须有 `--impulse`：这是唯一不靠假设就能拿到 D1 真值的手段（技术设计 §11.5 方法 3）。
//! 没有它，就只能相信「cpal 说 480 帧就是 10 ms」。
//!
//! 本工具是 P2 期间唯一允许打印的地方。

#![allow(clippy::print_stdout)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use audio_core::{AudioCodec, FRAME_SAMPLES, MAX_PACKET_BYTES, OpusCodec, OpusConfig};
use audio_io::{StreamEvent, StreamFormat};
use serde::Serialize;

const USAGE: &str = "\
桌面环回工具（P2）\n\
\n\
用法：\n\
  loopback --list-devices\n\
  loopback [--input <名字>] [--output <名字>] [--seconds 10] [--ring-ms 10]\n\
           [--codec none|opus:10ms:32k] [--report out/loopback.json]\n\
  loopback --impulse [--period-ms 500] [--seconds 10] [--report out/impulse.json]\n\
\n\
说明：\n\
  --ring-ms   播放/采集环形缓冲长度，直接进环回延迟，默认 10 ms\n\
  --impulse   播放已知脉冲串并用麦克风录回，互相关求环回延迟（外放时才有声学路径）\n";

/// 一次会话的统计。
#[derive(Debug, Default, Serialize)]
struct SessionReport {
    mode: String,
    input: Option<String>,
    output: Option<String>,
    capture_format: Option<FormatReport>,
    playout_format: Option<FormatReport>,
    ring_samples: usize,
    seconds: f64,
    capture_dropped_samples: u64,
    playout_underrun_samples: u64,
    stream_errors: u64,
    restarts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    delay_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delay_lag_samples: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    correlation: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uncertainty_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capture_peak: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    opus_frames: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct FormatReport {
    sample_rate: u32,
    channels: u16,
}

impl From<StreamFormat> for FormatReport {
    fn from(value: StreamFormat) -> Self {
        Self {
            sample_rate: value.sample_rate,
            channels: value.channels,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Codec {
    None,
    Opus,
}

struct Args {
    list_devices: bool,
    input: Option<String>,
    output: Option<String>,
    seconds: f64,
    ring_ms: f64,
    impulse: bool,
    period_ms: f64,
    codec: Codec,
    report: Option<PathBuf>,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut parsed = Args {
        list_devices: false,
        input: None,
        output: None,
        seconds: 10.0,
        ring_ms: 10.0,
        impulse: false,
        period_ms: 500.0,
        codec: Codec::None,
        report: None,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--list-devices" => parsed.list_devices = true,
            "--input" => parsed.input = Some(take(args, &mut i, "--input")?),
            "--output" => parsed.output = Some(take(args, &mut i, "--output")?),
            "--seconds" => {
                parsed.seconds = take(args, &mut i, "--seconds")?
                    .parse()
                    .map_err(|e| format!("--seconds 不是数字：{e}"))?;
            }
            "--ring-ms" => {
                parsed.ring_ms = take(args, &mut i, "--ring-ms")?
                    .parse()
                    .map_err(|e| format!("--ring-ms 不是数字：{e}"))?;
            }
            "--period-ms" => {
                parsed.period_ms = take(args, &mut i, "--period-ms")?
                    .parse()
                    .map_err(|e| format!("--period-ms 不是数字：{e}"))?;
            }
            "--impulse" => parsed.impulse = true,
            "--codec" => {
                let spec = take(args, &mut i, "--codec")?;
                parsed.codec = if spec == "none" {
                    Codec::None
                } else if spec.starts_with("opus") {
                    Codec::Opus
                } else {
                    return Err(format!("未知 codec {spec:?}（none / opus:10ms:32k）"));
                };
            }
            "--report" => parsed.report = Some(PathBuf::from(take(args, &mut i, "--report")?)),
            other => return Err(format!("不认识参数 {other:?}")),
        }
        i += 1;
    }
    Ok(parsed)
}

fn take(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} 缺少取值"))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // 只有「参数不对」才打印用法；设备/格式这类运行期错误只报错，别刷一屏帮助。
    let parsed = match parse_args(&args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("错误：{message}");
            eprintln!();
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    match execute(&parsed) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("错误：{message}");
            ExitCode::FAILURE
        }
    }
}

fn execute(parsed: &Args) -> Result<(), String> {
    if parsed.list_devices {
        let devices = audio_io::list_devices().map_err(|e| e.to_string())?;
        println!("输入设备：");
        for device in &devices.inputs {
            println!(
                "  {}{}",
                device.name,
                if device.is_default { "  (默认)" } else { "" }
            );
        }
        println!("输出设备：");
        for device in &devices.outputs {
            println!(
                "  {}{}",
                device.name,
                if device.is_default { "  (默认)" } else { "" }
            );
        }
        return Ok(());
    }

    let report = if parsed.impulse {
        run_impulse(parsed)?
    } else {
        run_passthrough(parsed)?
    };

    let json = serde_json::to_string_pretty(&report).map_err(|e| format!("序列化报告失败：{e}"))?;
    match &parsed.report {
        Some(path) => {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("建目录 {} 失败：{e}", parent.display()))?;
            }
            std::fs::write(path, &json).map_err(|e| format!("写报告失败：{e}"))?;
            println!("报告已写入 {}", path.display());
        }
        None => println!("{json}"),
    }
    Ok(())
}

/// 采集直通播放（可选串 Opus）。
fn run_passthrough(args: &Args) -> Result<SessionReport, String> {
    let mut attempt = 0_u32;
    loop {
        let report = session_passthrough(args)?;
        if report.stream_errors == 0 || attempt >= 2 {
            return Ok(SessionReport {
                restarts: attempt,
                ..report
            });
        }
        // P2-4：设备报错后重建整条流（rtrb 端点在回调里，重建流只能连缓冲一起重建）。
        attempt += 1;
        eprintln!("流报错，重建第 {attempt} 次…");
    }
}

fn session_passthrough(args: &Args) -> Result<SessionReport, String> {
    let ring_samples = ring_capacity(args);
    let (tx, rx) = mpsc::channel();

    let (capture, playout, stop, codec_frames) = match args.codec {
        Codec::None => {
            let (producer, consumer) = rtrb::RingBuffer::<f32>::new(ring_samples);
            let capture = audio_io::start_capture(args.input.as_deref(), producer, tx.clone())
                .map_err(|e| e.to_string())?;
            let playout = audio_io::start_playout(args.output.as_deref(), consumer, tx)
                .map_err(|e| e.to_string())?;
            (capture, playout, None, None)
        }
        Codec::Opus => {
            let (producer_a, consumer_a) = rtrb::RingBuffer::<f32>::new(ring_samples);
            let (producer_b, consumer_b) = rtrb::RingBuffer::<f32>::new(ring_samples);
            let capture = audio_io::start_capture(args.input.as_deref(), producer_a, tx.clone())
                .map_err(|e| e.to_string())?;
            let playout = audio_io::start_playout(args.output.as_deref(), consumer_b, tx)
                .map_err(|e| e.to_string())?;
            if !capture.is_48k() || !playout.is_48k() {
                return Err(format!(
                    "Opus 路径要求 48 kHz（采集 {} Hz / 播放 {} Hz）；先换 48 kHz 设备或去掉 --codec",
                    capture.format.sample_rate, playout.format.sample_rate
                ));
            }
            let stop = Arc::new(AtomicBool::new(false));
            let frames = Arc::new(AtomicU64::new(0));
            spawn_opus_worker(
                consumer_a,
                producer_b,
                Arc::clone(&stop),
                Arc::clone(&frames),
            );
            (capture, playout, Some(stop), Some(frames))
        }
    };

    wait(args.seconds, &rx);

    if let Some(stop) = &stop {
        stop.store(true, Ordering::Relaxed);
    }
    let report = SessionReport {
        mode: match args.codec {
            Codec::None => "passthrough".to_owned(),
            Codec::Opus => "opus".to_owned(),
        },
        input: args.input.clone(),
        output: args.output.clone(),
        capture_format: Some(capture.format.into()),
        playout_format: Some(playout.format.into()),
        ring_samples,
        seconds: args.seconds,
        capture_dropped_samples: capture.stats.dropped_samples.load(Ordering::Relaxed),
        playout_underrun_samples: playout.stats.underrun_samples.load(Ordering::Relaxed),
        stream_errors: capture.stats.callback_errors.load(Ordering::Relaxed)
            + playout.stats.callback_errors.load(Ordering::Relaxed),
        restarts: 0,
        delay_ms: None,
        delay_lag_samples: None,
        correlation: None,
        uncertainty_ms: None,
        capture_peak: None,
        opus_frames: codec_frames.map(|frames| frames.load(Ordering::Relaxed)),
    };

    // 显式丢弃句柄，确保流在统计之后才停。
    drop(capture);
    drop(playout);
    Ok(report)
}

fn ring_capacity(args: &Args) -> usize {
    let samples = (args.ring_ms / 1000.0 * f64::from(audio_io::PREFERRED_SAMPLE_RATE)) as usize;
    samples.max(FRAME_SAMPLES)
}

fn wait(seconds: f64, events: &mpsc::Receiver<StreamEvent>) {
    let deadline = Instant::now() + Duration::from_secs_f64(seconds.max(0.0));
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(100)) {
            Ok(StreamEvent::Error(message)) => eprintln!("流事件：{message}"),
            Ok(StreamEvent::Disconnected) => eprintln!("设备断开"),
            Err(_) => {}
        }
    }
}

fn spawn_opus_worker(
    mut from_capture: rtrb::Consumer<f32>,
    mut to_playout: rtrb::Producer<f32>,
    stop: Arc<AtomicBool>,
    frames: Arc<AtomicU64>,
) {
    std::thread::spawn(move || {
        let mut codec = match OpusCodec::new(OpusConfig::default()) {
            Ok(codec) => codec,
            Err(err) => {
                eprintln!("Opus 初始化失败：{err}");
                return;
            }
        };
        let mut frame = [0.0_f32; FRAME_SAMPLES];
        let mut decoded = [0.0_f32; FRAME_SAMPLES];
        let mut packet = [0_u8; MAX_PACKET_BYTES];

        while !stop.load(Ordering::Relaxed) {
            // 攒满一帧；不够就先睡 1 ms（这里不是实时线程，允许睡眠）。
            let mut filled = 0;
            while filled < FRAME_SAMPLES {
                match from_capture.pop() {
                    Ok(sample) => {
                        frame[filled] = sample;
                        filled += 1;
                    }
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(1));
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                    }
                }
            }

            let len = codec.encode(&frame, &mut packet);
            if len == 0 || !codec.decode(&packet[..len], false, &mut decoded) {
                continue;
            }
            frames.fetch_add(1, Ordering::Relaxed);

            let mut pushed = 0;
            while pushed < FRAME_SAMPLES {
                if to_playout.push(decoded[pushed]).is_ok() {
                    pushed += 1;
                } else {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    });
}

/// `--impulse`：推已知脉冲串，同时录音，互相关求环回延迟。
fn run_impulse(args: &Args) -> Result<SessionReport, String> {
    let ring_samples = ring_capacity(args);
    let (tx, rx) = mpsc::channel();
    // 录音侧单独放大到 20 倍（默认 200 ms）：抽干是「每轮循环一次」，Windows 上两轮之间可能有 15 ms，
    // 10 ms 的环必然溢出——那不是设备问题，是我们自己把缓冲开小了。
    let capture_ring = ring_samples.saturating_mul(20);
    let (capture_producer, capture_consumer) = rtrb::RingBuffer::<f32>::new(capture_ring);
    let (playout_producer, playout_consumer) = rtrb::RingBuffer::<f32>::new(ring_samples);

    let capture = audio_io::start_capture(args.input.as_deref(), capture_producer, tx.clone())
        .map_err(|e| e.to_string())?;
    let playout = audio_io::start_playout(args.output.as_deref(), playout_consumer, tx)
        .map_err(|e| e.to_string())?;

    let sample_rate = capture.format.sample_rate;
    if playout.format.sample_rate != sample_rate {
        return Err(format!(
            "采集与播放采样率不一致（{} / {} Hz），先换同一设备族再测",
            sample_rate, playout.format.sample_rate
        ));
    }

    let period = ((args.period_ms / 1000.0 * f64::from(sample_rate)) as usize).max(1);
    let total = (args.seconds.max(0.1) * f64::from(sample_rate)) as usize;
    let train = audio_io::impulse::click_train(total, period, sample_rate, 0.7);

    let stop = Arc::new(AtomicBool::new(false));
    let (emitted_len, recorded) = pump_impulse(
        capture_consumer,
        playout_producer,
        &train,
        sample_rate,
        Arc::clone(&stop),
        args.seconds,
    );
    stop.store(true, Ordering::Relaxed);

    // 把流事件打印出来（设备被拔、格式失效都会走这里），别让它们静默消失。
    while let Ok(event) = rx.try_recv() {
        eprintln!("流事件：{event:?}");
    }

    let estimate = audio_io::impulse::estimate_delay(
        &recorded,
        &train[..emitted_len.min(train.len())],
        period,
        sample_rate,
    );

    // 不确定度：播放环形缓冲占用 + 一个回调缓冲（采集侧和播放侧各算一次，取保守值）。
    let uncertainty_samples = ring_samples + FRAME_SAMPLES;
    let report = SessionReport {
        mode: "impulse".to_owned(),
        input: args.input.clone(),
        output: args.output.clone(),
        capture_format: Some(capture.format.into()),
        playout_format: Some(playout.format.into()),
        ring_samples,
        seconds: args.seconds,
        capture_dropped_samples: capture.stats.dropped_samples.load(Ordering::Relaxed),
        playout_underrun_samples: playout.stats.underrun_samples.load(Ordering::Relaxed),
        stream_errors: capture.stats.callback_errors.load(Ordering::Relaxed)
            + playout.stats.callback_errors.load(Ordering::Relaxed),
        restarts: 0,
        delay_ms: estimate.map(|e| e.delay_ms),
        delay_lag_samples: estimate.map(|e| e.lag_samples),
        correlation: estimate.map(|e| e.correlation),
        uncertainty_ms: Some(uncertainty_samples as f64 * 1000.0 / f64::from(sample_rate)),
        capture_peak: Some(recorded.iter().fold(0.0_f32, |acc, s| acc.max(s.abs()))),
        opus_frames: None,
    };

    drop(capture);
    drop(playout);

    match estimate {
        Some(estimate) => println!(
            "环回延迟 {:.2} ms（lag {} 样本，相关度 {:.3}，不确定度 ±{:.2} ms）",
            estimate.delay_ms,
            estimate.lag_samples,
            estimate.correlation,
            report.uncertainty_ms.unwrap_or(0.0)
        ),
        None => eprintln!(
            "没找到相关峰：录音峰值 {:.4}（−60 dBFS 以上才算有声）。检查麦克风是否静音、输出是否真的接到了这台设备的输入",
            report.capture_peak.unwrap_or(0.0)
        ),
    }
    Ok(report)
}

/// 推送脉冲串并录音，返回（已推送样本数, 录音）。
/// 按**墙钟时间**推进发射序列，同时把录音抽干。
///
/// 为什么不是「每次 sleep 1 ms 就推 1 ms 的量」：Windows 的 `sleep(1ms)` 实际睡 ~15 ms，
/// 那样推得比设备慢，播放侧会大面积欠载（实测 3 秒欠载 16 704 样本），测出来的延迟也没意义。
/// 改成「按已流逝时间算出应该推到哪」之后，粗粒度睡眠自动被补偿，环缓冲占用也保持很低。
fn pump_impulse(
    mut from_capture: rtrb::Consumer<f32>,
    mut to_playout: rtrb::Producer<f32>,
    train: &[f32],
    sample_rate: u32,
    stop: Arc<AtomicBool>,
    seconds: f64,
) -> (usize, Vec<f32>) {
    let mut recorded: Vec<f32> = Vec::with_capacity((seconds * f64::from(sample_rate)) as usize);
    let mut emitted = 0_usize;
    let rate = f64::from(sample_rate);
    let start = Instant::now();

    loop {
        let elapsed = start.elapsed().as_secs_f64();
        if elapsed >= seconds.max(0.1) || stop.load(Ordering::Relaxed) {
            break;
        }
        let target = ((elapsed * rate) as usize).min(train.len());
        while emitted < target {
            if to_playout.push(train[emitted]).is_err() {
                break; // 环满：设备还没取走，下一轮再补
            }
            emitted += 1;
        }
        while let Ok(sample) = from_capture.pop() {
            recorded.push(sample);
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    // 收尾：把设备侧最后一点录音也排空
    while let Ok(sample) = from_capture.pop() {
        recorded.push(sample);
    }
    (emitted, recorded)
}
