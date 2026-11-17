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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IncomingPacket {
    /// 发送方标识（v1 用 ssrc）。
    pub sender: u32,
    /// 有效载荷长度。
    pub len: u16,
    /// 载荷本体（Opus 包）。
    pub payload: [u8; MAX_PACKET_BYTES],
}

impl IncomingPacket {
    /// 从切片构造，超长会被截断（Opus 包不会超过 400 字节，截断只是防御）。
    #[must_use]
    pub fn new(sender: u32, payload: &[u8]) -> Self {
        let mut buf = [0_u8; MAX_PACKET_BYTES];
        let len = payload.len().min(MAX_PACKET_BYTES);
        buf[..len].copy_from_slice(&payload[..len]);
        Self {
            sender,
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
    /// 因为环满而丢掉的包数。
    pub packets_dropped_full: u64,
    /// 抖动缓冲解码出的帧数。
    pub frames_decoded: u64,
    /// 抖动缓冲没给出帧（空缓冲 / 错误）的次数。
    pub frames_missing: u64,
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
            if self.buffer.push(packet.as_slice()).is_ok() {
                self.stats.packets_in += 1;
            } else {
                self.stats.packets_dropped_full += 1;
            }
        }

        match self.buffer.pull() {
            Ok(frame) => {
                self.stats.frames_decoded += 1;
                for sample in frame.samples.iter().take(FRAME_SAMPLES) {
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
        for index in 0..10 {
            let payload = encode(&mut codec, index);
            assert!(tx.send(IncomingPacket::new(SENDER, &payload)), "入队失败");
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
        assert_eq!(stats.packets_dropped_full, 0);
    }

    #[test]
    fn packets_from_other_senders_are_ignored() {
        let (mut tx, mut path, _out) = ReceivePath::new(SENDER, 16, 4).expect("构造接收路径");
        let mut codec = OpusCodec::with_defaults().expect("构造编码器");
        let payload = encode(&mut codec, 0);
        assert!(tx.send(IncomingPacket::new(SENDER + 1, &payload)));
        for _ in 0..3 {
            path.tick();
        }
        assert_eq!(path.stats().packets_in, 0, "别的发送方的包不该进本实例");
    }

    #[test]
    fn packet_ring_full_is_reported_not_panicked() {
        // 环容量 2：塞 3 个包，第三个必须返回 false 而不是阻塞或 panic。
        let (mut tx, _path, _out) = ReceivePath::new(SENDER, 2, 4).expect("构造接收路径");
        let packet = IncomingPacket::new(SENDER, &[1, 2, 3]);
        assert!(tx.send(packet));
        assert!(tx.send(packet));
        assert!(!tx.send(packet), "环满应当返回 false");
    }
}
