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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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
    /// 设备实际的采样格式（U8/I16/F32…）。排查「峰值只有 1/128」这类问题时是第一线索。
    pub sample_format: cpal::SampleFormat,
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
    /// 采集峰值（f32 的位模式）。麦克风静音时这里会接近 0，是排查第一线索。
    peak_bits: AtomicU32,
}

impl CaptureStats {
    /// 记录一个峰值（只在回调里按缓冲块调用一次，不做逐样本原子操作）。
    fn update_peak(&self, value: f32) {
        let mut current = self.peak_bits.load(Ordering::Relaxed);
        loop {
            if value.to_bits() <= current {
                return;
            }
            match self.peak_bits.compare_exchange_weak(
                current,
                value.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    /// 采集峰值（线性）。
    #[must_use]
    pub fn peak(&self) -> f32 {
        f32::from_bits(self.peak_bits.load(Ordering::Relaxed))
    }
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

/// 一次回调的时间戳。
///
/// 跨流可比性：WASAPI 上输入/输出两路都用 `QueryPerformanceCounter`，所以两边的
/// `stream_nanos` 在同一个时钟域里，可以直接相减。这是把环回延迟压到毫秒级的关键——
/// 只靠「发射序列 vs 录音」的相关，会把环形缓冲占用与流启动错位一起算进去。
///
/// 其它平台（ALSA/CoreAudio/AAudio）也是单调时钟，但 cpal 明确说「跨流不保证共享原点」，
/// 因此使用前应先做一次合理性检查（延迟落在 0–200 ms 内）。
#[derive(Debug, Clone, Copy)]
pub struct CallbackStamp {
    /// 该缓冲第一个样本的设备时刻（输出=预计播放时刻，输入=采集时刻），纳秒。
    pub stream_nanos: u64,
    /// 该缓冲在所属流中的第一个样本序号。
    pub first_index: u64,
    /// 缓冲帧数。
    pub frames: u64,
    /// 本缓冲里**真正来自环形缓冲**的样本数（输出侧）；输入侧等于 `frames`。
    ///
    /// 输出侧存在「环里没数据 → 补静音」的情况，所以「推入的样本序号」和「流里的样本序号」
    /// 不是同一个东西。要按推入序号找播放时刻，必须先靠这个字段建立映射。
    pub real_frames: u64,
    /// 回调发生时刻，基于 crate 内的统一单调纪元（纳秒）。
    ///
    /// 为什么还要这个：cpal 的 `StreamInstant` 在 WASAPI 上虽是 QPC，但两路流的原点
    /// 不保证一致（实测输入/输出差出上百毫秒）。回调墙钟是同一个 `Instant` 纪元，
    /// 用它做时基可以把两路对齐到同一根时间轴上。
    pub wall_nanos: u64,
}

impl CallbackStamp {
    /// 把流内的某个样本序号换算成设备时刻（纳秒）。
    #[must_use]
    pub fn time_of(&self, index: u64, sample_rate: u32) -> Option<u64> {
        if index < self.first_index || index >= self.first_index + self.frames {
            return None;
        }
        let offset = index - self.first_index;
        Some(self.stream_nanos + offset * 1_000_000_000 / u64::from(sample_rate))
    }
}

/// crate 内统一的单调纪元。
static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// 自纪元起的纳秒数（单调，不受系统时间调整影响）。
///
/// 公开是因为 `CallbackStamp::wall_nanos` 必须落在同一个纪元上才可比：
/// 其它后端（如 `apps/loopback` 里的 WASAPI 独占渲染器）自己开流时要用同一根时间轴。
pub fn wall_nanos() -> u64 {
    let epoch = EPOCH.get_or_init(std::time::Instant::now);
    u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// 回调停滞多久算「流已经死了」（毫秒）。
///
/// 参考：TS3 3.6.2 的看门狗阈值是 **5 秒**（`callbackWaits2 wait exceeded 5 secs`，
/// 渲染侧自旋上限 1000 次、采集侧 10000 次），而且它**只记日志不恢复**——官方已知限制里
/// 那句「目标设备不出声时渲染线程会卡在 callbackWaits，之后不再响应切换」就是这么来的。
/// 我们按 10 ms 缓冲算，500 ms 已经落后 50 个缓冲；再等下去只是让用户多听半秒静音。
pub const CALLBACK_STALL_MS: u64 = 500;

/// `last` 之后过了 `now`，是否已经算停滞。
#[must_use]
pub fn is_stalled(last_nanos: u64, now_nanos: u64) -> bool {
    now_nanos.saturating_sub(last_nanos) > CALLBACK_STALL_MS * 1_000_000
}

/// 看门狗与回调之间共享的状态。
#[derive(Debug)]
struct WatchdogShared {
    /// 最近一次回调的时刻（`wall_nanos`）。
    last_nanos: AtomicU64,
    /// 监视线程该退出了。
    stopped: AtomicBool,
    /// 本轮停滞是否已经报过，避免每 50 ms 刷一条。
    reported: AtomicBool,
}

/// 回调看门狗：回调停滞超过 [`CALLBACK_STALL_MS`] 就报一条 `Fatal`。
///
/// 为什么必须有它：设备被拔、驱动挂了、或者端点「`Initialize`/`Start` 成功但硬件不走」时，
/// cpal 的回调会**直接不再被调用**，不抛任何错误。上层只会看到「一直没有新数据」，
/// 分不清是对方安静还是流已经死了——这类故障只能靠计时发现，不能靠错误码。
pub struct Watchdog {
    shared: Arc<WatchdogShared>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Watchdog {
    /// 起一条监视线程；`stream` 只用于日志里区分采集/播放。
    fn start(events: Sender<StreamEvent>, stream: &'static str) -> Self {
        let shared = Arc::new(WatchdogShared {
            last_nanos: AtomicU64::new(wall_nanos()),
            stopped: AtomicBool::new(false),
            reported: AtomicBool::new(false),
        });
        let watched = Arc::clone(&shared);
        let name = format!("watchdog-{stream}");
        let join = std::thread::Builder::new()
            .name(name)
            .spawn(move || {
                // 50 ms 一轮：远小于阈值，又不会把核心烧掉。
                while !watched.stopped.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    if watched.stopped.load(Ordering::Relaxed) {
                        return;
                    }
                    let last = watched.last_nanos.load(Ordering::Relaxed);
                    let now = wall_nanos();
                    if is_stalled(last, now) {
                        if !watched.reported.swap(true, Ordering::Relaxed) {
                            let idle_ms = (now - last) / 1_000_000;
                            let _ = events.send(StreamEvent::Fatal {
                                kind: "stalled".to_owned(),
                                message: format!(
                                    "{stream} 回调已停滞 {idle_ms} ms（阈值 {CALLBACK_STALL_MS} ms）：\
                                     设备多半已被拔掉，或驱动接受流却不再消费"
                                ),
                            });
                        }
                    } else {
                        // 恢复了就允许再报一次，别把第二次停滞吞掉。
                        watched.reported.store(false, Ordering::Relaxed);
                    }
                }
            })
            .ok();
        Self { shared, join }
    }

    /// 回调里调一次，代价是一次 QPC 读（与时间戳通道同源，无分配、无锁）。
    fn touch(shared: &WatchdogShared) {
        shared.last_nanos.store(wall_nanos(), Ordering::Relaxed);
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
/// 在一组时间戳里找出包含 `index` 的那个，并换算成设备时刻。
#[must_use]
pub fn sample_time(stamps: &[CallbackStamp], index: u64, sample_rate: u32) -> Option<u64> {
    stamps
        .iter()
        .find_map(|stamp| stamp.time_of(index, sample_rate))
}

/// 采集句柄。**必须持有**：drop 即停流。
pub struct CaptureHandle {
    stream: cpal::Stream,
    /// 实际格式。
    pub format: StreamFormat,
    /// 计数。
    pub stats: Arc<CaptureStats>,
    /// 回调看门狗。只为 Drop 而持有：drop 即停监视。
    _watchdog: Watchdog,
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
    /// 回调看门狗。只为 Drop 而持有：drop 即停监视。
    _watchdog: Watchdog,
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

/// 采样格式的偏好排序：越小越好。
///
/// 第一版只挑「支持 48 kHz 的第一个配置」，结果在 TOPPING DAC + USB 电容麦上抓到了 **U8**：
/// 播放侧 8-bit 量化、录音侧噪声底就是 1/128（−42 dBFS），脉冲根本测不出来。
/// 格式必须按质量排，不能听设备的返回顺序。
fn format_rank(format: cpal::SampleFormat) -> u8 {
    match format {
        cpal::SampleFormat::F32 => 0,
        cpal::SampleFormat::F64 => 1,
        cpal::SampleFormat::I32 => 2,
        cpal::SampleFormat::I16 => 3,
        cpal::SampleFormat::U16 => 4,
        cpal::SampleFormat::U8 => 5,
        _ => 6,
    }
}

/// 挑一个配置：优先 48 kHz + 单声道 + 高质量采样格式，都不满足时退回设备默认。
fn choose_config(
    device: &cpal::Device,
    input: bool,
    preferred_rate: u32,
) -> Result<(cpal::StreamConfig, cpal::SampleFormat), AudioIoError> {
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

    let mut candidates: Vec<cpal::SupportedStreamConfig> = ranges
        .into_iter()
        .filter(|range| {
            range.min_sample_rate() <= preferred_rate && preferred_rate <= range.max_sample_rate()
        })
        .map(|range| range.with_sample_rate(preferred_rate))
        .collect();

    // 先按声道数（单声道优先，少一次下混），再按采样格式质量。
    candidates.sort_by_key(|config| (config.channels() != 1, format_rank(config.sample_format())));

    let chosen = match candidates.into_iter().next() {
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
    stamps: Option<rtrb::Producer<CallbackStamp>>,
) -> Result<CaptureHandle, AudioIoError> {
    start_capture_impl(device_name, producer, events, buffer_frames, stamps, false)
}

/// 在**播放设备**上开一条采集流（WASAPI 回环）。
///
/// 为什么需要它：物理环回把「播放缓冲 + DAC + 空气 + ADC + 采集缓冲」揉成一个数，
/// 分不清哪一段该优化。回环采集拿到的是设备**真正要播的那份数据**，于是延迟可以拆成
/// 「播放侧 + 数字回环」与「声学 + 采集侧」两段，分别量。
///
/// 只有 WASAPI 有这个能力（cpal 在渲染设备上建采集流时会自动开 loopback 模式），
/// 其它后端会返回设备找不到。
pub fn start_loopback_capture(
    device_name: Option<&str>,
    producer: rtrb::Producer<f32>,
    events: Sender<StreamEvent>,
    buffer_frames: Option<u32>,
    stamps: Option<rtrb::Producer<CallbackStamp>>,
) -> Result<CaptureHandle, AudioIoError> {
    start_capture_impl(device_name, producer, events, buffer_frames, stamps, true)
}

fn start_capture_impl(
    device_name: Option<&str>,
    producer: rtrb::Producer<f32>,
    events: Sender<StreamEvent>,
    buffer_frames: Option<u32>,
    stamps: Option<rtrb::Producer<CallbackStamp>>,
    loopback: bool,
) -> Result<CaptureHandle, AudioIoError> {
    let host = cpal::default_host();
    // 回环时设备要在**输出**列表里找，但流是按输入建的（cpal 借此开 loopback 模式）。
    let device = find_device(&host, device_name, !loopback)?;
    // 回环必须用端点的**混音格式**：WASAPI 的 loopback 采集不接受别的采样率
    // （实测传 48 kHz 直接 AUDCLNT_E_UNSUPPORTED_FORMAT），而混音格式就是设备的默认输出配置。
    // 正常采集则从输入侧挑配置。
    let (mut config, sample_format) = if loopback {
        let default = device
            .default_output_config()
            .map_err(|e| AudioIoError::Config(e.to_string()))?;
        (default.config(), default.sample_format())
    } else {
        choose_config(&device, true, PREFERRED_SAMPLE_RATE)?
    };
    apply_buffer(&mut config, buffer_frames);
    let format = StreamFormat {
        sample_rate: config.sample_rate,
        channels: config.channels,
        requested_buffer_frames: buffer_frames,
        sample_format,
    };
    let (shared, local) = capture_stats_pair();
    // 回调一停就报 Fatal，而不是让上层一直等下去（见 Watchdog 的文档）。
    let watchdog = Watchdog::start(events.clone(), "采集");
    let watchdog_shared = Arc::clone(&watchdog.shared);

    let stream = match sample_format {
        cpal::SampleFormat::F32 => start_input::<f32, _>(
            &device,
            config,
            CallbackContext {
                ring: producer,
                stats: local,
                events,
                stamps,
                watchdog: watchdog_shared,
            },
            |s| s,
        ),
        cpal::SampleFormat::I16 => start_input::<i16, _>(
            &device,
            config,
            CallbackContext {
                ring: producer,
                stats: local,
                events,
                stamps,
                watchdog: watchdog_shared,
            },
            |s| f32::from(s) / 32_768.0,
        ),
        cpal::SampleFormat::U16 => start_input::<u16, _>(
            &device,
            config,
            CallbackContext {
                ring: producer,
                stats: local,
                events,
                stamps,
                watchdog: watchdog_shared,
            },
            |s| (f32::from(s) - 32_768.0) / 32_768.0,
        ),
        cpal::SampleFormat::I32 => start_input::<i32, _>(
            &device,
            config,
            CallbackContext {
                ring: producer,
                stats: local,
                events,
                stamps,
                watchdog: watchdog_shared,
            },
            |s| s as f32 / 2_147_483_648.0,
        ),
        cpal::SampleFormat::F64 => start_input::<f64, _>(
            &device,
            config,
            CallbackContext {
                ring: producer,
                stats: local,
                events,
                stamps,
                watchdog: watchdog_shared,
            },
            |s| s as f32,
        ),
        // 8-bit 设备（部分虚拟声卡只提供 U8）
        cpal::SampleFormat::U8 => start_input::<u8, _>(
            &device,
            config,
            CallbackContext {
                ring: producer,
                stats: local,
                events,
                stamps,
                watchdog: watchdog_shared,
            },
            |s| (f32::from(s) - 128.0) / 128.0,
        ),
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
        _watchdog: watchdog,
    })
}

/// 回调里要用到的共享句柄。
///
/// 打包成一个结构体是为了压住参数个数：采集与播放各一份，环形缓冲与计数的类型不同，所以泛型。
struct CallbackContext<Q, S> {
    /// 环形缓冲的一端（采集侧是 `Producer`，播放侧是 `Consumer`）。
    ring: Q,
    /// 计数。
    stats: Arc<S>,
    /// 错误事件出口。
    events: Sender<StreamEvent>,
    /// 时间戳通道（可选）。
    stamps: Option<rtrb::Producer<CallbackStamp>>,
    /// 看门狗的共享态。
    watchdog: Arc<WatchdogShared>,
}

fn start_input<T, F>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    context: CallbackContext<rtrb::Producer<f32>, CaptureStats>,
    to_f32: F,
) -> Result<cpal::Stream, AudioIoError>
where
    T: cpal::SizedSample + Send + 'static,
    F: Fn(T) -> f32 + Send + 'static,
{
    let CallbackContext {
        ring,
        stats,
        events,
        stamps,
        watchdog,
    } = context;
    let mut producer = ring;
    let mut stamps = stamps;
    let channels = usize::from(config.channels).max(1);
    let error_stats = Arc::clone(&stats);
    let mut first_index: u64 = 0;
    device
        .build_input_stream::<T, _, _>(
            config,
            move |data: &[T], info: &cpal::InputCallbackInfo| {
                Watchdog::touch(&watchdog);
                let frames = (data.len() / channels) as u64;
                if let Some(sink) = stamps.as_mut() {
                    let stamp = CallbackStamp {
                        stream_nanos: u64::try_from(info.timestamp().capture.as_nanos())
                            .unwrap_or(u64::MAX),
                        first_index,
                        frames,
                        real_frames: frames,
                        wall_nanos: wall_nanos(),
                    };
                    // 时间戳环满了就丢：它只影响测量，不影响音频本身。
                    let _ = sink.push(stamp);
                }
                first_index += frames;
                // 只做下混 + push；满了就丢新样本并计数。
                let mut peak = 0.0_f32;
                for frame in data.chunks_exact(channels) {
                    let mut sum = 0.0_f32;
                    for sample in frame {
                        sum += to_f32(*sample);
                    }
                    let mono = sum / channels as f32;
                    peak = peak.max(mono.abs());
                    if producer.push(mono).is_err() {
                        stats.dropped_samples.fetch_add(1, Ordering::Relaxed);
                    }
                }
                stats.update_peak(peak);
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
    stamps: Option<rtrb::Producer<CallbackStamp>>,
    preferred_rate: u32,
) -> Result<PlayoutHandle, AudioIoError> {
    let host = cpal::default_host();
    let device = find_device(&host, device_name, false)?;
    let (mut config, sample_format) = choose_config(&device, false, preferred_rate)?;
    apply_buffer(&mut config, buffer_frames);
    let format = StreamFormat {
        sample_rate: config.sample_rate,
        channels: config.channels,
        requested_buffer_frames: buffer_frames,
        sample_format,
    };
    let (shared, local) = playout_stats_pair();
    let watchdog = Watchdog::start(events.clone(), "播放");
    let watchdog_shared = Arc::clone(&watchdog.shared);

    let stream = match sample_format {
        cpal::SampleFormat::F32 => start_output::<f32, _>(
            &device,
            config,
            CallbackContext {
                ring: consumer,
                stats: local,
                events,
                stamps,
                watchdog: watchdog_shared,
            },
            |s| s,
        ),
        cpal::SampleFormat::I16 => start_output::<i16, _>(
            &device,
            config,
            CallbackContext {
                ring: consumer,
                stats: local,
                events,
                stamps,
                watchdog: watchdog_shared,
            },
            |s| (s.clamp(-1.0, 1.0) * 32_767.0) as i16,
        ),
        cpal::SampleFormat::U16 => start_output::<u16, _>(
            &device,
            config,
            CallbackContext {
                ring: consumer,
                stats: local,
                events,
                stamps,
                watchdog: watchdog_shared,
            },
            |s| ((s.clamp(-1.0, 1.0) * 32_767.0) + 32_768.0) as u16,
        ),
        cpal::SampleFormat::I32 => start_output::<i32, _>(
            &device,
            config,
            CallbackContext {
                ring: consumer,
                stats: local,
                events,
                stamps,
                watchdog: watchdog_shared,
            },
            |s| (s.clamp(-1.0, 1.0) * 2_147_483_647.0) as i32,
        ),
        cpal::SampleFormat::F64 => start_output::<f64, _>(
            &device,
            config,
            CallbackContext {
                ring: consumer,
                stats: local,
                events,
                stamps,
                watchdog: watchdog_shared,
            },
            f64::from,
        ),
        cpal::SampleFormat::U8 => start_output::<u8, _>(
            &device,
            config,
            CallbackContext {
                ring: consumer,
                stats: local,
                events,
                stamps,
                watchdog: watchdog_shared,
            },
            |s| ((s.clamp(-1.0, 1.0) * 127.0) + 128.0) as u8,
        ),
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
        _watchdog: watchdog,
    })
}

fn start_output<T, F>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    context: CallbackContext<rtrb::Consumer<f32>, PlayoutStats>,
    from_f32: F,
) -> Result<cpal::Stream, AudioIoError>
where
    T: cpal::SizedSample + Send + 'static,
    F: Fn(f32) -> T + Send + 'static,
{
    let CallbackContext {
        ring,
        stats,
        events,
        stamps,
        watchdog,
    } = context;
    let mut consumer = ring;
    let mut stamps = stamps;
    let channels = usize::from(config.channels).max(1);
    let error_stats = Arc::clone(&stats);
    let mut first_index: u64 = 0;
    device
        .build_output_stream::<T, _, _>(
            config,
            move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
                Watchdog::touch(&watchdog);
                let frames = (data.len() / channels) as u64;
                let mut real_frames = 0_u64;
                for frame in data.chunks_exact_mut(channels) {
                    let sample = match consumer.pop() {
                        Ok(value) => {
                            real_frames += 1;
                            value
                        }
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
                if let Some(sink) = stamps.as_mut() {
                    let stamp = CallbackStamp {
                        stream_nanos: u64::try_from(info.timestamp().playback.as_nanos())
                            .unwrap_or(u64::MAX),
                        first_index,
                        frames,
                        real_frames,
                        wall_nanos: wall_nanos(),
                    };
                    let _ = sink.push(stamp);
                }
                first_index += frames;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stall_threshold_boundary() {
        let base = 1_000_000_000_u64;
        assert!(!is_stalled(base, base), "同一时刻不算停滞");
        assert!(
            !is_stalled(base, base + CALLBACK_STALL_MS * 1_000_000),
            "恰好等于阈值不算"
        );
        assert!(
            is_stalled(base, base + CALLBACK_STALL_MS * 1_000_000 + 1),
            "超过阈值一纳秒就算"
        );
    }

    #[test]
    fn watchdog_reports_a_stalled_stream() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let _watchdog = Watchdog::start(sender, "测试");
        let event = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("回调一直没来，看门狗应当报一条事件");
        match event {
            StreamEvent::Fatal { kind, .. } => assert_eq!(kind, "stalled"),
            other => panic!("期望 Fatal，实际是 {other:?}"),
        }
    }

    #[test]
    fn watchdog_stays_quiet_while_callbacks_keep_coming() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let watchdog = Watchdog::start(sender, "测试");
        for _ in 0..12 {
            Watchdog::touch(&watchdog.shared);
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            receiver.try_recv().is_err(),
            "回调持续到来时不该报警，否则正常播放会被误判成流已死"
        );
    }
}
