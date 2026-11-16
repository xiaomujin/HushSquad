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
  --ring-ms   播放/采集环形缓冲长度，直接进环回延迟，默认 50 ms\n\
  --buffer    设备缓冲（帧，如 128/256/480）；不给就用设备默认。调小可降延迟，代价是更容易 xrun\n\
  --amplitude 脉冲振幅（0–0.95），默认 0.7；怕吵就先给 0.2 探一下\n\
  --single    只发一个脉冲（在 0.5 s 处）：没有周期歧义，结果唯一，推荐先用它\n\
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
    xrun_events: u64,
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
    /// 实际推给播放设备的样本数（impulse 模式）。为 0 就说明根本没喂进去。
    #[serde(skip_serializing_if = "Option::is_none")]
    emitted_samples: Option<usize>,
    /// 用两路回调时间戳算出的延迟（实验值：WASAPI 上两路 StreamInstant 原点不共享）。
    #[serde(skip_serializing_if = "Option::is_none")]
    delay_ms_stream_clock: Option<f64>,
    /// 上述实验值的逐脉冲极差。
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_clock_spread_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    opus_frames: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
struct FormatReport {
    sample_rate: u32,
    channels: u16,
    /// 请求的设备缓冲（帧）；`null` 表示用设备默认。
    requested_buffer_frames: Option<u32>,
    /// 设备实际采样格式。
    sample_format: String,
}

impl From<StreamFormat> for FormatReport {
    fn from(value: StreamFormat) -> Self {
        Self {
            sample_rate: value.sample_rate,
            channels: value.channels,
            requested_buffer_frames: value.requested_buffer_frames,
            sample_format: format!("{:?}", value.sample_format),
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
    buffer: Option<u32>,
    impulse: bool,
    single: bool,
    period_ms: f64,
    amplitude: f32,
    codec: Codec,
    report: Option<PathBuf>,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut parsed = Args {
        list_devices: false,
        input: None,
        output: None,
        seconds: 10.0,
        ring_ms: 50.0,
        buffer: None,
        impulse: false,
        single: false,
        period_ms: 500.0,
        amplitude: 0.7,
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
            "--buffer" => {
                let frames: u32 = take(args, &mut i, "--buffer")?
                    .parse()
                    .map_err(|e| format!("--buffer 不是整数：{e}"))?;
                parsed.buffer = Some(frames);
            }
            "--amplitude" => {
                parsed.amplitude = take(args, &mut i, "--amplitude")?
                    .parse()
                    .map_err(|e| format!("--amplitude 不是数字：{e}"))?;
            }
            "--impulse" => parsed.impulse = true,
            "--single" => parsed.single = true,
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
        // 只有「设备真的没了」才重建；xrun 只是抖了一下，重建反而丢更多音频。
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
            let capture = audio_io::start_capture(
                args.input.as_deref(),
                producer,
                tx.clone(),
                args.buffer,
                None,
            )
            .map_err(|e| e.to_string())?;
            let playout =
                audio_io::start_playout(args.output.as_deref(), consumer, tx, args.buffer, None)
                    .map_err(|e| e.to_string())?;
            (capture, playout, None, None)
        }
        Codec::Opus => {
            let (producer_a, consumer_a) = rtrb::RingBuffer::<f32>::new(ring_samples);
            let (producer_b, consumer_b) = rtrb::RingBuffer::<f32>::new(ring_samples);
            let capture = audio_io::start_capture(
                args.input.as_deref(),
                producer_a,
                tx.clone(),
                args.buffer,
                None,
            )
            .map_err(|e| e.to_string())?;
            let playout =
                audio_io::start_playout(args.output.as_deref(), consumer_b, tx, args.buffer, None)
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
        stream_errors: capture.stats.fatal_errors.load(Ordering::Relaxed)
            + playout.stats.fatal_errors.load(Ordering::Relaxed),
        xrun_events: capture.stats.xrun_events.load(Ordering::Relaxed)
            + playout.stats.xrun_events.load(Ordering::Relaxed),
        restarts: 0,
        delay_ms: None,
        delay_lag_samples: None,
        correlation: None,
        uncertainty_ms: None,
        capture_peak: Some(capture.stats.peak()),
        emitted_samples: None,
        delay_ms_stream_clock: None,
        stream_clock_spread_ms: None,
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
            Ok(StreamEvent::Fatal { kind, message }) => eprintln!("[致命] {kind}: {message}"),
            Ok(StreamEvent::Warning { kind, message }) => eprintln!("[xrun] {kind}: {message}"),
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
    // 时间戳通道：输入/输出各一条，回调里无锁写入，跑完再抽出来算延迟。
    let (stamp_out_tx, mut stamp_out_rx) = rtrb::RingBuffer::<audio_io::CallbackStamp>::new(8_192);
    let (stamp_in_tx, mut stamp_in_rx) = rtrb::RingBuffer::<audio_io::CallbackStamp>::new(8_192);

    let capture = audio_io::start_capture(
        args.input.as_deref(),
        capture_producer,
        tx.clone(),
        args.buffer,
        Some(stamp_in_tx),
    )
    .map_err(|e| e.to_string())?;
    let playout = audio_io::start_playout(
        args.output.as_deref(),
        playout_consumer,
        tx,
        args.buffer,
        Some(stamp_out_tx),
    )
    .map_err(|e| e.to_string())?;

    let sample_rate = capture.format.sample_rate;
    if playout.format.sample_rate != sample_rate {
        return Err(format!(
            "采集与播放采样率不一致（{} / {} Hz），先换同一设备族再测",
            sample_rate, playout.format.sample_rate
        ));
    }

    let total = (args.seconds.max(0.1) * f64::from(sample_rate)) as usize;
    let amplitude = args.amplitude.clamp(0.0, 0.95);
    // 单次脉冲：周期取整段长度 ⇒ 只有一个脉冲；再旋到 0.5 s 处，前后都留余量。
    // 周期脉冲串做互相关有「延迟 mod 周期」的歧义，实测会在 4 / 23 / 443 ms 之间乱跳。
    let (period, train) = if args.single {
        let mut single = audio_io::impulse::click_train(total, total, sample_rate, amplitude);
        let shift = ((0.5 * f64::from(sample_rate)) as usize).min(total.saturating_sub(1));
        single.rotate_right(shift);
        (total, single)
    } else {
        let period = ((args.period_ms / 1000.0 * f64::from(sample_rate)) as usize).max(1);
        (
            period,
            audio_io::impulse::click_train(total, period, sample_rate, amplitude),
        )
    };

    let stop = Arc::new(AtomicBool::new(false));
    let (emitted_len, recorded, min_occupancy, max_occupancy) = pump_impulse(
        capture_consumer,
        playout_producer,
        &train,
        sample_rate,
        Arc::clone(&stop),
        args.seconds,
        ring_samples,
    );
    stop.store(true, Ordering::Relaxed);

    // 抽出两边的时间戳（回调里只写，不读，避免任何阻塞）。
    let mut out_stamps: Vec<audio_io::CallbackStamp> = Vec::new();
    while let Ok(stamp) = stamp_out_rx.pop() {
        out_stamps.push(stamp);
    }
    let mut in_stamps: Vec<audio_io::CallbackStamp> = Vec::new();
    while let Ok(stamp) = stamp_in_rx.pop() {
        in_stamps.push(stamp);
    }

    // 把流事件打印出来（设备被拔、格式失效都会走这里），别让它们静默消失。
    while let Ok(event) = rx.try_recv() {
        match event {
            StreamEvent::Fatal { kind, message } => eprintln!("[致命] {kind}: {message}"),
            StreamEvent::Warning { kind, message } => eprintln!("[xrun] {kind}: {message}"),
        }
    }

    // 主算法：用两路回调时间戳（WASAPI 上都是 QPC）直接算「播放时刻 → 采集时刻」。
    // 这样环形缓冲占用、流启动错位、回调抖动都不再进入结果。
    let stamp_delay = compute_stamp_delay(
        &train[..emitted_len.min(train.len())],
        &recorded,
        &out_stamps,
        &in_stamps,
        sample_rate,
        period,
    );

    let estimate = audio_io::impulse::estimate_delay(
        &recorded,
        &train[..emitted_len.min(train.len())],
        period,
        sample_rate,
    );

    // 不确定度 = 播放环占用的**波动范围** + 一个回调缓冲。
    // 恒定占用只是把延迟整体平移（已知量），波动才会让单个 lag 对不齐。
    let jitter = max_occupancy.saturating_sub(min_occupancy.min(max_occupancy));
    let uncertainty_samples = jitter + FRAME_SAMPLES;
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
        stream_errors: capture.stats.fatal_errors.load(Ordering::Relaxed)
            + playout.stats.fatal_errors.load(Ordering::Relaxed),
        xrun_events: capture.stats.xrun_events.load(Ordering::Relaxed)
            + playout.stats.xrun_events.load(Ordering::Relaxed),
        restarts: 0,
        // 主值用相关法：它自带交叉验证（全局峰与最早强峰应当一致），不确定度可算。
        delay_ms: estimate.map(|e| e.delay_ms),
        delay_lag_samples: estimate.map(|e| e.lag_samples),
        correlation: estimate.map(|e| e.correlation),
        uncertainty_ms: Some(uncertainty_samples as f64 * 1000.0 / f64::from(sample_rate)),
        // 时间戳法：cpal 的 StreamInstant 在 WASAPI 上两路原点不共享（实测差上百毫秒），
        // 现在只能当实验值放这里，等共享时钟方案落地后再当主值。
        delay_ms_stream_clock: stamp_delay.map(|(median, _)| median),
        stream_clock_spread_ms: stamp_delay.map(|(_, spread)| spread),
        capture_peak: Some(capture.stats.peak()),
        emitted_samples: Some(emitted_len),
        opus_frames: None,
    };

    let underruns = playout.stats.underrun_samples.load(Ordering::Relaxed);
    drop(capture);
    drop(playout);

    println!(
        "已推送 {:.2} s 脉冲到播放设备（欠载 {} 样本），录音 {:.2} s",
        emitted_len as f64 / f64::from(sample_rate),
        underruns,
        recorded.len() as f64 / f64::from(sample_rate)
    );

    match estimate {
        Some(estimate) => {
            println!(
                "环回延迟 {:.2} ms（最早强峰 lag {} 样本，相关度 {:.3}，不确定度 ±{:.2} ms）",
                estimate.delay_ms,
                estimate.lag_samples,
                estimate.correlation,
                report.uncertainty_ms.unwrap_or(0.0)
            );
            if estimate.peak_lag_samples != estimate.lag_samples {
                println!(
                    "  参考：全局最高峰在 {:.2} ms（相关度 {:.3}），按多径反射忽略",
                    estimate.peak_lag_samples as f64 * 1000.0 / f64::from(sample_rate),
                    estimate.peak_correlation
                );
            }
        }
        None => {
            eprintln!(
                "没找到相关峰：录音峰值 {:.4}（−60 dBFS 以上才算有声）。检查麦克风是否静音、输出是否真的接到了这台设备的输入",
                report.capture_peak.unwrap_or(0.0)
            );
            let curve = audio_io::impulse::correlation_curve(
                &recorded,
                &train[..emitted_len],
                period,
                sample_rate,
            );
            let mut peaks: Vec<(usize, f32)> = curve.iter().copied().enumerate().collect();
            peaks.sort_by(|a, b| b.1.total_cmp(&a.1));
            eprintln!("相关度最高的 5 个位移（用于判断是没收到还是算法没对上）：");
            for (lag, score) in peaks.iter().take(5) {
                eprintln!(
                    "  {:.2} ms  score {:.3}",
                    *lag as f64 * 1000.0 / f64::from(sample_rate),
                    score
                );
            }
        }
    }
    Ok(report)
}

/// 把「推入环形缓冲的样本序号」换算成「回调墙钟」时间（纳秒）。
///
/// 关键：推入序号 ≠ 流序号。流启动时会先播若干静音、欠载时又会插静音，
/// 所以必须用 `real_frames` 累积出「推入序号 ↔ 流序号」的映射，否则会算出一个
/// 随欠载量漂移的假延迟（实测偏了 150–200 ms）。
fn play_time_of(
    stamps: &[audio_io::CallbackStamp],
    train_index: u64,
    sample_rate: u32,
) -> Option<u64> {
    let mut consumed = 0_u64;
    for stamp in stamps {
        let next = consumed + stamp.real_frames;
        if train_index >= consumed && train_index < next {
            let stream_index = stamp.first_index + (train_index - consumed);
            return Some(
                stamp.wall_nanos
                    + (stream_index - stamp.first_index) * 1_000_000_000 / u64::from(sample_rate),
            );
        }
        consumed = next;
    }
    None
}

/// 把流内样本序号换算成「回调墙钟」时间（纳秒）。
///
/// 不用 cpal 的 `stream_nanos`：实测输入/输出两路原点差出上百毫秒，直接相减会得到一个假延迟。
/// 回调墙钟来自同一个 `Instant` 纪元，两路天然可比。
fn wall_time_of(stamps: &[audio_io::CallbackStamp], index: u64, sample_rate: u32) -> Option<u64> {
    stamps.iter().find_map(|stamp| {
        if index < stamp.first_index || index >= stamp.first_index + stamp.frames {
            return None;
        }
        Some(
            stamp.wall_nanos + (index - stamp.first_index) * 1_000_000_000 / u64::from(sample_rate),
        )
    })
}

/// 用两路回调时间戳算环回延迟：`采集时刻 − 播放时刻`，逐脉冲取中位数。
///
/// 这是唯一不含「我们自己的缓冲」的算法：播放侧用的是设备承诺的播放时刻
/// （`OutputCallbackInfo::timestamp().playback`），采集侧用的是设备承诺的采集时刻
/// （`InputCallbackInfo::timestamp().capture`），两者在 WASAPI 上同源于 QPC。
///
/// 返回（中位数 ms, 逐脉冲极差 ms）；极差大说明脉冲配对错了或设备在丢样本。
fn compute_stamp_delay(
    emitted: &[f32],
    recorded: &[f32],
    out_stamps: &[audio_io::CallbackStamp],
    in_stamps: &[audio_io::CallbackStamp],
    sample_rate: u32,
    period: usize,
) -> Option<(f64, f64)> {
    let emitted_peak = emitted.iter().fold(0.0_f32, |acc, s| acc.max(s.abs()));
    let recorded_peak = recorded.iter().fold(0.0_f32, |acc, s| acc.max(s.abs()));
    if emitted_peak < 1e-3 || recorded_peak < 1e-3 {
        return None;
    }
    let emitted_clicks =
        audio_io::impulse::click_positions(emitted, emitted_peak * 0.5, period / 2);
    let recorded_clicks =
        audio_io::impulse::click_positions(recorded, recorded_peak * 0.3, period / 2);
    let pairs = emitted_clicks.len().min(recorded_clicks.len());
    if pairs == 0 {
        return None;
    }

    let mut delays: Vec<f64> = Vec::with_capacity(pairs);
    for index in 0..pairs {
        let Some(t_play) = play_time_of(out_stamps, emitted_clicks[index] as u64, sample_rate)
        else {
            continue;
        };
        let Some(t_capture) = wall_time_of(in_stamps, recorded_clicks[index] as u64, sample_rate)
        else {
            continue;
        };
        if t_capture > t_play {
            delays.push((t_capture - t_play) as f64 / 1_000_000.0);
        }
    }
    if delays.is_empty() {
        return None;
    }
    delays.sort_by(f64::total_cmp);
    let median = delays[delays.len() / 2];
    let spread = delays[delays.len() - 1] - delays[0];
    Some((median, spread))
}

/// 推送脉冲串并录音，返回（已推送样本数, 录音）。
/// 按**墙钟时间**推进发射序列，同时把录音抽干。
///
/// 为什么不是「每次 sleep 1 ms 就推 1 ms 的量」：Windows 的 `sleep(1ms)` 实际睡 ~15 ms，
/// 那样推得比设备慢，播放侧会大面积欠载（实测 3 秒欠载 16 704 样本），测出来的延迟也没意义。
/// 改成「按已流逝时间算出应该推到哪」之后，粗粒度睡眠自动被补偿，环缓冲占用也保持很低。
/// 维持「半满」的播放环：既不欠载，也不把延迟堆到环满。
///
/// 为什么不能按墙钟硬推：环满时只能停推，于是「发射序号 ↔ 真实时间」不再是线性的，
/// 8 个脉冲就无法用同一个 lag 对齐，相关度会塌到 0.3 左右（实测就是这样）。
/// 改成「盯住占用量、缺多少补多少」之后，推送节奏自动跟随设备消费，映射重新变成线性。
///
/// 返回（已推送样本数, 录音, 占用最小值, 占用最大值）。
fn pump_impulse(
    mut from_capture: rtrb::Consumer<f32>,
    mut to_playout: rtrb::Producer<f32>,
    train: &[f32],
    sample_rate: u32,
    stop: Arc<AtomicBool>,
    seconds: f64,
    playout_capacity: usize,
) -> (usize, Vec<f32>, usize, usize) {
    let mut recorded: Vec<f32> = Vec::with_capacity((seconds * f64::from(sample_rate)) as usize);
    let mut emitted = 0_usize;
    let capacity = playout_capacity;
    let target_occupancy = capacity / 2;
    let mut min_occupancy = usize::MAX;
    let mut max_occupancy = 0_usize;
    let start = Instant::now();

    loop {
        if start.elapsed().as_secs_f64() >= seconds.max(0.1) || stop.load(Ordering::Relaxed) {
            break;
        }

        let occupancy = capacity.saturating_sub(to_playout.slots());
        let mut to_push = target_occupancy.saturating_sub(occupancy);
        while to_push > 0 && emitted < train.len() {
            if to_playout.push(train[emitted]).is_err() {
                break;
            }
            emitted += 1;
            to_push -= 1;
        }

        while let Ok(sample) = from_capture.pop() {
            recorded.push(sample);
        }

        min_occupancy = min_occupancy.min(occupancy);
        max_occupancy = max_occupancy.max(occupancy);
        std::thread::sleep(Duration::from_millis(1));
    }

    while let Ok(sample) = from_capture.pop() {
        recorded.push(sample);
    }
    (emitted, recorded, min_occupancy, max_occupancy)
}
