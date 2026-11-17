//! 接收路径（P4-2）：网络侧塞包 → 抖动缓冲解码 → 音频侧取 PCM。
//!
//! # 为什么分成「tick / pull」两半
//!
//! `neteq` 的接口（`AudioDecoder::decode -> Vec<f32>`、`AudioFrame { samples: Vec<f32> }`）**每包
//! 都会分配**，见模块 [`crate::jitter`] 的文档。而 audio-core 的热路径纪律是零分配。两者的
//! 调和办法不是改 neteq，而是**把分配关在另一个线程里**：
//!
//! ```text
//! 网络线程 ──packets(rtrb)──▶ [ReceivePath::tick]  ← 会分配，10 ms 一次
//!                                   │
//!                                   └──pcm(rtrb)──▶ 音频回调 [pull_frame] ← 零分配
//! ```
//!
//! 于是音频线程只做两件事：`rtrb` 出队、混音（P4-5）。线程本身归 P5 的网络循环所有——这里
//! 只提供**可同步测试**的 `tick`，不擅自起线程：没有真实网络源时，多一个线程只是多一份不可
//! 复现的行为。测试里直接调 `tick()`，行为完全确定。
//!
//! # 每个发送方一个实例
//!
//! v1 的 `ReceivePath` 只认**一个** `sender`，多个发送方就建多个实例、各自一条 `pcm` 环，
//! 混音在 P4-5 做。这样「按人音量/静音/独听」才有地方挂——它们是混音**前**的独立增益。

use rtrb::{Consumer, Producer, RingBuffer};

use crate::codec::MAX_PACKET_BYTES;
use crate::jitter::JitterBuffer;
use crate::{FRAME_SAMPLES, Frame};

/// 一个入站包（固定大小，环里不分配）。
///
/// 序列号与时间戳是**载荷的一部分**，不是缓冲自己编的：丢包只能靠序号空洞被发现，
/// 见 [`crate::jitter::JitterBuffer::push`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IncomingPacket {
    /// 发送方标识（v1 用 ssrc）。
    pub sender: u32,
    /// RTP 序列号（16 位回绕）。
    pub seq: u16,
    /// RTP 时间戳（48 kHz 样本，每帧 +480）。
    pub timestamp: u32,
    /// 有效载荷长度。
    pub len: u16,
    /// 载荷本体（Opus 包）。
    pub payload: [u8; MAX_PACKET_BYTES],
}

impl IncomingPacket {
    /// 从切片构造，超长会被截断（Opus 包不会超过 400 字节，截断只是防御）。
    #[must_use]
    pub fn new(sender: u32, seq: u16, timestamp: u32, payload: &[u8]) -> Self {
        let mut buf = [0_u8; MAX_PACKET_BYTES];
        let len = payload.len().min(MAX_PACKET_BYTES);
        buf[..len].copy_from_slice(&payload[..len]);
        Self {
            sender,
            seq,
            timestamp,
            len: len as u16,
            payload: buf,
        }
    }

    /// 有效载荷。
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.payload[..usize::from(self.len)]
    }
}

/// 网络线程侧：把包塞进环。
pub struct PacketSender {
    producer: Producer<IncomingPacket>,
}

impl PacketSender {
    /// 投递一个包。环满时返回 `false`（丢包由调用方计数，不阻塞、不分配）。
    pub fn send(&mut self, packet: IncomingPacket) -> bool {
        self.producer.push(packet).is_ok()
    }
}

/// 接收路径统计（工作线程侧）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReceiveStats {
    /// 入队成功的包数。
    pub packets_in: u64,
    /// 抖动缓冲**拒收**的包数（时间戳跳变越界、或 neteq 内部缓冲满）。
    ///
    /// 注意：包环满导致丢弃发生在发送侧，由 [`PacketSender::send`] 返回 `false` 上报。
    pub packets_rejected: u64,
    /// 从序列号空洞**推断**出来的丢包数（网络丢的，不是我们丢的）。
    pub packets_lost: u64,
    /// 抖动缓冲解码出的帧数。
    pub frames_decoded: u64,
    /// 抖动缓冲没给出帧（空缓冲 / 错误）的次数。
    pub frames_missing: u64,
    /// 抖动缓冲给出的帧长不是 480 的次数（时间伸缩会改变帧长，属正常但要能看见）。
    pub frames_off_size: u64,
    /// PCM 环满、丢掉的样本数（音频侧没跟上）。
    pub samples_dropped_full: u64,
}

/// 音频回调侧：只从环里取 PCM，零分配。
pub struct PcmReceiver {
    consumer: Consumer<f32>,
    underruns: u64,
}

impl PcmReceiver {
    /// 取一帧。
    ///
    /// 环里不足一帧时填静音并返回 `false`（上层据此决定 PLC / 渐隐）。
    pub fn pull_frame(&mut self, out: &mut Frame) -> bool {
        let mut filled = 0;
        while filled < FRAME_SAMPLES {
            match self.consumer.pop() {
                Ok(sample) => {
                    out[filled] = sample;
                    filled += 1;
                }
                Err(_) => break,
            }
        }
        if filled == FRAME_SAMPLES {
            return true;
        }
        for sample in out.iter_mut().skip(filled) {
            *sample = 0.0;
        }
        self.underruns += 1;
        false
    }

    /// 下溢次数。
    #[must_use]
    pub fn underruns(&self) -> u64 {
        self.underruns
    }
}

/// 接收路径。
pub struct ReceivePath {
    sender: u32,
    packets: Consumer<IncomingPacket>,
    pcm: Producer<f32>,
    buffer: JitterBuffer,
    stats: ReceiveStats,
    /// 下一个期望的序列号（`None` = 还没收到第一个包）。用来把网络丢包变成可见的数字。
    next_expected: Option<u16>,
}

impl ReceivePath {
    /// 构造。`packet_capacity` 是入站包的环容量，`pcm_frames` 是 PCM 环能放多少帧（10 ms/帧）。
    ///
    /// PCM 环的深度直接等于**接收侧额外延迟**：4 帧 = 40 ms。别开太大，抖动缓冲已经在
    /// 自适应延迟了，再叠一层只是把总延迟推高。
    pub fn new(
        sender: u32,
        packet_capacity: usize,
        pcm_frames: usize,
    ) -> Result<(PacketSender, Self, PcmReceiver), String> {
        let buffer = JitterBuffer::new().map_err(|e| format!("构造抖动缓冲失败：{e}"))?;
        let (producer, packets) = RingBuffer::new(packet_capacity);
        let (pcm, consumer) = RingBuffer::new(pcm_frames * FRAME_SAMPLES);
        Ok((
            PacketSender { producer },
            Self {
                sender,
                packets,
                pcm,
                buffer,
                stats: ReceiveStats::default(),
                next_expected: None,
            },
            PcmReceiver {
                consumer,
                underruns: 0,
            },
        ))
    }

    /// 网络线程每 10 ms 调一次：把到齐的包塞进抖动缓冲，再从中取一帧写进 PCM 环。
    ///
    /// 会分配（neteq 解码），所以**不能在音频回调里调用**。
    pub fn tick(&mut self) {
        while let Ok(packet) = self.packets.pop() {
            if packet.sender != self.sender {
                continue;
            }
            self.note_seq(packet.seq);
            if self
                .buffer
                .push(packet.seq, packet.timestamp, packet.as_slice())
                .is_ok()
            {
                self.stats.packets_in += 1;
            } else {
                self.stats.packets_rejected += 1;
            }
        }

        match self.buffer.pull() {
            Ok(frame) => {
                self.stats.frames_decoded += 1;
                // 时间伸缩会改变帧长（加速时变短、合并时变长）。这里照单全收、只计数：
                // PCM 环是**样本流**，`pull_frame` 永远取 480 个，帧边界不影响它。
                if frame.samples.len() != FRAME_SAMPLES {
                    self.stats.frames_off_size += 1;
                }
                for sample in &frame.samples {
                    // 环满 = 音频侧没跟上，丢掉最旧的写法会把时序搞乱；这里直接丢新样本并计数。
                    if self.pcm.push(*sample).is_err() {
                        self.stats.samples_dropped_full += 1;
                        break;
                    }
                }
            }
            Err(_) => self.stats.frames_missing += 1,
        }
    }

    /// 用序列号空洞把网络丢包数出来。
    ///
    /// 只处理 16 位回绕下的前向空洞：序号往回走（迟到、重复、重排后到）不计丢失——
    /// 那些包 NetEQ 自己会按序号丢弃或重排，我们不该重复记一遍。
    fn note_seq(&mut self, seq: u16) {
        match self.next_expected {
            None => self.next_expected = Some(seq.wrapping_add(1)),
            Some(expected) => {
                let ahead = seq.wrapping_sub(expected);
                if ahead == 0 {
                    self.next_expected = Some(expected.wrapping_add(1));
                } else if ahead < 0x8000 {
                    self.stats.packets_lost += u64::from(ahead);
                    self.next_expected = Some(seq.wrapping_add(1));
                }
            }
        }
    }

    /// 统计。
    #[must_use]
    pub fn stats(&self) -> ReceiveStats {
        self.stats
    }

    /// 抖动缓冲当前深度（毫秒）。
    #[must_use]
    pub fn buffered_ms(&self) -> u32 {
        self.buffer.buffered_ms()
    }

    /// 设置目标延迟的上下限（毫秒），返回实际生效的 `(下限, 上限)`。
    ///
    /// 不调这个，抖动缓冲会用 NetEQ 的默认目标延迟 **80 ms**——那是浏览器场景的调参，
    /// 而附录 A 第 9 行的预算是 L1 10–15 ms / L2 15–20 ms。
    pub fn set_delay_limits(&mut self, min_ms: u32, max_ms: u32) -> (u32, u32) {
        let max = self.buffer.set_maximum_delay(max_ms);
        let min = self.buffer.set_minimum_delay(min_ms);
        (min, max)
    }

    /// 抖动缓冲的目标延迟（毫秒），进测量报告用。
    #[must_use]
    pub fn target_delay_ms(&self) -> u32 {
        self.buffer.target_delay_ms()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioCodec, OpusCodec, SAMPLE_RATE};

    const SENDER: u32 = 7;

    fn tone(frame_index: usize) -> Frame {
        let mut frame: Frame = [0.0; FRAME_SAMPLES];
        for (n, sample) in frame.iter_mut().enumerate() {
            let t = (frame_index * FRAME_SAMPLES + n) as f32 / SAMPLE_RATE as f32;
            *sample = 0.5 * (2.0 * std::f32::consts::PI * 1_000.0 * t).sin();
        }
        frame
    }

    fn encode(codec: &mut OpusCodec, index: usize) -> Vec<u8> {
        let mut packet = [0_u8; MAX_PACKET_BYTES];
        let len = codec.encode(&tone(index), &mut packet);
        assert!(len > 0, "第 {index} 帧编码失败");
        packet[..len].to_vec()
    }

    #[test]
    fn underrun_yields_silence() {
        let (_tx, _path, mut out) = ReceivePath::new(SENDER, 16, 4).expect("构造接收路径");
        let mut frame: Frame = [1.0; FRAME_SAMPLES];
        assert!(!out.pull_frame(&mut frame), "没有数据时不该报告成功");
        assert!(frame.iter().all(|x| *x == 0.0), "下溢必须填静音");
        assert_eq!(out.underruns(), 1);
    }

    #[test]
    fn packets_flow_through_the_jitter_buffer() {
        // 网络侧塞 10 帧 → tick 足够多次 → 音频侧必须取到有声的帧。
        let (mut tx, mut path, mut out) = ReceivePath::new(SENDER, 32, 8).expect("构造接收路径");
        let mut codec = OpusCodec::with_defaults().expect("构造编码器");
        for index in 0..10_u32 {
            let payload = encode(&mut codec, index as usize);
            let packet = IncomingPacket::new(SENDER, index as u16, index * 480, &payload);
            assert!(tx.send(packet), "入队失败");
        }

        let mut frames: Vec<Frame> = Vec::new();
        for _ in 0..20 {
            path.tick();
            let mut frame: Frame = [0.0; FRAME_SAMPLES];
            if out.pull_frame(&mut frame) {
                frames.push(frame);
            }
        }
        assert!(!frames.is_empty(), "一帧都没取到");
        assert!(
            frames.iter().any(|f| f.iter().any(|s| s.abs() > 0.05)),
            "取到的全是静音——解码没生效"
        );
        let stats = path.stats();
        assert_eq!(stats.packets_in, 10, "10 个包都该进抖动缓冲");
        assert_eq!(stats.packets_rejected, 0);
    }

    #[test]
    fn packets_from_other_senders_are_ignored() {
        let (mut tx, mut path, _out) = ReceivePath::new(SENDER, 16, 4).expect("构造接收路径");
        let mut codec = OpusCodec::with_defaults().expect("构造编码器");
        let payload = encode(&mut codec, 0);
        assert!(tx.send(IncomingPacket::new(SENDER + 1, 0, 0, &payload)));
        for _ in 0..3 {
            path.tick();
        }
        assert_eq!(path.stats().packets_in, 0, "别的发送方的包不该进本实例");
    }

    #[test]
    fn packet_ring_full_is_reported_not_panicked() {
        // 环容量 2：塞 3 个包，第三个必须返回 false 而不是阻塞或 panic。
        let (mut tx, _path, _out) = ReceivePath::new(SENDER, 2, 4).expect("构造接收路径");
        let packet = IncomingPacket::new(SENDER, 0, 0, &[1, 2, 3]);
        assert!(tx.send(packet));
        assert!(tx.send(packet));
        assert!(!tx.send(packet), "环满应当返回 false");
    }

    #[test]
    fn sequence_gap_is_counted_and_concealed() {
        // P4-3 的地基：丢掉中间一个包，接收侧必须（1）数出来、（2）让 NetEQ 走 PLC，
        // 而不是把后面的音频往前挪。旧实现自己递增序号，这两件事都不会发生。
        let (mut tx, mut path, mut out) = ReceivePath::new(SENDER, 32, 8).expect("构造接收路径");
        let mut codec = OpusCodec::with_defaults().expect("构造编码器");
        for index in 0..12_u32 {
            if index == 5 {
                continue; // 网络把它丢了
            }
            let payload = encode(&mut codec, index as usize);
            let packet = IncomingPacket::new(SENDER, index as u16, index * 480, &payload);
            assert!(tx.send(packet), "入队失败");
        }

        let mut samples: Vec<f32> = Vec::new();
        for _ in 0..30 {
            path.tick();
            let mut frame: Frame = [0.0; FRAME_SAMPLES];
            if out.pull_frame(&mut frame) {
                samples.extend_from_slice(&frame);
            }
        }

        let stats = path.stats();
        assert_eq!(stats.packets_lost, 1, "序列号空洞应当被数成 1 个丢包");
        assert_eq!(stats.packets_in, 11, "其余 11 个包都该进抖动缓冲");
        assert_eq!(stats.frames_missing, 0, "缓冲里有数据，不该报缺帧");
        assert!(
            samples.iter().any(|s| s.abs() > 0.05),
            "输出全静音——PLC 没跑起来"
        );
    }

    #[test]
    fn sequence_wraparound_is_not_mistaken_for_loss() {
        // 16 位序号回绕是真实存在的（65535 -> 0）。当成丢包会虚报 65 536 个丢包。
        let (mut tx, mut path, _out) = ReceivePath::new(SENDER, 64, 4).expect("构造接收路径");
        let mut codec = OpusCodec::with_defaults().expect("构造编码器");
        let payload = encode(&mut codec, 0);
        // 注意时间戳**不能**跟着序号一起回绕：真实 RTP 里时间戳按样本计数，48 kHz 下要 24.8 小时
        // 才回绕一次，而序号 11 分钟就回绕一次。让两者同步回绕等于伪造一个 11 分钟的时间倒流。
        for (seq, timestamp) in [
            (65_534_u16, 100_000_u32),
            (65_535, 100_480),
            (0, 100_960),
            (1, 101_440),
        ] {
            let packet = IncomingPacket::new(SENDER, seq, timestamp, &payload);
            assert!(tx.send(packet), "入队失败");
        }
        for _ in 0..4 {
            path.tick();
        }
        assert_eq!(path.stats().packets_lost, 0, "回绕不是丢包");
    }

    #[test]
    fn late_and_duplicate_packets_do_not_count_as_loss() {
        // 乱序后到的包（序号往回走）不该被记成丢包，否则弱网统计会双重计数。
        let (mut tx, mut path, _out) = ReceivePath::new(SENDER, 64, 4).expect("构造接收路径");
        let mut codec = OpusCodec::with_defaults().expect("构造编码器");
        let payload = encode(&mut codec, 0);
        for seq in [0_u16, 1, 2, 1, 3] {
            let packet = IncomingPacket::new(SENDER, seq, u32::from(seq) * 480, &payload);
            assert!(tx.send(packet), "入队失败");
        }
        for _ in 0..5 {
            path.tick();
        }
        assert_eq!(path.stats().packets_lost, 0, "重排/重复不是丢包");
    }
}
