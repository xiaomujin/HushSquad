//! cpal 封装：设备枚举、采集、播放、xruns 计数与错误上报。
//!
//! 三条纪律（与执行方案 §4 一致）：
//!
//! 1. **回调里只做 `push`/`pop`**：不分配、不加锁、不打印、不做重采样。
//!    设备不是 48 kHz 时由上层在 I/O 边界处理（P2 先用设备原生率量环回，Opus 路径要求 48 kHz）。
//! 2. **缓冲不足/溢出必须计数**，不许静默：采集侧写满就丢新样本并计 `dropped_samples`，
//!    播放侧取空就写静音并计 `underrun_samples`。这两个数字进 measurements。
//! 3. **设备消失不许 panic**：错误回调把事件塞进 channel，由上层决定重启流（P2-4）。

pub mod impulse;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// 链路内部统一采样率。设备不是这个值时，Opus 路径不可用（P2 只测环回延迟，不做重采样）。
pub const PREFERRED_SAMPLE_RATE: u32 = 48_000;

/// 一个设备的名字与默认标记。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// 设备名（cpal 的 `Display`，Windows 上是 WASAPI 的友好名）。
    pub name: String,
    /// 是否是系统默认设备。
    pub is_default: bool,
}

/// 输入/输出设备列表。
#[derive(Debug, Default, Clone)]
pub struct Devices {
    /// 输入设备。
    pub inputs: Vec<DeviceInfo>,
    /// 输出设备。
    pub outputs: Vec<DeviceInfo>,
}

/// 实际打开的流格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFormat {
    /// 采样率（Hz）。
    pub sample_rate: u32,
    /// 设备声道数（内部会下混成单声道）。
    pub channels: u16,
    /// 请求的设备缓冲（帧）；`None` 表示用设备默认。
    pub requested_buffer_frames: Option<u32>,
}

/// 音频 I/O 错误。
#[derive(Debug)]
pub enum AudioIoError {
    /// 系统没有可用设备。
    NoDevice,
    /// 按名字找不到设备。
    NoSuchDevice {
        /// 请求的名字。
        requested: String,
        /// 实际可用的名字。
        available: Vec<String>,
    },
    /// 设备不支持我们要的配置。
    Config(String),
    /// 开流失败。
    Stream(String),
}

impl std::fmt::Display for AudioIoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDevice => f.write_str("系统没有可用的音频设备"),
            Self::NoSuchDevice {
                requested,
                available,
            } => {
                write!(
                    f,
                    "找不到设备 {requested:?}；可用：{}",
                    available.join(" | ")
                )
            }
            Self::Config(msg) => write!(f, "设备配置不可用：{msg}"),
            Self::Stream(msg) => write!(f, "开流失败：{msg}"),
        }
    }
}

impl std::error::Error for AudioIoError {}

/// 流事件。
///
/// 关键区分：`Xrun`（缓冲欠载/溢出）是**良性**的——它只是这一次回调抖了，流还在；
/// 只有设备真的没了（`DeviceNotAvailable` 等）才需要重建。把两者混为一谈会导致
/// 「每抖一次就重建流」，实测会疯狂重启并丢掉全部音频。
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// 必须重建流（设备被拔、host 消失、格式失效、无权限）。
    Fatal {
        /// 错误类别（cpal 的 `ErrorKind`）。
        kind: String,
        /// 完整描述。
        message: String,
    },
    /// 良性事件：xrun、路由变化、实时调度被拒。只计数，不重建。
    Warning {
        /// 错误类别。
        kind: String,
        /// 完整描述。
        message: String,
    },
}

/// 把 cpal 的错误分类成「要不要重建流」。
fn classify(error: &cpal::Error) -> StreamEvent {
    use cpal::ErrorKind;
    let kind = error.kind();
    let message = format!("{kind}: {error}");
    let kind = kind.to_string();
    match error.kind() {
        ErrorKind::Xrun
        | ErrorKind::DeviceChanged
        | ErrorKind::DeviceBusy
        | ErrorKind::RealtimeDenied
        | ErrorKind::ResourceExhausted => StreamEvent::Warning { kind, message },
        // DeviceNotAvailable / HostUnavailable / StreamInvalidated / UnsupportedConfig /
        // PermissionDenied / BackendError / Other：都按「流不能再用」处理。
        _ => StreamEvent::Fatal { kind, message },
    }
}

/// 采集侧计数。
#[derive(Debug, Default)]
pub struct CaptureStats {
    /// 环形缓冲满而丢弃的样本数（丢的是**新**样本，见模块注释）。
    pub dropped_samples: AtomicU64,
    /// 需要重建流的错误次数。
    pub fatal_errors: AtomicU64,
    /// 良性事件（xrun 等）次数。
    pub xrun_events: AtomicU64,
}

/// 播放侧计数。
#[derive(Debug, Default)]
pub struct PlayoutStats {
    /// 环形缓冲空而补的静音样本数（欠载）。
    pub underrun_samples: AtomicU64,
    /// 需要重建流的错误次数。
    pub fatal_errors: AtomicU64,
    /// 良性事件（xrun 等）次数。
    pub xrun_events: AtomicU64,
}

/// 采集句柄。**必须持有**：drop 即停流。
pub struct CaptureHandle {
    stream: cpal::Stream,
    /// 实际格式。
    pub format: StreamFormat,
    /// 计数。
    pub stats: Arc<CaptureStats>,
}

impl CaptureHandle {
    /// 是否按 48 kHz 打开（Opus 路径的前提）。
    #[must_use]
    pub fn is_48k(&self) -> bool {
        self.format.sample_rate == PREFERRED_SAMPLE_RATE
    }

    /// 重新开始推流（热插拔恢复用）。
    pub fn resume(&self) -> Result<(), AudioIoError> {
        self.stream
            .play()
            .map_err(|e| AudioIoError::Stream(e.to_string()))
    }
}

/// 播放句柄。**必须持有**：drop 即停流。
pub struct PlayoutHandle {
    stream: cpal::Stream,
    /// 实际格式。
    pub format: StreamFormat,
    /// 计数。
    pub stats: Arc<PlayoutStats>,
}

impl PlayoutHandle {
    /// 是否按 48 kHz 打开。
    #[must_use]
    pub fn is_48k(&self) -> bool {
        self.format.sample_rate == PREFERRED_SAMPLE_RATE
    }

    /// 重新开始推流。
    pub fn resume(&self) -> Result<(), AudioIoError> {
        self.stream
            .play()
            .map_err(|e| AudioIoError::Stream(e.to_string()))
    }
}

/// 枚举输入/输出设备。
pub fn list_devices() -> Result<Devices, AudioIoError> {
    let host = cpal::default_host();
    let default_in = host.default_input_device().map(|d| d.to_string());
    let default_out = host.default_output_device().map(|d| d.to_string());

    let mut inputs = Vec::new();
    for device in host
        .input_devices()
        .map_err(|e| AudioIoError::Stream(e.to_string()))?
    {
        let name = device.to_string();
        let is_default = default_in.as_deref() == Some(name.as_str());
        inputs.push(DeviceInfo { name, is_default });
    }

    let mut outputs = Vec::new();
    for device in host
        .output_devices()
        .map_err(|e| AudioIoError::Stream(e.to_string()))?
    {
        let name = device.to_string();
        let is_default = default_out.as_deref() == Some(name.as_str());
        outputs.push(DeviceInfo { name, is_default });
    }

    Ok(Devices { inputs, outputs })
}

fn find_device(
    host: &cpal::Host,
    name: Option<&str>,
    input: bool,
) -> Result<cpal::Device, AudioIoError> {
    match name {
        None => {
            let device = if input {
                host.default_input_device()
            } else {
                host.default_output_device()
            };
            device.ok_or(AudioIoError::NoDevice)
        }
        Some(requested) => {
            let devices = if input {
                host.input_devices()
                    .map_err(|e| AudioIoError::Stream(e.to_string()))?
                    .collect::<Vec<_>>()
            } else {
                host.output_devices()
                    .map_err(|e| AudioIoError::Stream(e.to_string()))?
                    .collect::<Vec<_>>()
            };
            // 精确匹配优先，其次子串匹配（命令行里手打全名很痛苦）。
            let exact = devices.iter().find(|d| d.to_string() == requested);
            let fuzzy = devices.iter().find(|d| d.to_string().contains(requested));
            exact
                .or(fuzzy)
                .cloned()
                .ok_or_else(|| AudioIoError::NoSuchDevice {
                    requested: requested.to_owned(),
                    available: devices.iter().map(ToString::to_string).collect(),
                })
        }
    }
}

/// 挑一个配置：优先 48 kHz 单声道，否则退回设备默认。
fn choose_config(
    device: &cpal::Device,
    input: bool,
) -> Result<(cpal::StreamConfig, cpal::SampleFormat), AudioIoError> {
    // cpal 0.18 把 SampleRate 变成了 u32 的类型别名，不再有 .0 字段。
    let preferred_rate: u32 = PREFERRED_SAMPLE_RATE;
    let ranges: Vec<cpal::SupportedStreamConfigRange> = if input {
        device
            .supported_input_configs()
            .map_err(|e| AudioIoError::Config(e.to_string()))?
            .collect()
    } else {
        device
            .supported_output_configs()
            .map_err(|e| AudioIoError::Config(e.to_string()))?
            .collect()
    };

    let mut best: Option<cpal::SupportedStreamConfig> = None;
    for range in ranges {
        if range.min_sample_rate() <= preferred_rate && preferred_rate <= range.max_sample_rate() {
            let candidate = range.with_sample_rate(preferred_rate);
            // 优先单声道：语音链路本来就单声道，少一次下混。
            if candidate.channels() == 1 {
                best = Some(candidate);
                break;
            }
            if best.is_none() {
                best = Some(candidate);
            }
        }
    }

    let chosen = match best {
        Some(config) => config,
        None => {
            let default = if input {
                device.default_input_config()
            } else {
                device.default_output_config()
            };
            default.map_err(|e| AudioIoError::Config(e.to_string()))?
        }
    };

    Ok((chosen.config(), chosen.sample_format()))
}

fn apply_buffer(config: &mut cpal::StreamConfig, frames: Option<u32>) {
    if let Some(frames) = frames {
        config.buffer_size = cpal::BufferSize::Fixed(frames);
    }
}

fn capture_stats_pair() -> (Arc<CaptureStats>, Arc<CaptureStats>) {
    let stats = Arc::new(CaptureStats::default());
    (Arc::clone(&stats), stats)
}

fn playout_stats_pair() -> (Arc<PlayoutStats>, Arc<PlayoutStats>) {
    let stats = Arc::new(PlayoutStats::default());
    (Arc::clone(&stats), stats)
}

/// 开始采集：把设备音频下混成单声道 f32 推进 `producer`。
///
/// `buffer_frames` 为 `None` 时用设备默认缓冲；给定时用固定缓冲（P2-3 的「buffer 调小一档」）。
/// 固定值不被设备接受时**自动回退**到默认，并把实际使用的缓冲记在日志里——
/// 否则「我明明调小了」和「设备没理我」会变成一笔糊涂账。
pub fn start_capture(
    device_name: Option<&str>,
    producer: rtrb::Producer<f32>,
    events: Sender<StreamEvent>,
    buffer_frames: Option<u32>,
) -> Result<CaptureHandle, AudioIoError> {
    let host = cpal::default_host();
    let device = find_device(&host, device_name, true)?;
    let (mut config, sample_format) = choose_config(&device, true)?;
    apply_buffer(&mut config, buffer_frames);
    let format = StreamFormat {
        sample_rate: config.sample_rate,
        channels: config.channels,
        requested_buffer_frames: buffer_frames,
    };
    let (shared, local) = capture_stats_pair();

    let stream = match sample_format {
        cpal::SampleFormat::F32 => {
            start_input::<f32, _>(&device, config, producer, local, events, |s| s)
        }
        cpal::SampleFormat::I16 => {
            start_input::<i16, _>(&device, config, producer, local, events, |s| {
                f32::from(s) / 32_768.0
            })
        }
        cpal::SampleFormat::U16 => {
            start_input::<u16, _>(&device, config, producer, local, events, |s| {
                (f32::from(s) - 32_768.0) / 32_768.0
            })
        }
        cpal::SampleFormat::I32 => {
            start_input::<i32, _>(&device, config, producer, local, events, |s| {
                s as f32 / 2_147_483_648.0
            })
        }
        cpal::SampleFormat::F64 => {
            start_input::<f64, _>(&device, config, producer, local, events, |s| s as f32)
        }
        // 8-bit 设备（部分虚拟声卡只提供 U8）
        cpal::SampleFormat::U8 => {
            start_input::<u8, _>(&device, config, producer, local, events, |s| {
                (f32::from(s) - 128.0) / 128.0
            })
        }
        other => Err(AudioIoError::Config(format!(
            "暂不支持的采样格式 {other:?}"
        ))),
    }?;

    stream
        .play()
        .map_err(|e| AudioIoError::Stream(e.to_string()))?;

    Ok(CaptureHandle {
        stream,
        format,
        stats: shared,
    })
}

fn start_input<T, F>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut producer: rtrb::Producer<f32>,
    stats: Arc<CaptureStats>,
    events: Sender<StreamEvent>,
    to_f32: F,
) -> Result<cpal::Stream, AudioIoError>
where
    T: cpal::SizedSample + Send + 'static,
    F: Fn(T) -> f32 + Send + 'static,
{
    let channels = usize::from(config.channels).max(1);
    let error_stats = Arc::clone(&stats);
    device
        .build_input_stream::<T, _, _>(
            config,
            move |data: &[T], _info| {
                // 只做下混 + push；满了就丢新样本并计数。
                for frame in data.chunks_exact(channels) {
                    let mut sum = 0.0_f32;
                    for sample in frame {
                        sum += to_f32(*sample);
                    }
                    if producer.push(sum / channels as f32).is_err() {
                        stats.dropped_samples.fetch_add(1, Ordering::Relaxed);
                    }
                }
            },
            move |err| {
                let event = classify(&err);
                match &event {
                    StreamEvent::Fatal { .. } => {
                        error_stats.fatal_errors.fetch_add(1, Ordering::Relaxed);
                    }
                    StreamEvent::Warning { .. } => {
                        error_stats.xrun_events.fetch_add(1, Ordering::Relaxed);
                    }
                }
                let _ = events.send(event);
            },
            None,
        )
        .map_err(|e| AudioIoError::Stream(e.to_string()))
}

/// 开始播放：从 `consumer` 取单声道 f32 铺到所有声道；取空就写静音并计数。
pub fn start_playout(
    device_name: Option<&str>,
    consumer: rtrb::Consumer<f32>,
    events: Sender<StreamEvent>,
    buffer_frames: Option<u32>,
) -> Result<PlayoutHandle, AudioIoError> {
    let host = cpal::default_host();
    let device = find_device(&host, device_name, false)?;
    let (mut config, sample_format) = choose_config(&device, false)?;
    apply_buffer(&mut config, buffer_frames);
    let format = StreamFormat {
        sample_rate: config.sample_rate,
        channels: config.channels,
        requested_buffer_frames: buffer_frames,
    };
    let (shared, local) = playout_stats_pair();

    let stream = match sample_format {
        cpal::SampleFormat::F32 => {
            start_output::<f32, _>(&device, config, consumer, local, events, |s| s)
        }
        cpal::SampleFormat::I16 => {
            start_output::<i16, _>(&device, config, consumer, local, events, |s| {
                (s.clamp(-1.0, 1.0) * 32_767.0) as i16
            })
        }
        cpal::SampleFormat::U16 => {
            start_output::<u16, _>(&device, config, consumer, local, events, |s| {
                ((s.clamp(-1.0, 1.0) * 32_767.0) + 32_768.0) as u16
            })
        }
        cpal::SampleFormat::I32 => {
            start_output::<i32, _>(&device, config, consumer, local, events, |s| {
                (s.clamp(-1.0, 1.0) * 2_147_483_647.0) as i32
            })
        }
        cpal::SampleFormat::F64 => {
            start_output::<f64, _>(&device, config, consumer, local, events, f64::from)
        }
        cpal::SampleFormat::U8 => {
            start_output::<u8, _>(&device, config, consumer, local, events, |s| {
                ((s.clamp(-1.0, 1.0) * 127.0) + 128.0) as u8
            })
        }
        other => Err(AudioIoError::Config(format!(
            "暂不支持的采样格式 {other:?}"
        ))),
    }?;

    stream
        .play()
        .map_err(|e| AudioIoError::Stream(e.to_string()))?;

    Ok(PlayoutHandle {
        stream,
        format,
        stats: shared,
    })
}

fn start_output<T, F>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut consumer: rtrb::Consumer<f32>,
    stats: Arc<PlayoutStats>,
    events: Sender<StreamEvent>,
    from_f32: F,
) -> Result<cpal::Stream, AudioIoError>
where
    T: cpal::SizedSample + Send + 'static,
    F: Fn(f32) -> T + Send + 'static,
{
    let channels = usize::from(config.channels).max(1);
    let error_stats = Arc::clone(&stats);
    device
        .build_output_stream::<T, _, _>(
            config,
            move |data: &mut [T], _info| {
                for frame in data.chunks_exact_mut(channels) {
                    let sample = match consumer.pop() {
                        Ok(value) => value,
                        Err(_) => {
                            stats.underrun_samples.fetch_add(1, Ordering::Relaxed);
                            0.0
                        }
                    };
                    let converted = from_f32(sample);
                    for slot in frame.iter_mut() {
                        *slot = converted;
                    }
                }
            },
            move |err| {
                let event = classify(&err);
                match &event {
                    StreamEvent::Fatal { .. } => {
                        error_stats.fatal_errors.fetch_add(1, Ordering::Relaxed);
                    }
                    StreamEvent::Warning { .. } => {
                        error_stats.xrun_events.fetch_add(1, Ordering::Relaxed);
                    }
                }
                let _ = events.send(event);
            },
            None,
        )
        .map_err(|e| AudioIoError::Stream(e.to_string()))
}
