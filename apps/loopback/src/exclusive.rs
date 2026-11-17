//! WASAPI 独占模式渲染器（仅 Windows）。
//!
//! 为什么必须有它：默认端点上的 APO 链实测是 Realtek 的 `RltkAPOU64` 加 DTS 的
//! `dtsapo3core64`，MFX 与 EFX 各挂两个、合计 5 个 APO。后果是 WASAPI 自己只报 43 ms，
//! 真机环回却量到 198 ms——那 155 ms 对 `IAudioClock` 完全不可见。独占模式按定义不经过
//! 音频引擎与 APO，是唯一能在本机把 D1 压进预算的路径，而且**不需要管理员权限**
//! （改注册表关「音频增强」要提权，独占不要）。
//!
//! 只做渲染：采集侧实测已经是实时的（录音下标除以采样率等于采集回调时刻，误差小于 2 ms），
//! 198 ms 全在播放侧，所以只替换播放这一半，采集继续用 cpal 共享模式。
//!
//! 与共享模式的对齐点：产出的 [`CallbackStamp`] 五个字段语义完全一致，主值仍然走
//! 「回调墙钟」（`wall_nanos`），因此 `main.rs` 里的 `stamp_delay_of` 一行都不用改。

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::thread::JoinHandle;

use audio_io::{CallbackStamp, PlayoutStats, StreamEvent};

/// 独占模式实际协商到的格式与缓冲参数。
#[derive(Debug, Clone)]
pub struct ExclusiveFormat {
    /// 采样率（Hz）。
    pub sample_rate: u32,
    /// 声道数。
    pub channels: u16,
    /// 采样格式的可读名（`f32` / `i16` / `i24-in-32`）。
    pub sample_format: &'static str,
    /// 设备缓冲总帧数（`GetBufferSize`）。独占模式下这是**真值**，不是建议值。
    pub buffer_frames: u32,
    /// 设备周期（帧，`GetDevicePeriod`）。
    pub period_frames: u32,
    /// `GetStreamLatency` 报的流延迟（毫秒）。
    pub stream_latency_ms: f64,
    /// 协商留痕：试过哪些格式、最后用了哪个。
    ///
    /// 写进报告，免得「换台机器就不对」时无从查起。
    pub negotiation: String,
}

/// 把系统定时器精度提到 1 ms，drop 时还原。
///
/// 为什么需要：Windows 默认定时器精度约 15.6 ms，`sleep(1ms)` 实际会睡 15 ms。
/// 这对两条路都是灾难——轮询模式按 15 ms 的节奏补数据，喂不动 6–10 ms 的设备缓冲；
/// `pump_impulse` 同样按 1 ms 的节奏供料，供不上就大面积欠载（实测共享模式 3 s 欠载 1122–1602 样本）。
pub struct TimerResolution {
    #[cfg(windows)]
    active: bool,
}

impl TimerResolution {
    /// 申请 1 ms 精度；失败也不致命，只是退回默认精度。
    #[must_use]
    pub fn raise() -> Self {
        #[cfg(windows)]
        {
            // SAFETY: 无指针参数，只是向 winmm 请求全局定时器精度。
            let active = unsafe { windows::Win32::Media::timeBeginPeriod(1) } == 0;
            Self { active }
        }
        #[cfg(not(windows))]
        {
            Self {}
        }
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        #[cfg(windows)]
        if self.active {
            // SAFETY: 与 raise() 里的 timeBeginPeriod(1) 配对。
            unsafe { windows::Win32::Media::timeEndPeriod(1) };
        }
    }
}

/// 事件驱动的取用方式。
///
/// 实测这台机器的 Realtek 端点只有**轮询**能真正跑起来：事件驱动能 Initialize、能 Start，
/// 但设备时钟恒为 0、`padding` 恒等于缓冲大小，一个样本都不消费。所以不能写死事件驱动。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExclusiveMode {
    /// 先试事件驱动，失败再试轮询（默认）。
    Auto,
    /// 只用事件驱动。
    Event,
    /// 只用轮询：靠 1 ms 定时器精度盯着 `padding` 补数据。
    Poll,
}

/// 独占模式的渲染句柄。**必须持有**：drop 即停流并回收线程。
pub struct ExclusiveRender {
    /// 实际格式。
    pub format: ExclusiveFormat,
    /// 计数（与共享模式同一套类型，统计口径一致）。
    pub stats: Arc<PlayoutStats>,
    /// 实际填过多少次缓冲。为 0 就说明渲染线程根本没被事件唤醒过。
    pub fills: Arc<std::sync::atomic::AtomicU64>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Drop for ExclusiveRender {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// 打开独占模式渲染。
///
/// `buffer_frames` 为 `None` 时用设备默认周期；给定时是**真实的**设备缓冲时长
/// （共享模式下 `BufferSize::Fixed` 只是建议值，独占模式下它决定 `GetBufferSize`）。
///
/// `device_name` 为 `None` 时用系统默认播放端点；否则按端点友好名匹配（精确优先，其次子串）。
pub fn start(
    consumer: rtrb::Consumer<f32>,
    stamps: rtrb::Producer<CallbackStamp>,
    events: mpsc::Sender<StreamEvent>,
    sample_rate: u32,
    buffer_frames: Option<u32>,
    device_name: Option<String>,
    mode: ExclusiveMode,
) -> Result<ExclusiveRender, String> {
    #[cfg(windows)]
    {
        platform::start(
            consumer,
            stamps,
            events,
            sample_rate,
            buffer_frames,
            device_name,
            mode,
        )
    }
    #[cfg(not(windows))]
    {
        let _ = (
            consumer,
            stamps,
            events,
            sample_rate,
            buffer_frames,
            device_name,
            mode,
        );
        Err("独占模式目前只在 Windows(WASAPI) 上实现".to_owned())
    }
}

/// QPC 与 `audio_io::wall_nanos()` 纪元的固定差（纳秒）；非 Windows 返回 `None`。
///
/// 用来把 `CallbackStamp::stream_nanos`（QPC）换算到 `wall_nanos` 的纪元上，
/// 这样「设备时刻 − 回调」才等于真实的设备缓冲深度。
pub fn qpc_epoch_offset_ns() -> Option<i128> {
    #[cfg(windows)]
    {
        platform::qpc_epoch_offset_ns()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[cfg(windows)]
mod platform {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    use audio_io::{CallbackStamp, PlayoutStats, StreamEvent, wall_nanos};
    use windows::Win32::Devices::Properties::DEVPKEY_Device_FriendlyName;
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::Media::Audio::{
        AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, DEVICE_STATE_ACTIVE,
        IAudioClient, IAudioClock, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
        MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
        WAVEFORMATEXTENSIBLE_0, eConsole, eRender,
    };
    use windows::Win32::Media::KernelStreaming::{
        KSAUDIO_SPEAKER_DIRECTOUT, KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE,
    };
    use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
    use windows::Win32::System::Com::STGM_READ;
    use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        CoUninitialize,
    };
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    use windows::Win32::System::Variant::VT_LPWSTR;
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
    use windows::core::GUID;

    /// QPC 频率（Hz）。进程内恒定，取一次就够。
    static FREQ: std::sync::OnceLock<i64> = std::sync::OnceLock::new();

    fn frequency() -> Option<i64> {
        let value = *FREQ.get_or_init(|| {
            let mut frequency = 0_i64;
            // SAFETY: 只写本地变量。
            let _ = unsafe { QueryPerformanceFrequency(&mut frequency) };
            frequency
        });
        (value > 0).then_some(value)
    }

    /// 当前 QPC 时刻（纳秒）。与 cpal 的 `stream_nanos` 同一个时钟域。
    fn qpc_nanos() -> Option<u64> {
        let frequency = frequency()?;
        let mut counter = 0_i64;
        // SAFETY: 只写本地变量。
        unsafe { QueryPerformanceCounter(&mut counter) }.ok()?;
        u64::try_from(i128::from(counter) * 1_000_000_000 / i128::from(frequency)).ok()
    }

    /// QPC 与 `audio_io::wall_nanos()` 纪元的固定差（纳秒）。
    ///
    /// cpal 的 `stream_nanos` 是 QPC，我们的 `wall_nanos` 是进程内的 `Instant`，两者原点不同
    /// （实测差几十分钟，就是机器开机到进程启动那段）。差值恒定，减掉它「设备时刻 − 回调」
    /// 才等于真正的设备缓冲深度；不减就会印出 +3900356 ms 这种没法读的数。
    pub(super) fn qpc_epoch_offset_ns() -> Option<i128> {
        let qpc = i128::from(qpc_nanos()?);
        let wall = i128::from(audio_io::wall_nanos());
        Some(qpc - wall)
    }

    use super::{ExclusiveFormat, ExclusiveMode, ExclusiveRender};

    /// `WAVE_FORMAT_IEEE_FLOAT` 的数值：windows crate 里没导出这个常量，自己写死。
    const IEEE_FLOAT_TAG: u16 = 0x0003;

    /// 采样格式。
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Kind {
        F32,
        Pcm16,
        Pcm24,
    }

    impl Kind {
        fn name(self) -> &'static str {
            match self {
                Self::F32 => "f32",
                Self::Pcm16 => "i16",
                Self::Pcm24 => "i24-in-32",
            }
        }

        /// 容器位宽（`wBitsPerSample`）。
        fn container_bits(self) -> u16 {
            match self {
                Self::Pcm16 => 16,
                Self::F32 | Self::Pcm24 => 32,
            }
        }

        /// 有效位宽（`wValidBitsPerSample`）：i24 装在 32 位容器里，只有高 24 位有效。
        fn valid_bits(self) -> u16 {
            match self {
                Self::F32 => 32,
                Self::Pcm16 => 16,
                Self::Pcm24 => 24,
            }
        }

        fn bytes(self) -> u16 {
            self.container_bits() / 8
        }
    }

    /// 按给定格式构造 `WAVEFORMATEXTENSIBLE`。
    ///
    /// 一律用 EXTENSIBLE 标签（即使 16 位）：Windows 自己给 24 位/浮点就是这么写的，
    /// 少一个分支就少一处「只有 16 位能开」的意外。
    fn make_format(kind: Kind, rate: u32, channels: u16) -> WAVEFORMATEXTENSIBLE {
        let block_align = channels * kind.bytes();
        let extensible =
            std::mem::size_of::<WAVEFORMATEXTENSIBLE>() - std::mem::size_of::<WAVEFORMATEX>();
        WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
                nChannels: channels,
                nSamplesPerSec: rate,
                nAvgBytesPerSec: u32::from(block_align) * rate,
                nBlockAlign: block_align,
                wBitsPerSample: kind.container_bits(),
                cbSize: extensible as u16,
            },
            Samples: WAVEFORMATEXTENSIBLE_0 {
                wValidBitsPerSample: kind.valid_bits(),
            },
            dwChannelMask: KSAUDIO_SPEAKER_DIRECTOUT,
            SubFormat: match kind {
                Kind::F32 => KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
                Kind::Pcm16 | Kind::Pcm24 => KSDATAFORMAT_SUBTYPE_PCM,
            },
        }
    }

    /// 从 `GetMixFormat` 的返回里读出（格式, 采样率, 声道）；认不出就 `None`。
    ///
    /// 全程按偏移 `read_unaligned`：windows crate 把 `WAVEFORMATEXTENSIBLE` 声明成 `packed(1)`，
    /// 而 `SubFormat` 是 GUID（要求 4 字节对齐），对那个字段取引用是未定义行为，编译期直接报错。
    ///
    /// # Safety
    ///
    /// `format` 必须非空、可读；`wFormatTag` 为 EXTENSIBLE 时后面必须真的有扩展字段
    /// （WASAPI 的混合格式保证如此）。
    unsafe fn describe(format: *const WAVEFORMATEX) -> Option<(Kind, u32, u16)> {
        if format.is_null() {
            return None;
        }
        // SAFETY: 调用方保证指针可读；WAVEFORMATEX 本身是 packed(1)，按未对齐读。
        let base = unsafe { std::ptr::read_unaligned(format) };
        let (rate, channels) = (base.nSamplesPerSec, base.nChannels);
        if base.wFormatTag == WAVE_FORMAT_PCM as u16 {
            return match base.wBitsPerSample {
                16 => Some((Kind::Pcm16, rate, channels)),
                32 => Some((Kind::Pcm24, rate, channels)),
                _ => None,
            };
        }
        if base.wFormatTag == IEEE_FLOAT_TAG && base.wBitsPerSample == 32 {
            return Some((Kind::F32, rate, channels));
        }
        if base.wFormatTag == WAVE_FORMAT_EXTENSIBLE as u16 {
            let samples_offset = std::mem::offset_of!(WAVEFORMATEXTENSIBLE, Samples);
            let sub_offset = std::mem::offset_of!(WAVEFORMATEXTENSIBLE, SubFormat);
            let bytes = format.cast::<u8>();
            // SAFETY: 见函数文档，EXTENSIBLE 标签保证扩展字段存在。
            let valid =
                unsafe { std::ptr::read_unaligned(bytes.add(samples_offset).cast::<u16>()) };
            // SAFETY: 同上。
            let sub = unsafe { std::ptr::read_unaligned(bytes.add(sub_offset).cast::<GUID>()) };
            let valid = if valid == 0 {
                base.wBitsPerSample
            } else {
                valid
            };
            if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT && base.wBitsPerSample == 32 {
                return Some((Kind::F32, rate, channels));
            }
            if sub == KSDATAFORMAT_SUBTYPE_PCM {
                return match (base.wBitsPerSample, valid) {
                    (16, _) => Some((Kind::Pcm16, rate, channels)),
                    (32, 24) => Some((Kind::Pcm24, rate, channels)),
                    _ => None,
                };
            }
        }
        None
    }

    /// 读端点属性存储里的友好名。与 cpal 显示的名字同源，所以 `--output` 能直接对上。
    fn friendly_name(device: &IMMDevice) -> Option<String> {
        // SAFETY: device 是有效的 COM 端点；只读属性存储，读到的 PROPVARIANT 由
        // PropVariantClear 释放。
        unsafe {
            let store: IPropertyStore = device.OpenPropertyStore(STGM_READ).ok()?;
            let mut value = store
                .GetValue(&DEVPKEY_Device_FriendlyName as *const _ as *const _)
                .ok()?;
            let variant = &value.Anonymous.Anonymous;
            let name = if variant.vt == VT_LPWSTR {
                let pointer = *(&variant.Anonymous as *const _ as *const *const u16);
                if pointer.is_null() {
                    None
                } else {
                    let mut len = 0_usize;
                    // 上限 32768 个字符：防的不是正常数据，是坏掉的属性存储。
                    while len < 32_768 && *pointer.add(len) != 0 {
                        len += 1;
                    }
                    Some(String::from_utf16_lossy(std::slice::from_raw_parts(
                        pointer, len,
                    )))
                }
            } else {
                None
            };
            let _ = PropVariantClear(&mut value);
            name
        }
    }

    /// 按名字挑播放端点；`None` 就用系统默认。
    fn pick_device(
        enumerator: &IMMDeviceEnumerator,
        requested: Option<&str>,
    ) -> Result<IMMDevice, String> {
        let Some(requested) = requested else {
            // SAFETY: 枚举器有效。
            return unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
                .map_err(|e| format!("取默认播放端点失败：{e}"));
        };
        // SAFETY: 枚举器有效；只枚举活动端点。
        let collection = unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }
            .map_err(|e| format!("枚举播放端点失败：{e}"))?;
        // SAFETY: 同上。
        let count = unsafe { collection.GetCount() }.map_err(|e| format!("取端点数失败：{e}"))?;
        let mut names = Vec::new();
        let mut exact: Option<IMMDevice> = None;
        let mut fuzzy: Option<IMMDevice> = None;
        for index in 0..count {
            // SAFETY: index 小于 GetCount()。
            let Ok(device) = (unsafe { collection.Item(index) }) else {
                continue;
            };
            let name = friendly_name(&device).unwrap_or_default();
            if name == requested {
                exact = Some(device.clone());
            }
            if fuzzy.is_none() && name.contains(requested) {
                fuzzy = Some(device.clone());
            }
            names.push(name);
        }
        if let Some(device) = exact.or(fuzzy) {
            return Ok(device);
        }
        Err(format!(
            "找不到端点 {requested:?}；活动播放端点：{}",
            names.join(" | ")
        ))
    }

    /// 独占模式能不能接受这个格式。
    ///
    /// 返回 `Ok(true)` 才是真支持：`IsFormatSupported` 在共享模式下会用 S_FALSE 表示
    /// 「只能靠自动重采样」，那个值在独占模式下不该出现，一律当不支持处理。
    fn exclusive_supported(client: &IAudioClient, format: &WAVEFORMATEXTENSIBLE) -> bool {
        let mut closest: *mut WAVEFORMATEX = std::ptr::null_mut();
        // SAFETY: format 在调用期间有效；closest 由 WASAPI 用 CoTaskMemAlloc 分配。
        let hr = unsafe {
            client.IsFormatSupported(
                AUDCLNT_SHAREMODE_EXCLUSIVE,
                &format.Format,
                Some(&mut closest),
            )
        };
        if !closest.is_null() {
            // SAFETY: 上面约定的分配方式。
            unsafe { CoTaskMemFree(Some(closest.cast())) };
        }
        hr.0 == 0
    }

    pub(super) fn start(
        consumer: rtrb::Consumer<f32>,
        stamps: rtrb::Producer<CallbackStamp>,
        events: mpsc::Sender<StreamEvent>,
        sample_rate: u32,
        buffer_frames: Option<u32>,
        device_name: Option<String>,
        mode: ExclusiveMode,
    ) -> Result<ExclusiveRender, String> {
        let (init_tx, init_rx) = mpsc::channel::<Result<ExclusiveFormat, String>>();
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(PlayoutStats::default());
        let fills = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let stop_thread = Arc::clone(&stop);
        let stats_thread = Arc::clone(&stats);
        let fills_thread = Arc::clone(&fills);
        let join = std::thread::Builder::new()
            .name("wasapi-exclusive".to_owned())
            .spawn(move || {
                let outcome = run(
                    consumer,
                    stamps,
                    events,
                    sample_rate,
                    buffer_frames,
                    device_name,
                    mode,
                    &stop_thread,
                    &stats_thread,
                    &fills_thread,
                    &init_tx,
                );
                if let Err(message) = outcome {
                    // 初始化已经失败过的话接收端早就走了，发送失败无所谓。
                    let _ = init_tx.send(Err(message));
                }
            })
            .map_err(|e| format!("起独占渲染线程失败：{e}"))?;
        let format = match init_rx.recv() {
            Ok(Ok(format)) => format,
            Ok(Err(message)) => return Err(message),
            Err(_) => return Err("独占渲染线程没报初始化结果就退出了".to_owned()),
        };
        Ok(ExclusiveRender {
            format,
            stats,
            fills,
            stop,
            join: Some(join),
        })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "渲染线程的入参就是这几样，拆成结构体反而更难对"
    )]
    fn run(
        consumer: rtrb::Consumer<f32>,
        mut stamps: rtrb::Producer<CallbackStamp>,
        events: mpsc::Sender<StreamEvent>,
        sample_rate: u32,
        buffer_frames: Option<u32>,
        device_name: Option<String>,
        mode: ExclusiveMode,
        stop: &AtomicBool,
        stats: &PlayoutStats,
        fills: &std::sync::atomic::AtomicU64,
        init_tx: &mpsc::Sender<Result<ExclusiveFormat, String>>,
    ) -> Result<(), String> {
        // COM 是线程本地的：所有 WASAPI 对象都必须在这一个线程上创建与释放。
        // S_FALSE 表示本线程已经初始化过（同样可用），只有负值才算失败。
        // SAFETY: 本线程新建，之前没有别人在这里初始化过 MTA。
        let com_ok = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();

        let device = {
            // SAFETY: 枚举器在本线程上新建，随后立即用于同线程调用。
            let enumerator: IMMDeviceEnumerator = unsafe {
                CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
            }
            .map_err(|e| format!("建 MMDeviceEnumerator 失败：{e}"))?;
            pick_device(&enumerator, device_name.as_deref())?
        };
        // SAFETY: device 有效且在本线程上。
        let client: IAudioClient = unsafe {
            device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| format!("Activate IAudioClient 失败：{e}"))?
        };

        // 混合格式只用来猜「设备最可能接受什么」，随后立刻释放。
        // SAFETY: GetMixFormat 成功时返回 CoTaskMemAlloc 分配的块。
        let mix =
            unsafe { client.GetMixFormat() }.map_err(|e| format!("GetMixFormat 失败：{e}"))?;
        // SAFETY: 见上，指针非空才读。
        let mix_info = unsafe { describe(mix) };
        // SAFETY: 与分配方式配对。
        unsafe { CoTaskMemFree(Some(mix.cast())) };

        let mix_channels = mix_info.map_or(2, |(_, _, channels)| channels).max(1);
        let mut candidates: Vec<(Kind, u32, u16)> = Vec::new();
        if let Some((kind, rate, channels)) = mix_info
            && rate == sample_rate
        {
            candidates.push((kind, rate, channels));
        }
        for kind in [Kind::F32, Kind::Pcm24, Kind::Pcm16] {
            let candidate = (kind, sample_rate, mix_channels);
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }

        let mut default_period: i64 = 0;
        let mut min_period: i64 = 0;
        // SAFETY: 只写本地变量。
        let _ = unsafe { client.GetDevicePeriod(Some(&mut default_period), Some(&mut min_period)) };
        let period = if min_period > 0 {
            min_period
        } else if default_period > 0 {
            default_period
        } else {
            // 兜底 10 ms：拿不到周期时至少还能试一次。
            100_000
        };
        // 独占模式的缓冲时长必须是设备周期的整数倍，而且 1 倍未必被接受：
        // 第一次实测传 0 拿到 AUDCLNT_E_BUFFER_SIZE_ERROR（0x88890016），传 1/2/4 倍也全是它。
        // 所以从 1 倍一路试到 32 倍，取设备真正接受的最小值。
        let durations: Vec<i64> = match buffer_frames {
            Some(frames) => {
                let wanted = i64::from(frames) * 10_000_000 / i64::from(sample_rate);
                let base = (wanted + period - 1) / period.max(1) * period;
                vec![base.max(period), base.max(period) * 2]
            }
            // 从 2 个周期起：1 个周期时「缓冲 == 周期」，事件驱动下设备实测不消费
            // （padding 恒为满），只有 2 个周期以上才留得出一块可写区。
            None => [2_i64, 4, 8, 16, 32]
                .iter()
                .map(|multiple| period * multiple)
                .collect(),
        };

        // 一次把「格式 × 缓冲时长 × 周期性 × 事件驱动」的矩阵试完。
        //
        // 为什么要矩阵而不是按文档推：这台机器 IsFormatSupported 说支持，Initialize 却拒绝；
        // 光看文档猜不出来是哪一维不对，而每一次尝试都是几微秒的事，试完再写结论更省时间。
        let mut attempts: Vec<String> = Vec::new();
        let mut chosen: Option<(WAVEFORMATEXTENSIBLE, Kind, u16, i64, u32)> = None;
        'search: for (kind, rate, channels) in candidates {
            let format = make_format(kind, rate, channels);
            if !exclusive_supported(&client, &format) {
                attempts.push(format!("{rate}Hz/{channels}ch/{}:格式不支持", kind.name()));
                continue;
            }
            // 搜索顺序：事件驱动优先（轮询的填充节奏受 Windows 睡眠粒度支配，实测 1 ms 的 sleep
            // 会睡到 15 ms，比设备消费慢，必然大面积欠载），并且先把**所有**缓冲时长配
            // periodicity = 0 试一遍，再试 periodicity = duration。
            //
            // 顺序很关键：第一次实现把 periodicity 放最内层，结果在「1 个周期 + periodicity=duration」
            // 上就成功了，于是「2 个周期 + periodicity=0」这个标准组合从来没被试到。
            let flag_candidates: &[u32] = match mode {
                ExclusiveMode::Auto => &[AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 0],
                ExclusiveMode::Event => &[AUDCLNT_STREAMFLAGS_EVENTCALLBACK],
                ExclusiveMode::Poll => &[0],
            };
            for &flags in flag_candidates {
                for periodic_mode in 0..3 {
                    for duration in &durations {
                        // 0 = 非周期；period = 每个设备周期唤醒一次（标准独占事件驱动）；
                        // duration = 缓冲与周期相等（本驱动唯一接受的非零组合之一）。
                        let periodicity = match periodic_mode {
                            0 => 0,
                            1 => period,
                            _ => *duration,
                        };
                        // SAFETY: format 是栈上变量，在 Initialize 期间有效；会话 GUID 传 NULL
                        // 表示不复用。失败的 Initialize 不会把 client 置为已初始化，可以接着试。
                        let outcome = unsafe {
                            client.Initialize(
                                AUDCLNT_SHAREMODE_EXCLUSIVE,
                                flags,
                                *duration,
                                periodicity,
                                &format.Format,
                                None,
                            )
                        };
                        match outcome {
                            Ok(()) => {
                                attempts.push(format!(
                                    "{} d={duration} p={periodicity} f={flags:#x}=OK",
                                    kind.name()
                                ));
                                chosen = Some((format, kind, channels, *duration, flags));
                                break 'search;
                            }
                            Err(e) => attempts.push(format!(
                                "{} d={duration} p={periodicity} f={flags:#x}=0x{:08X}",
                                kind.name(),
                                e.code().0
                            )),
                        }
                    }
                }
            }
        }
        let Some((_format, kind, channels, duration, flags)) = chosen else {
            return Err(format!(
                "独占模式没有任何组合被接受（{sample_rate}Hz，设备周期 {period} hns）：{}",
                attempts.join(" ")
            ));
        };

        // 只有事件驱动才需要事件句柄：轮询模式下 SetEventHandle 会拿到
        // AUDCLNT_E_EVENTHANDLE_NOT_EXPECTED。
        let event = if flags & AUDCLNT_STREAMFLAGS_EVENTCALLBACK != 0 {
            // SAFETY: 手动重置事件，只有本线程等待与关闭。
            let handle = unsafe { CreateEventW(None, false, false, None) }
                .map_err(|e| format!("CreateEventW 失败：{e}"))?;
            // SAFETY: handle 是本线程刚创建的有效句柄。
            if let Err(e) = unsafe { client.SetEventHandle(handle) } {
                // SAFETY: 与创建配对。
                let _ = unsafe { CloseHandle(handle) };
                return Err(format!("SetEventHandle 失败：{e}"));
            }
            Some(handle)
        } else {
            None
        };

        // SAFETY: client 已 Initialize，下面这些查询都在同一线程上。
        let buffer =
            unsafe { client.GetBufferSize() }.map_err(|e| format!("GetBufferSize 失败：{e}"))?;
        let period_frames =
            u32::try_from(period * i64::from(sample_rate) / 10_000_000).unwrap_or(buffer);
        // SAFETY: 同上。
        let stream_latency_ms = unsafe { client.GetStreamLatency() }
            .map_or(f64::NAN, |hns| hns.max(0) as f64 / 10_000.0);

        // SAFETY: client 已 Initialize。
        let render: IAudioRenderClient = unsafe { client.GetService::<IAudioRenderClient>() }
            .map_err(|e| format!("取 IAudioRenderClient 失败：{e}"))?;
        // 时钟只用于诊断（「这批样本还有多久才播到」），取不到就退化成 0，不影响主值。
        // SAFETY: 同上。
        let clock = unsafe { client.GetService::<IAudioClock>() }.ok();

        let negotiation = format!(
            "混合格式 {}；采用 {sample_rate}Hz/{channels}ch/{}，缓冲时长 {duration} hns，周期 {period} hns，flags {flags:#x}；尝试 {}",
            mix_info.map_or_else(
                || "认不出".to_owned(),
                |(kind, rate, ch)| format!("{rate}Hz/{ch}ch/{}", kind.name())
            ),
            kind.name(),
            attempts.join(" ")
        );
        let report = ExclusiveFormat {
            sample_rate,
            channels,
            sample_format: kind.name(),
            buffer_frames: buffer,
            period_frames,
            stream_latency_ms,
            negotiation,
        };
        if init_tx.send(Ok(report.clone())).is_err() {
            return Err("独占流建好了但调用方已经走了".to_owned());
        }

        let mut consumer = consumer;
        let mut written: u64 = 0;
        let mut failure: Option<String> = None;

        // 独占模式必须先把整个缓冲填一遍（静音）再 Start。
        //
        // 不填就死锁：没释放过缓冲时 GetCurrentPadding 一直报「满」，available 恒为 0，
        // 事件照来（实测 wait=0）却永远拿不到可写区，于是 3 秒里一次都没填进去、
        // 播放端一个样本都没发出去。
        //
        // 而且不能靠 GetCurrentPadding 判断「能不能填」：Start 之前它可能直接返回错误，
        // 一旦被兜底成「满」，available 就永远是 0，预填根本不会发生（第一次改就踩了这个坑）。
        // 照 MSDN 的做法直接抓整块。
        let prefill = buffer;
        if prefill > 0 {
            // SAFETY: prefill 不超过 GetBufferSize，取到的指针可写 prefill 帧。
            let Ok(destination) = (unsafe { render.GetBuffer(prefill) }) else {
                if let Some(handle) = event {
                    // SAFETY: 与创建配对。
                    let _ = unsafe { CloseHandle(handle) };
                }
                return Err("预填缓冲失败：GetBuffer 被拒".to_owned());
            };
            // SAFETY: 同上；全 0 字节就是静音（f32 与整数 PCM 的 0 都是）。
            unsafe {
                std::ptr::write_bytes(
                    destination,
                    0,
                    prefill as usize * usize::from(channels) * usize::from(kind.bytes()),
                );
            }
            // SAFETY: 与 GetBuffer 配对，帧数一致。
            if let Err(e) = unsafe { render.ReleaseBuffer(prefill, 0) } {
                if let Some(handle) = event {
                    // SAFETY: 与创建配对。
                    let _ = unsafe { CloseHandle(handle) };
                }
                return Err(format!("预填缓冲失败：{e}"));
            }
            let now = wall_nanos();
            let anchor = qpc_nanos().unwrap_or(now);
            let _ = stamps.push(CallbackStamp {
                stream_nanos: anchor,
                first_index: 0,
                frames: u64::from(prefill),
                // 预填的是静音，没从环里取样本：real_frames = 0 让「推入序号 ↔ 流序号」
                // 的映射把它当空气，与共享模式启动时补静音的处理一致。
                real_frames: 0,
                wall_nanos: now,
            });
            written = u64::from(prefill);
        }

        // SAFETY: 缓冲已释放，Start 之后事件才会真正开始流动。
        if let Err(e) = unsafe { client.Start() } {
            if let Some(handle) = event {
                // SAFETY: 与创建配对。
                let _ = unsafe { CloseHandle(handle) };
            }
            return Err(format!("Start 失败：{e}"));
        }
        'outer: while !stop.load(Ordering::Relaxed) {
            if let Some(handle) = event {
                // SAFETY: handle 是有效句柄；200 ms 超时只是为了定期看一眼 stop。
                let wait = unsafe { WaitForSingleObject(handle, 200) };

                if wait == WAIT_TIMEOUT {
                    continue;
                }
                if wait != WAIT_OBJECT_0 {
                    failure = Some(format!("WaitForSingleObject 返回 0x{:08X}", wait.0));
                    break;
                }
            } else {
                // 轮询模式：没有事件可等，小睡一下再看 padding。
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            loop {
                // SAFETY: client 已初始化且流在跑。
                let padding = match unsafe { client.GetCurrentPadding() } {
                    Ok(value) => value,
                    Err(e) => {
                        failure = Some(format!("GetCurrentPadding 失败：{e}"));
                        break 'outer;
                    }
                };
                let available = buffer.saturating_sub(padding);

                if available == 0 {
                    break;
                }
                // SAFETY: available 不超过 GetBufferSize，取到的指针可写 available 帧。
                let destination = match unsafe { render.GetBuffer(available) } {
                    Ok(pointer) => pointer,
                    Err(e) => {
                        failure = Some(format!("GetBuffer 失败：{e}"));
                        break 'outer;
                    }
                };
                // SAFETY: 指针来自 GetBuffer(available)，可写 available 帧。
                let real =
                    unsafe { fill(destination, available, channels, kind, &mut consumer, stats) };
                // SAFETY: 与 GetBuffer 配对，帧数一致。
                if let Err(e) = unsafe { render.ReleaseBuffer(available, 0) } {
                    failure = Some(format!("ReleaseBuffer 失败：{e}"));
                    break 'outer;
                }
                let now = wall_nanos();
                let first_index = written;
                written += u64::from(available);
                // 「这批样本前面还压着多少帧」——用设备时钟问，问不到就当 0。
                let queued = clock_position(clock.as_ref(), first_index);
                // 用 QPC 而不是 wall_nanos 当设备时刻的锚点：cpal 那边的 stream_nanos 就是 QPC，
                // 两边同一时钟域，诊断量（设备时刻 − 回调）才能相减。
                let anchor = qpc_nanos().unwrap_or(now);
                let stamp = CallbackStamp {
                    stream_nanos: anchor + queued * 1_000_000_000 / u64::from(sample_rate),
                    first_index,
                    frames: u64::from(available),
                    real_frames: real,
                    wall_nanos: now,
                };
                fills.fetch_add(1, Ordering::Relaxed);
                // 时间戳环满了就丢：只影响诊断，不影响音频。
                let _ = stamps.push(stamp);
            }
        }

        // SAFETY: 流在跑，Stop 合法。
        let _ = unsafe { client.Stop() };
        if let Some(handle) = event {
            // SAFETY: 与创建配对。
            let _ = unsafe { CloseHandle(handle) };
        }
        drop(render);
        drop(clock);
        drop(client);
        if com_ok {
            // SAFETY: 与本函数开头的 CoInitializeEx 配对；COM 对象已经全部释放。
            unsafe { CoUninitialize() };
        }

        match failure {
            Some(message) => {
                stats.fatal_errors.fetch_add(1, Ordering::Relaxed);
                let _ = events.send(StreamEvent::Fatal {
                    kind: "exclusive".to_owned(),
                    message: message.clone(),
                });
                Err(message)
            }
            None => Ok(()),
        }
    }

    /// 问设备时钟：`first_index` 那一帧前面还压着多少帧没播。
    ///
    /// 只用于诊断，所以任何失败都退化成 0（那时 `stream_nanos` 就等于回调墙钟）。
    fn clock_position(clock: Option<&IAudioClock>, first_index: u64) -> u64 {
        let Some(clock) = clock else {
            return 0;
        };
        let mut position: u64 = 0;
        // SAFETY: 只写本地变量。
        if unsafe { clock.GetPosition(&mut position, None) }.is_err() {
            return 0;
        }
        first_index.saturating_sub(position)
    }

    /// 把环里的样本铺到 WASAPI 缓冲区，返回真正来自环的帧数。
    ///
    /// 环空了就补静音并计欠载——与共享模式同一套口径，不许静默。
    ///
    /// # Safety
    ///
    /// `destination` 必须是 `IAudioRenderClient::GetBuffer(frames)` 返回的、可写
    /// `frames` 帧的指针。WASAPI 保证它按帧对齐。
    unsafe fn fill(
        destination: *mut u8,
        frames: u32,
        channels: u16,
        kind: Kind,
        consumer: &mut rtrb::Consumer<f32>,
        stats: &PlayoutStats,
    ) -> u64 {
        let channels = usize::from(channels);
        let slots = frames as usize * channels;
        let mut real: u64 = 0;
        let mut take = || match consumer.pop() {
            Ok(value) => {
                real += 1;
                value
            }
            Err(_) => {
                stats.underrun_samples.fetch_add(1, Ordering::Relaxed);
                0.0
            }
        };
        match kind {
            Kind::F32 => {
                // SAFETY: 见函数文档；f32 是 4 字节，帧对齐保证按 4 字节对齐。
                let out =
                    unsafe { std::slice::from_raw_parts_mut(destination.cast::<f32>(), slots) };
                for frame in out.chunks_exact_mut(channels) {
                    let value = take();
                    for slot in frame.iter_mut() {
                        *slot = value;
                    }
                }
            }
            Kind::Pcm16 => {
                // SAFETY: 见函数文档；i16 是 2 字节，帧对齐保证按 2 字节对齐。
                let out =
                    unsafe { std::slice::from_raw_parts_mut(destination.cast::<i16>(), slots) };
                for frame in out.chunks_exact_mut(channels) {
                    let value = (take().clamp(-1.0, 1.0) * 32_767.0) as i16;
                    for slot in frame.iter_mut() {
                        *slot = value;
                    }
                }
            }
            Kind::Pcm24 => {
                // SAFETY: 见函数文档；i24 装在 32 位容器里，按 4 字节对齐。
                let out =
                    unsafe { std::slice::from_raw_parts_mut(destination.cast::<i32>(), slots) };
                for frame in out.chunks_exact_mut(channels) {
                    // WASAPI 把 24 位放在 32 位容器的高位，低 8 位补零（与 cpal 读侧一致）。
                    let value = ((take().clamp(-1.0, 1.0) * 8_388_607.0) as i32) << 8;
                    for slot in frame.iter_mut() {
                        *slot = value;
                    }
                }
            }
        }
        real
    }
}
