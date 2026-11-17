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

/// 单个发送方的抖动缓冲。
pub struct JitterBuffer {
    neteq: NetEq,
    next_seq: u16,
    next_timestamp: u32,
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
            next_seq: 0,
            next_timestamp: 0,
        })
    }

    /// 送入一个 Opus 载荷（序列号与时间戳由缓冲自己推进）。
    pub fn push(&mut self, payload: &[u8]) -> Result<(), NetEqError> {
        let header = RtpHeader::new(
            self.next_seq,
            self.next_timestamp,
            SSRC,
            PAYLOAD_TYPE,
            false,
        );
        let packet = AudioPacket::new(header, payload.to_vec(), SAMPLE_RATE, 1, FRAME_MS);
        self.neteq.insert_packet(packet)?;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.next_timestamp = self.next_timestamp.wrapping_add(FRAME_SAMPLES as u32);
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

        for index in 0..10 {
            let frame = tone(index);
            let len = codec.encode(&frame, &mut packet);
            assert!(len > 0, "第 {index} 帧编码失败");
            buffer.push(&packet[..len]).expect("入队失败");
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
