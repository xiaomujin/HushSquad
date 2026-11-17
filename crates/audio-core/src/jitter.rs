//! 抖动缓冲接缝（P4-2 / D7-1）：把**我们自己的** Opus 解码器插进 `neteq`。
//!
//! # D7-1 的问题与答案
//!
//! 问题（选型报告 §2.6）：「`neteq` 关掉默认 feature 后还能用吗？还是它只提供「包→PCM」一体
//! 接口、必须接受 `ropus` 解码？」
//!
//! 答案是**能用，而且解码器可插拔**：
//!
//! - `cargo tree` 实测：`default-features = false` 之后只剩 `log` / `ringbuf` / `serde` /
//!   `thiserror` / `web-time`，没有 `cpal` / `axum` / `tokio` / `tower` / `ropus`；
//! - `neteq::codec::AudioDecoder` 是公开 trait，`NetEq::register_decoder(payload_type, Box<dyn
//!   AudioDecoder + Send>)` 收任意实现。于是我们保留自己的 libopus 解码（与编码器同源，
//!   FEC / PLC 行为一致），**不引入第二个 Opus 实现**——两个实现意味着两种丢包恢复行为，
//!   这种不一致在弱网下最难查。
//!
//! # 代价：它会分配（这是 P4-2 的架构前提）
//!
//! `AudioDecoder::decode` 的签名是 `-> Result<Vec<f32>>`，`get_audio()` 返回的 `AudioFrame`
//! 里也是 `Vec<f32>`：**neteq 每包、每 tick 都会堆分配**。这与 audio-core 的「热路径零分配」
//! 纪律直接冲突，所以抖动缓冲**不能**在音频回调里跑。两条可行路线（P4-2 要选一条）：
//!
//! 1. 抖动缓冲跑在**网络线程**上，解出来的 PCM 经无锁环（`rtrb`）交给音频线程——音频线程仍然
//!    零分配，代价是环形缓冲本身要吃一次拷贝与一点延迟；
//! 2. 换一个不分配的缓冲实现（自己写或改 `jittr`），失去 NetEQ 的时间伸缩。
//!
//! 倾向 1：时间伸缩正是 NetEQ 存在的理由，而「网络线程分配」本身不是问题。

use neteq::codec::AudioDecoder;
use neteq::{AudioPacket, NetEq, NetEqConfig, NetEqError, RtpHeader};

use crate::{AudioCodec, FRAME_MS, FRAME_SAMPLES, Frame, OpusCodec, SAMPLE_RATE};

/// 我们自己的 RTP 载荷类型（v1 只有一个 Opus 档，取 96 是惯例上的动态载荷起点）。
pub const PAYLOAD_TYPE: u8 = 96;

/// 同步源标识。v1 每个发送方一个实例，这里用固定值，接入 P5 后换成真实 ssrc。
pub const SSRC: u32 = 0x4853_5100;

/// 把 [`OpusCodec`] 适配成 `neteq` 的解码器。
///
/// 解码缓冲在构造期分配一次，`decode` 里只做「解到预分配的 Frame，再 `to_vec`」——
/// 那个 `to_vec` 是 trait 签名逼出来的，躲不掉（见模块文档）。
pub struct OpusDecoderAdapter {
    codec: OpusCodec,
    frame: Frame,
}

impl OpusDecoderAdapter {
    /// 用默认 Opus 参数构造。
    pub fn new() -> Result<Self, NetEqError> {
        let codec = OpusCodec::with_defaults()
            .map_err(|e| NetEqError::DecoderError(format!("构造 Opus 解码器失败：{e}")))?;
        Ok(Self {
            codec,
            frame: [0.0; FRAME_SAMPLES],
        })
    }
}

impl AudioDecoder for OpusDecoderAdapter {
    fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    fn channels(&self) -> u8 {
        1
    }

    fn decode(&mut self, encoded: &[u8]) -> neteq::Result<Vec<f32>> {
        if self.codec.decode(encoded, false, &mut self.frame) {
            Ok(self.frame.to_vec())
        } else {
            Err(NetEqError::DecoderError("libopus 解码失败".to_owned()))
        }
    }
}

/// 时间戳允许的最大跳变（样本）。5 秒 @ 48 kHz。
///
/// 超过就认为这包不属于本流（对端时钟错乱、或有 bug），直接拒收。这不只是洁癖：
/// `neteq` 0.9.1 的 `PacketBuffer::get_span_duration_ms` 用 `u32` 算 `span_samples * 1000`，
/// 时间戳跨度一大就**乘溢出**——debug 构建 panic、release 构建算出垃圾。而这条路径在
/// 网络线程上，panic 等于断音。拒收一个荒唐的包比赌它不出事便宜得多。
pub const MAX_TIMESTAMP_JUMP: u32 = 48_000 * 5;

/// 抖动缓冲的累计统计快照（P4-4 用）。
///
/// 只把**我们关心的**几项抄出来，不把 `neteq::NetEqStats` 直接漏到公开 API 上：那个结构
/// 里十几个字段的口径随上游版本变，泄漏出去等于给自己钉死一个外部类型。
///
/// 字段全部是**累计值**（除缓冲深度与目标延迟），所以长跑里「末次读数 − 首次读数」就是
/// 这 4 小时里发生过多少时间伸缩。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitterStats {
    /// 当前缓冲深度（毫秒）。不是累计值。
    pub buffered_ms: u32,
    /// 当前缓冲深度（样本）。不是累计值。
    ///
    /// 要这个是因为 `buffered_ms` 是整数毫秒：目标延迟被夹在 15–20 ms，1 ms 的分辨率
    /// 会把「漂移正在被吸收」的过程糊成一堵墙。480 样本 = 10 ms，所以 1 ms = 48 样本。
    pub buffered_samples: u32,
    /// 当前目标延迟（毫秒）。不是累计值。
    pub target_delay_ms: u32,
    /// 被**时间压缩**吃掉的样本数（发送方快了，缓冲涨了）。
    pub removed_samples_for_acceleration: u64,
    /// 被**时间拉伸**补进去的样本数（发送方慢了，缓冲见底）。
    pub inserted_samples_for_deceleration: u64,
    /// PLC 外推出来的样本数。
    ///
    /// **这是上界，不是精确值**：`neteq` 把 Expand 的整个输出帧都记成隐藏样本，而 Expand 实际
    /// 会先吃掉一部分真音频、只外推剩下的一小段。P4-4 实测高报 25 %（自报 93 120，按样本守恒
    /// 推导 68 928）。要用它做账，只能当「造出来的样本数」的**上限**（C-32）。
    pub concealed_samples: u64,
    /// 其中「静音隐藏」的样本数（外推到静音，说明本来就没声音）。
    pub silent_concealed_samples: u64,
    /// PLC 事件次数。
    pub concealment_events: u64,
    /// 迟到被丢掉的包数。
    pub late_packets_discarded: u64,
    /// 缓冲被清空的次数（每次清空都是一次可听的断裂）。
    pub buffer_flushes: u64,
    /// 进过缓冲的包数。
    pub packets_received: u64,
    /// 被判为「抖动尖峰」的次数。非零意味着目标延迟被抬过——长跑里它应当保持 0。
    pub jitter_peaks_found: u16,
    /// 还排着队等解码的**包数**（不是样本数）。
    ///
    /// 这是漂移补偿该用的传感器：`buffered_samples` 把 `leftover_samples` 与
    /// `leftover_time_stretched_samples` 也算了进去，而那两个在 NetEQ 做时间伸缩时会自己涨，
    /// 拿它当反馈会形成正反馈（实测会一路跑到 10 % 的时间压缩）。包数不会。
    pub packets_queued: u32,
}

/// 单个发送方的抖动缓冲。
pub struct JitterBuffer {
    neteq: NetEq,
    /// 上一个被接受的包的时间戳，用来做跳变检查。
    last_timestamp: Option<u32>,
}

impl JitterBuffer {
    /// 按 48 kHz 单声道构造，并注册我们的 Opus 解码器。
    ///
    /// 采样率必须显式传 48 000：`NetEqConfig::default()` 是 **16 000**（报告 §2.6 的提醒 3），
    /// 静默用默认值会让时间伸缩按错误速率工作。
    pub fn new() -> Result<Self, NetEqError> {
        let config = NetEqConfig {
            sample_rate: SAMPLE_RATE,
            channels: 1,
            ..NetEqConfig::default()
        };
        let mut neteq = NetEq::new(config)?;
        neteq.register_decoder(PAYLOAD_TYPE, Box::new(OpusDecoderAdapter::new()?));
        Ok(Self {
            neteq,
            last_timestamp: None,
        })
    }

    /// 送入一个 Opus 载荷。
    ///
    /// **序列号与时间戳必须由发送方给**，不能让缓冲自己递增：自己递增的话，网络丢掉的包在
    /// NetEQ 眼里根本不存在（序号是连续的），它就不会做 PLC，只会把后面的音频往前挪——
    /// 那是「时间压缩」而不是「丢包隐藏」，测出来的数字也就不是弱网下的数字。
    ///
    /// 时间戳按 48 kHz 样本计，每帧推进 [`FRAME_SAMPLES`]；乱序由 NetEQ 自己按序号重排。
    pub fn push(&mut self, seq: u16, timestamp: u32, payload: &[u8]) -> Result<(), NetEqError> {
        if let Some(last) = self.last_timestamp {
            // 有符号距离：前跳为正、回跳（乱序/迟到）为负。
            let jump = timestamp.wrapping_sub(last) as i32;
            if jump.unsigned_abs() > MAX_TIMESTAMP_JUMP {
                return Err(NetEqError::InvalidTimestamp);
            }
        }
        let header = RtpHeader::new(seq, timestamp, SSRC, PAYLOAD_TYPE, false);
        let packet = AudioPacket::new(header, payload.to_vec(), SAMPLE_RATE, 1, FRAME_MS);
        self.neteq.insert_packet(packet)?;
        self.last_timestamp = Some(timestamp);
        Ok(())
    }

    /// 取出一帧（可能来自正常解码、时间伸缩或 PLC）。
    pub fn pull(&mut self) -> Result<neteq::neteq::AudioFrame, NetEqError> {
        self.neteq.get_audio()
    }

    /// 当前缓冲深度（毫秒），进 measurements 用。
    #[must_use]
    pub fn buffered_ms(&self) -> u32 {
        self.neteq.current_buffer_size_ms()
    }

    /// 当前缓冲深度（样本）。
    ///
    /// 比 [`JitterBuffer::buffered_ms`] 细 48 倍：目标延迟被夹在 15–20 ms 时，整数毫秒
    /// 会把「漂移正在被吸收」的过程糊成一堵墙。
    #[must_use]
    pub fn buffered_samples(&self) -> u32 {
        self.neteq.current_buffer_size_samples() as u32
    }

    /// 目标延迟（毫秒）。
    #[must_use]
    pub fn target_delay_ms(&self) -> u32 {
        self.neteq.target_delay_ms()
    }

    /// 设置目标延迟的下限（毫秒），返回生效值。
    pub fn set_minimum_delay(&mut self, delay_ms: u32) -> u32 {
        self.neteq.set_minimum_delay(delay_ms)
    }

    /// 设置目标延迟的上限（毫秒），返回生效值。
    ///
    /// **必须用**：NetEQ 的默认目标延迟是 **80 ms**（浏览器场景的调参），而我们的延迟预算
    /// （附录 A 第 9 行）是 L1 10–15 ms、L2 15–20 ms。不压上限，L1 档从抖动缓冲这一步就超预算。
    pub fn set_maximum_delay(&mut self, delay_ms: u32) -> u32 {
        self.neteq.set_maximum_delay(delay_ms)
    }

    /// 累计统计快照（P4-4 漂移长跑用）。
    ///
    /// 会克隆几个小结构体（`NetEqStats` 里没有 `Vec`，但 `NetworkStatistics` 也不是 `Copy`），
    /// 所以**不要**在音频回调里每帧调它：网络线程每秒一次就够。
    #[must_use]
    pub fn stats(&self) -> JitterStats {
        let stats = self.neteq.get_statistics();
        JitterStats {
            buffered_ms: stats.current_buffer_size_ms,
            buffered_samples: self.neteq.current_buffer_size_samples() as u32,
            target_delay_ms: stats.target_delay_ms,
            removed_samples_for_acceleration: stats.lifetime.removed_samples_for_acceleration,
            inserted_samples_for_deceleration: stats.lifetime.inserted_samples_for_deceleration,
            concealed_samples: stats.lifetime.concealed_samples,
            silent_concealed_samples: stats.lifetime.silent_concealed_samples,
            concealment_events: stats.lifetime.concealment_events,
            late_packets_discarded: stats.lifetime.late_packets_discarded,
            buffer_flushes: stats.lifetime.buffer_flushes,
            packets_received: stats.lifetime.jitter_buffer_packets_received,
            jitter_peaks_found: stats.network.jitter_peaks_found,
            packets_queued: stats.packets_awaiting_decode as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MAX_PACKET_BYTES;

    fn tone(frame_index: usize) -> Frame {
        let mut frame: Frame = [0.0; FRAME_SAMPLES];
        for (n, sample) in frame.iter_mut().enumerate() {
            let t = (frame_index * FRAME_SAMPLES + n) as f32 / SAMPLE_RATE as f32;
            *sample = 0.5 * (2.0 * std::f32::consts::PI * 1_000.0 * t).sin();
        }
        frame
    }

    #[test]
    fn our_opus_decoder_plugs_into_neteq() {
        // D7-1 的验收：关掉 native feature 后，neteq 能用**我们的**解码器出声音。
        // 如果它落回 StubOpusDecoder，输出会是全零，这条测试会失败。
        let mut codec = OpusCodec::with_defaults().expect("构造编码器");
        let mut buffer = JitterBuffer::new().expect("构造抖动缓冲");
        let mut packet = [0_u8; MAX_PACKET_BYTES];

        for index in 0..10_u32 {
            let frame = tone(index as usize);
            let len = codec.encode(&frame, &mut packet);
            assert!(len > 0, "第 {index} 帧编码失败");
            buffer
                .push(index as u16, index * FRAME_SAMPLES as u32, &packet[..len])
                .expect("入队失败");
        }

        let mut loud = 0;
        for _ in 0..40 {
            if let Ok(frame) = buffer.pull()
                && frame.samples.iter().any(|s| s.abs() > 0.05)
            {
                loud += 1;
            }
        }
        assert!(
            loud > 0,
            "neteq 没解出声音——很可能落到了 StubOpusDecoder（全零输出）"
        );
    }

    #[test]
    fn absurd_timestamp_jump_is_rejected_not_panicked() {
        // 对端时钟错乱或恶意包不能把网络线程搞崩：neteq 0.9.1 在时间戳跨度大时会乘溢出。
        let mut codec = OpusCodec::with_defaults().expect("构造编码器");
        let mut packet = [0_u8; MAX_PACKET_BYTES];
        let len = codec.encode(&tone(0), &mut packet);
        assert!(len > 0, "编码失败");

        let mut buffer = JitterBuffer::new().expect("构造抖动缓冲");
        buffer
            .push(0, 100_000, &packet[..len])
            .expect("首包应当被接受");
        // 前跳 10 秒（480 000 样本），远超 MAX_TIMESTAMP_JUMP。
        assert!(
            matches!(
                buffer.push(1, 100_000 + 480_000, &packet[..len]),
                Err(NetEqError::InvalidTimestamp)
            ),
            "时间戳跳变没被拒收"
        );
        // 回跳 10 秒同样拒收（这是「包不属于本流」，不是乱序）。
        assert!(
            matches!(
                buffer.push(2, 100_000_u32.wrapping_sub(480_000), &packet[..len]),
                Err(NetEqError::InvalidTimestamp)
            ),
            "时间戳回跳没被拒收"
        );
        // 正常的 10 ms 步进与小幅乱序仍然放行。
        buffer
            .push(3, 100_480, &packet[..len])
            .expect("正常步进应当被接受");
        buffer
            .push(2, 100_000, &packet[..len])
            .expect("小幅回退（乱序）应当被接受");
    }

    #[test]
    fn default_target_delay_is_far_above_our_budget() {
        // 钉住一个必须处理的事实：NetEQ 的默认目标延迟是 80 ms（浏览器调参），
        // 而附录 A 第 9 行的预算是 L1 10–15 ms / L2 15–20 ms。
        let buffer = JitterBuffer::new().expect("构造抖动缓冲");
        assert_eq!(
            buffer.target_delay_ms(),
            80,
            "库的默认值变了，需要重新核对预算"
        );
        assert_eq!(buffer.buffered_ms(), 0, "空缓冲的深度应为 0");
    }

    #[test]
    fn maximum_delay_can_be_clamped_into_budget() {
        let mut buffer = JitterBuffer::new().expect("构造抖动缓冲");
        let applied = buffer.set_maximum_delay(20);
        assert!(applied <= 20, "上限没生效：{applied}");
        assert!(
            buffer.target_delay_ms() <= 20,
            "目标延迟仍是 {} ms，压不进预算",
            buffer.target_delay_ms()
        );
    }
}
