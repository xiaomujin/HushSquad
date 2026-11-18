//! 混音（P4-5）：多路 PCM → 一路输出，带**按人音量 / 静音 / 独听**。
//!
//! # 为什么混音必须在音频线程上、而且零分配
//!
//! 每路的抖动缓冲各自解出 PCM（P4-2），混音是最后一公里。它跑在音频回调里，所以：
//!
//! - 轨数上限是**编译期常数** [`MAX_TRACKS`]，槽位是定长数组，不扩容；
//! - 控制指令（音量/静音/独听/增删轨）走**无锁环**，音频侧只 `pop`，不加锁不分配；
//! - 控制侧（UI 线程）可以 `Mutex`——那不是实时路径，但推送本身仍然是 O(1)。
//!
//! # 增益变化必须是渐变的
//!
//! 直接改增益就是一次阶跃，听感是「咔」。所以每帧把当前增益**线性推到目标值**（一帧 = 10 ms
//! 的渐变），静音与独听也走同一条路——它们只是「目标增益 = 0」。于是：
//!
//! - 点静音 → 10 ms 内淡出，不是硬切；
//! - 别人开独听 → 其余轨 10 ms 内淡出；
//! - 新轨加入 → 从当前增益开始，不会突然蹦出来。
//!
//! # 独听的语义
//!
//! **只要有任一轨独听，没独听的轨就静音**（不管它自己的静音开关）。这是主流语音软件的一致行为，
//! 也是唯一不会让人困惑的定义。多轨同时独听 = 这几轨都出声。
//!
//! # 削波：静态软限幅，不做动态压缩
//!
//! 多路叠加必然可能超过满量程（32 路上限时更甚）。两条路：
//!
//! - **动态限幅器**（带攻击/释放）：波形保住了，但增益会随内容起伏，多人同时说话时听感是「抽气」；
//! - **静态软限幅**（本实现）：拐点 [`SOFT_KNEE`]（约 −3 dBFS）以下**完全不动**，以上按 `tanh` 曲线
//!   压向 ±1。没有状态、没有攻击时间、不会抽气，代价是超过拐点的部分有谐波失真。
//!
//! 选后者：它是「安全网」而不是「效果器」——正常混音（实测 8 路各 0.05 时峰值 0.61）根本碰不到
//! 拐点，只有真的过载时才介入，而且**永远不会有硬削**。过载量仍然如实计数
//! （[`MixStats::clipped_samples`] = 求和后超过满量程的样本数），UI 可以据此提示「混音过载」。

use std::sync::{Arc, Mutex};

use crate::receive::PcmReceiver;
use crate::{FRAME_SAMPLES, Frame};

/// 最大轨数。
///
/// UI 不设人数上限（2026-11-18 决定），这里是混音器的**定长容量**：32 路足以覆盖
/// 「小队语音」的全部真实场景，而槽位仍是预分配数组，音频回调里不扩容。
/// 超出的成员在混音侧拿不到槽位，必须由上层拒绝加入或降级为「只听混音」。
pub const MAX_TRACKS: usize = 32;

/// 音量下限（dB）。到下限当静音处理。
pub const MIN_GAIN_DB: f32 = -60.0;

/// 音量上限（dB）。留 12 dB 是为了「有人麦克风特别小」的场合。
pub const MAX_GAIN_DB: f32 = 12.0;

/// 指令环容量。控制指令很少（点一下一条），64 足够，满了就明确报错。
const COMMAND_CAPACITY: usize = 64;

/// 一帧里最多处理多少条指令。防止某帧指令堆积把回调撑爆。
const MAX_COMMANDS_PER_FRAME: usize = 64;

/// 软限幅的拐点（约 −3.1 dBFS）。低于它原样通过，高于它压向 ±1。
pub const SOFT_KNEE: f32 = 0.7;

/// 一路音源。
///
/// 做成 trait 是为了让混音器能脱离网络/编解码单独测：[`PcmReceiver`](crate::receive::PcmReceiver)
/// 是真实实现，测试里可以用一个直接吐 PCM 的假音源。
pub trait TrackSource: Send {
    /// 取一帧；返回 `false` 表示这一帧没有数据（实现方应已填静音）。
    fn pull_frame(&mut self, out: &mut Frame) -> bool;
}

impl TrackSource for PcmReceiver {
    fn pull_frame(&mut self, out: &mut Frame) -> bool {
        PcmReceiver::pull_frame(self, out)
    }
}

/// 控制指令。
///
/// 全部走一条无锁环：音频线程只 `pop`，控制线程只 `push`。`Box` 在**控制侧**分配，
/// 音频侧只拿到一个已经存在的指针。
pub enum MixCommand {
    /// 在某个槽位放一路音源（覆盖原有）。
    Add {
        /// 槽位（0..[`MAX_TRACKS`]）。
        slot: u8,
        /// 音源。
        source: Box<dyn TrackSource>,
    },
    /// 清空某个槽位。
    Remove {
        /// 槽位。
        slot: u8,
    },
    /// 设置某轨音量（dB）。
    SetGain {
        /// 槽位。
        slot: u8,
        /// 分贝值，会被夹到 [`MIN_GAIN_DB`]..=[`MAX_GAIN_DB`]。
        db: f32,
    },
    /// 设置某轨静音。
    SetMuted {
        /// 槽位。
        slot: u8,
        /// 是否静音。
        muted: bool,
    },
    /// 设置某轨独听。
    SetSolo {
        /// 槽位。
        slot: u8,
        /// 是否独听。
        solo: bool,
    },
    /// 设置主音量（dB）。
    SetMaster {
        /// 分贝值，会被夹到 [`MIN_GAIN_DB`]..=[`MAX_GAIN_DB`]。
        db: f32,
    },
}

/// 控制侧的出错原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixError {
    /// 槽位越界。
    SlotOutOfRange {
        /// 传进来的槽位。
        slot: usize,
    },
    /// 指令环满（音频线程没在消费，或指令刷得太快）。
    CommandQueueFull,
}

impl std::fmt::Display for MixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SlotOutOfRange { slot } => {
                write!(f, "槽位 {slot} 越界（最大 {} 路）", MAX_TRACKS)
            }
            Self::CommandQueueFull => write!(f, "混音指令环已满"),
        }
    }
}

impl std::error::Error for MixError {}

/// 一帧的混音统计。
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct MixStats {
    /// 有音源的轨数。
    pub tracks: u8,
    /// 本帧真正出声的轨数（目标增益 > 0）。
    pub sounding: u8,
    /// 本帧取不到数据的轨数。
    pub underruns: u8,
    /// 本帧**求和后超过满量程**的样本数（会被软限幅压下去，不会硬削）。
    pub clipped_samples: u32,
    /// 软限幅**之前**的峰值（绝对值）。超过 1.0 就说明过载了。
    pub peak: f32,
}

struct Slot {
    source: Option<Box<dyn TrackSource>>,
    gain_db: f32,
    muted: bool,
    solo: bool,
    /// 当前线性增益（逐帧向目标逼近）。
    gain: f32,
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            source: None,
            gain_db: 0.0,
            muted: false,
            solo: false,
            gain: 1.0,
        }
    }
}

/// 静态软限幅：拐点以下原样通过，以上按 `tanh` 压向 ±1。
///
/// 无状态、无攻击/释放时间，所以不会「抽气」；只在过载时介入，因此正常混音完全听不出它。
#[must_use]
pub fn soft_limit(value: f32) -> f32 {
    let magnitude = value.abs();
    if magnitude <= SOFT_KNEE {
        return value;
    }
    let head = 1.0 - SOFT_KNEE;
    let shaped = SOFT_KNEE + head * ((magnitude - SOFT_KNEE) / head).tanh();
    shaped.copysign(value)
}

fn db_to_linear(db: f32) -> f32 {
    if db <= MIN_GAIN_DB {
        0.0
    } else {
        10.0_f32.powf(db / 20.0)
    }
}

/// 混音器（音频线程持有）。
pub struct Mixer {
    slots: [Slot; MAX_TRACKS],
    commands: rtrb::Consumer<MixCommand>,
    master_db: f32,
    master_gain: f32,
}

impl Mixer {
    /// 构造混音器，返回（音频侧, 控制侧）。
    #[must_use]
    pub fn new() -> (Self, MixControl) {
        let (producer, consumer) = rtrb::RingBuffer::new(COMMAND_CAPACITY);
        (
            Self {
                slots: std::array::from_fn(|_| Slot::default()),
                commands: consumer,
                master_db: 0.0,
                master_gain: 1.0,
            },
            MixControl {
                producer: Arc::new(Mutex::new(producer)),
            },
        )
    }

    /// 混一帧。音频线程每 10 ms 调一次，**零分配**。
    pub fn mix(&mut self, out: &mut Frame) -> MixStats {
        self.apply_commands();

        out.fill(0.0);
        let any_solo = self
            .slots
            .iter()
            .any(|slot| slot.solo && slot.source.is_some());

        let mut stats = MixStats::default();
        // 栈上的临时帧：每轨解出来的 PCM 先落这里，再按增益累加。
        let mut scratch: Frame = [0.0; FRAME_SAMPLES];

        for slot in &mut self.slots {
            let Some(source) = slot.source.as_mut() else {
                continue;
            };
            stats.tracks += 1;
            if !source.pull_frame(&mut scratch) {
                stats.underruns += 1;
            }

            let target = if slot.muted || (any_solo && !slot.solo) {
                0.0
            } else {
                db_to_linear(slot.gain_db)
            };
            if target > 0.0 {
                stats.sounding += 1;
            }

            // 一帧内线性推到目标：静音、独听、音量调整都走这条路，所以都不会「咔」。
            let step = (target - slot.gain) / FRAME_SAMPLES as f32;
            let mut gain = slot.gain;
            for (acc, sample) in out.iter_mut().zip(scratch.iter()) {
                gain += step;
                *acc += sample * gain;
            }
            slot.gain = target;
        }

        // 主音量同样渐变。
        let master_target = db_to_linear(self.master_db);
        let master_step = (master_target - self.master_gain) / FRAME_SAMPLES as f32;
        let mut master = self.master_gain;
        for sample in out.iter_mut() {
            master += master_step;
            *sample *= master;
        }
        self.master_gain = master_target;

        // 软限幅并如实计数：过载要让上层看得见，但不许硬削。
        let mut peak = 0.0_f32;
        for sample in out.iter_mut() {
            let value = *sample;
            let magnitude = value.abs();
            if magnitude > peak {
                peak = magnitude;
            }
            if magnitude > 1.0 {
                stats.clipped_samples += 1;
            }
            *sample = soft_limit(value);
        }
        stats.peak = peak;
        stats
    }

    fn apply_commands(&mut self) {
        for _ in 0..MAX_COMMANDS_PER_FRAME {
            let Ok(command) = self.commands.pop() else {
                return;
            };
            match command {
                MixCommand::Add { slot, source } => {
                    if let Some(target) = self.slots.get_mut(usize::from(slot)) {
                        // 新音源从目标增益开始，不从 1.0 渐变过去（那是白给的一次淡入）。
                        target.gain = if target.muted {
                            0.0
                        } else {
                            db_to_linear(target.gain_db)
                        };
                        target.source = Some(source);
                    }
                }
                MixCommand::Remove { slot } => {
                    if let Some(target) = self.slots.get_mut(usize::from(slot)) {
                        target.source = None;
                    }
                }
                MixCommand::SetGain { slot, db } => {
                    if let Some(target) = self.slots.get_mut(usize::from(slot)) {
                        target.gain_db = db.clamp(MIN_GAIN_DB, MAX_GAIN_DB);
                    }
                }
                MixCommand::SetMuted { slot, muted } => {
                    if let Some(target) = self.slots.get_mut(usize::from(slot)) {
                        target.muted = muted;
                    }
                }
                MixCommand::SetSolo { slot, solo } => {
                    if let Some(target) = self.slots.get_mut(usize::from(slot)) {
                        target.solo = solo;
                    }
                }
                MixCommand::SetMaster { db } => {
                    self.master_db = db.clamp(MIN_GAIN_DB, MAX_GAIN_DB);
                }
            }
        }
    }

    /// 某轨当前是否挂了音源。
    ///
    /// 这是**音频线程侧**的状态：控制侧的指令要等下一次 [`Mixer::mix`] 才生效（最多 10 ms）。
    /// 别拿它当「指令已经执行」的凭据。
    #[must_use]
    pub fn has_track(&self, slot: usize) -> bool {
        self.slots.get(slot).is_some_and(|s| s.source.is_some())
    }
}

/// 控制侧（UI 线程持有）。
#[derive(Clone)]
pub struct MixControl {
    producer: Arc<Mutex<rtrb::Producer<MixCommand>>>,
}

impl MixControl {
    /// 在某槽位放一路音源（覆盖原有）。
    pub fn add_track(&self, slot: usize, source: Box<dyn TrackSource>) -> Result<(), MixError> {
        self.push(MixCommand::Add {
            slot: slot_of(slot)?,
            source,
        })
    }

    /// 清空某槽位。
    pub fn remove_track(&self, slot: usize) -> Result<(), MixError> {
        self.push(MixCommand::Remove {
            slot: slot_of(slot)?,
        })
    }

    /// 设置某轨音量（dB）。
    pub fn set_gain_db(&self, slot: usize, db: f32) -> Result<(), MixError> {
        self.push(MixCommand::SetGain {
            slot: slot_of(slot)?,
            db,
        })
    }

    /// 设置某轨静音。
    pub fn set_muted(&self, slot: usize, muted: bool) -> Result<(), MixError> {
        self.push(MixCommand::SetMuted {
            slot: slot_of(slot)?,
            muted,
        })
    }

    /// 设置某轨独听。
    pub fn set_solo(&self, slot: usize, solo: bool) -> Result<(), MixError> {
        self.push(MixCommand::SetSolo {
            slot: slot_of(slot)?,
            solo,
        })
    }

    /// 设置主音量（dB）。
    pub fn set_master_db(&self, db: f32) -> Result<(), MixError> {
        self.push(MixCommand::SetMaster { db })
    }

    fn push(&self, command: MixCommand) -> Result<(), MixError> {
        // 控制侧不是实时路径，这里加锁可以接受；音频侧永远不加锁。
        let mut producer = self
            .producer
            .lock()
            .map_err(|_| MixError::CommandQueueFull)?;
        producer
            .push(command)
            .map_err(|_| MixError::CommandQueueFull)
    }
}

fn slot_of(slot: usize) -> Result<u8, MixError> {
    if slot < MAX_TRACKS {
        Ok(slot as u8)
    } else {
        Err(MixError::SlotOutOfRange { slot })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 假音源：按顺序吐出预先准备的帧，吐完就一直返回「没数据」。
    struct FakeSource {
        frames: Vec<Frame>,
        position: usize,
    }

    impl FakeSource {
        fn dc(value: f32, count: usize) -> Box<dyn TrackSource> {
            Box::new(Self {
                frames: vec![[value; FRAME_SAMPLES]; count],
                position: 0,
            })
        }
    }

    impl TrackSource for FakeSource {
        fn pull_frame(&mut self, out: &mut Frame) -> bool {
            match self.frames.get(self.position) {
                Some(frame) => {
                    *out = *frame;
                    self.position += 1;
                    true
                }
                None => {
                    out.fill(0.0);
                    false
                }
            }
        }
    }

    fn mixer() -> (Mixer, MixControl) {
        Mixer::new()
    }

    /// 跑到增益稳定：连续混两帧，第二帧的增益就已经是目标值。
    fn settle(mixer: &mut Mixer) -> Frame {
        let mut frame: Frame = [0.0; FRAME_SAMPLES];
        mixer.mix(&mut frame);
        mixer.mix(&mut frame);
        frame
    }

    #[test]
    fn empty_mixer_is_silent() {
        let (mut mixer, _control) = mixer();
        let mut frame: Frame = [1.0; FRAME_SAMPLES];
        let stats = mixer.mix(&mut frame);
        assert!(frame.iter().all(|x| *x == 0.0), "没有音源时必须输出静音");
        assert_eq!(stats.tracks, 0);
        assert_eq!(stats.clipped_samples, 0);
    }

    #[test]
    fn two_tracks_sum() {
        let (mut mixer, control) = mixer();
        control
            .add_track(0, FakeSource::dc(0.25, 4))
            .expect("加轨失败");
        control
            .add_track(1, FakeSource::dc(0.125, 4))
            .expect("加轨失败");
        let frame = settle(&mut mixer);
        assert!(
            frame.iter().all(|x| (*x - 0.375).abs() < 1e-6),
            "两路 0.25 + 0.125 应当合成 0.375，实际 {}",
            frame[0]
        );
    }

    #[test]
    fn gain_change_ramps_over_one_frame() {
        // 阶跃改增益会「咔」：第一帧必须是从旧值渐变过来的，第二帧才到目标。
        let (mut mixer, control) = mixer();
        control
            .add_track(0, FakeSource::dc(0.5, 8))
            .expect("加轨失败");
        let mut frame: Frame = [0.0; FRAME_SAMPLES];
        mixer.mix(&mut frame);
        assert!((frame[0] - 0.5).abs() < 1e-6, "第一帧应当是 0 dB");

        // 降到 -6 dB（线性 0.501）。
        control.set_gain_db(0, -6.0).expect("改音量失败");
        mixer.mix(&mut frame);
        assert!(
            frame[0] < 0.5 && frame[0] > 0.25,
            "增益应当渐变，第一帧不该直接跳到目标：{}",
            frame[0]
        );
        mixer.mix(&mut frame);
        let expected = 0.5 * db_to_linear(-6.0);
        assert!(
            (frame[0] - expected).abs() < 1e-5,
            "第二帧应当到目标 {}，实际 {}",
            expected,
            frame[0]
        );
    }

    #[test]
    fn mute_fades_out_instead_of_cutting() {
        let (mut mixer, control) = mixer();
        control
            .add_track(0, FakeSource::dc(0.5, 8))
            .expect("加轨失败");
        let mut frame: Frame = [0.0; FRAME_SAMPLES];
        mixer.mix(&mut frame);
        control.set_muted(0, true).expect("静音失败");
        mixer.mix(&mut frame);
        assert!(
            frame[0] > 0.0 && frame[0] < 0.5,
            "静音应当淡出而不是硬切：{}",
            frame[0]
        );
        mixer.mix(&mut frame);
        assert!(frame.iter().all(|x| *x == 0.0), "第二帧应当完全静音");
    }

    #[test]
    fn solo_silences_everyone_else() {
        let (mut mixer, control) = mixer();
        control
            .add_track(0, FakeSource::dc(0.25, 8))
            .expect("加轨失败");
        control
            .add_track(1, FakeSource::dc(0.25, 8))
            .expect("加轨失败");
        let frame = settle(&mut mixer);
        assert!((frame[0] - 0.5).abs() < 1e-6, "两路都在时应为 0.5");

        control.set_solo(1, true).expect("独听失败");
        let frame = settle(&mut mixer);
        assert!(
            (frame[0] - 0.25).abs() < 1e-6,
            "独听第 1 轨时只剩 0.25，实际 {}",
            frame[0]
        );
    }

    #[test]
    fn several_solo_tracks_all_sound() {
        let (mut mixer, control) = mixer();
        for slot in 0..3 {
            control
                .add_track(slot, FakeSource::dc(0.25, 8))
                .expect("加轨失败");
        }
        control.set_solo(0, true).expect("独听失败");
        control.set_solo(2, true).expect("独听失败");
        let frame = settle(&mut mixer);
        assert!(
            (frame[0] - 0.5).abs() < 1e-6,
            "独听两轨应当是 0.25 + 0.25，实际 {}",
            frame[0]
        );
    }

    #[test]
    fn muted_track_stays_muted_under_solo() {
        // 静音优先于独听：自己点了静音，别人怎么独听都不该把自己放出来。
        let (mut mixer, control) = mixer();
        control
            .add_track(0, FakeSource::dc(0.25, 8))
            .expect("加轨失败");
        control.set_muted(0, true).expect("静音失败");
        control.set_solo(0, true).expect("独听失败");
        let frame = settle(&mut mixer);
        assert!(frame.iter().all(|x| *x == 0.0), "静音 + 独听应当仍是静音");
    }

    #[test]
    fn clipping_is_counted_not_hidden() {
        let (mut mixer, control) = mixer();
        for slot in 0..4 {
            control
                .add_track(slot, FakeSource::dc(0.5, 4))
                .expect("加轨失败");
        }
        let mut frame: Frame = [0.0; FRAME_SAMPLES];
        // 第一帧让 Add 生效，第二帧读稳定后的数字。
        mixer.mix(&mut frame);
        let stats = mixer.mix(&mut frame);
        assert_eq!(frame.iter().filter(|x| x.abs() > 1.0).count(), 0);
        assert!(stats.peak > 1.0, "峰值 {} 应当超过满量程", stats.peak);
        assert_eq!(
            stats.clipped_samples, FRAME_SAMPLES as u32,
            "4 路 0.5 相加是 2.0，整帧都该进软限幅"
        );
        assert!(
            frame.iter().all(|x| x.abs() <= 1.0),
            "软限幅之后绝不该有样本超过满量程"
        );
        assert!(
            frame.iter().all(|x| x.abs() > 0.99),
            "2.0 的输入应当被压到接近满量程"
        );
    }

    #[test]
    fn thirty_two_tracks_reach_the_knee_but_never_clip() {
        // 容量从 8 提到 32 之后，「32 人同时说话」就不再是极限场景：32 × 0.05 = 1.6，
        // 会按设计触发软限幅。这条测试钉住两件事：① 仍然没有硬削（任何样本不超过 ±1）；
        // ② 过载量被如实计数，UI 才能提示「混音过载」。
        let (mut mixer, control) = mixer();
        for slot in 0..MAX_TRACKS {
            control
                .add_track(slot, FakeSource::dc(0.05, 4))
                .expect("加轨失败");
        }
        let mut frame: Frame = [0.0; FRAME_SAMPLES];
        mixer.mix(&mut frame);
        let stats = mixer.mix(&mut frame);
        assert!(
            frame.iter().all(|x| x.abs() <= 1.0),
            "32 路叠加也不许有样本超过满量程"
        );
        assert!(
            stats.peak > 1.0,
            "32 × 0.05 = 1.6，峰值应当超过满量程（实际 {}）",
            stats.peak
        );
        assert_eq!(
            stats.clipped_samples, FRAME_SAMPLES as u32,
            "整帧都该进软限幅"
        );
        assert!(
            frame.iter().all(|x| x.abs() > 0.9),
            "限幅后仍应接近满量程，而不是被压成静音"
        );
    }

    #[test]
    fn soft_limit_is_transparent_below_the_knee() {
        // 正常混音（峰值 0.61 实测）根本碰不到拐点，所以它必须是**完全**透明的。
        for value in [-0.69_f32, -0.5, 0.0, 0.25, 0.5, SOFT_KNEE] {
            assert_eq!(soft_limit(value), value, "{value} 低于拐点，必须原样通过");
        }
    }

    #[test]
    fn soft_limit_never_exceeds_full_scale() {
        for value in [0.71_f32, 0.9, 1.0, 1.5, 2.0, 10.0, 1_000.0] {
            assert!(
                soft_limit(value) <= 1.0,
                "{value} 被限到 {}",
                soft_limit(value)
            );
            assert!(soft_limit(-value) >= -1.0);
            // 单调不减：输入越大输出越大，不能折返。极大输入时 tanh 在 f32 里饱和到 1.0，
            // 所以这里只能是「不减」而不是「严格增」。
            assert!(soft_limit(value) >= soft_limit(value - 0.05));
            if value <= 2.0 {
                assert!(
                    soft_limit(value) > soft_limit(value - 0.05),
                    "{value} 处曲线折返了"
                );
            }
        }
        assert_eq!(soft_limit(0.0), 0.0, "零输入必须是零输出");
    }

    #[test]
    fn master_gain_scales_the_mix() {
        let (mut mixer, control) = mixer();
        control
            .add_track(0, FakeSource::dc(0.4, 8))
            .expect("加轨失败");
        control.set_master_db(-6.0).expect("主音量失败");
        let frame = settle(&mut mixer);
        let expected = 0.4 * db_to_linear(-6.0);
        assert!(
            (frame[0] - expected).abs() < 1e-5,
            "主音量 -6 dB 后应当是 {expected}，实际 {}",
            frame[0]
        );
    }

    #[test]
    fn gain_is_clamped_to_the_allowed_range() {
        let (mut mixer, control) = mixer();
        control
            .add_track(0, FakeSource::dc(0.1, 4))
            .expect("加轨失败");
        control.set_gain_db(0, 999.0).expect("改音量失败");
        let frame = settle(&mut mixer);
        let expected = 0.1 * db_to_linear(MAX_GAIN_DB);
        assert!(
            (frame[0] - expected).abs() < 1e-5,
            "+999 dB 必须被夹到 12 dB"
        );
    }

    #[test]
    fn out_of_range_slot_is_rejected() {
        let (_mixer, control) = mixer();
        assert_eq!(
            control.set_gain_db(MAX_TRACKS, 0.0),
            Err(MixError::SlotOutOfRange { slot: MAX_TRACKS })
        );
    }

    #[test]
    fn underrun_is_counted_per_frame() {
        let (mut mixer, control) = mixer();
        // 只给 1 帧数据，第二帧就该报缺数据。
        control
            .add_track(0, FakeSource::dc(0.25, 1))
            .expect("加轨失败");
        let mut frame: Frame = [0.0; FRAME_SAMPLES];
        let first = mixer.mix(&mut frame);
        assert_eq!(first.underruns, 0);
        let second = mixer.mix(&mut frame);
        assert_eq!(second.underruns, 1, "数据用完必须报缺帧");
        assert!(frame.iter().all(|x| *x == 0.0), "缺帧时该轨贡献静音");
    }

    #[test]
    fn remove_track_frees_the_slot() {
        let (mut mixer, control) = mixer();
        control
            .add_track(0, FakeSource::dc(0.25, 8))
            .expect("加轨失败");
        let mut frame: Frame = [0.0; FRAME_SAMPLES];
        // 指令在 mix 边界生效：加完还没混过，音频侧当然还没有这一轨。
        assert!(!mixer.has_track(0), "指令不该绕过 mix 直接生效");
        mixer.mix(&mut frame);
        assert!(mixer.has_track(0));
        control.remove_track(0).expect("删轨失败");
        mixer.mix(&mut frame);
        assert!(!mixer.has_track(0));
        assert!(frame.iter().all(|x| *x == 0.0));
    }
}
