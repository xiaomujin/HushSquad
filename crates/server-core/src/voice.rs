//! SFU 语音面：6 步校验流水线 + fan-out。
//!
//! ```text
//! 协议草案 §6 的顺序          本实现（有意调整，理由见下）
//! 1 绑定检查                  1 绑定检查
//! 2 epoch 检查                2 格式校验（magic/version/flags/长度）
//! 3 窗口去重                  3 Bind / Probe 特殊包分流
//! 4 AEAD 验证                 4 限速
//! 5 目标集合                  5 epoch 检查
//! 6 fan-out                   6 AEAD 验证
//!                             7 窗口去重
//!                             8 目标集合 + fan-out
//! ```
//!
//! **为什么 AEAD 要挪到窗口前面**：`seq` 在明文头里。如果先按 `seq` 更新窗口，
//! 一个能伪造源地址的离线攻击者只要发一个 `seq` 很大的垃圾包，就能把窗口推到前面，
//! 让随后到达的合法包全被判成「太老」而丢弃 —— 一条不需要带宽的拒绝服务。
//! 代价是每个包多一次 Poly1305 校验（100 字节载荷约 1 µs，8 人 × 100 pps 合计 0.8 ms/s），
//! 换掉一个可被利用的漏洞，划算。
//!
//! **sender_id = 0 保留给服务器自己**。真实发送者从 1 开始分配，所以
//! 服务器生成的包（BindAck / ProbeAck）与任何转发包都不会撞 nonce。
//! 服务器自己的包用每条会话独立的 `server_seq`，因此重绑定也不会重复 nonce。

use std::net::SocketAddr;

use protocol::voice::{
    self as wire, BIND_ACK_PAYLOAD_LEN, Direction, HEADER_LEN, MAX_ON_WIRE, PROBE_PAYLOAD_LEN,
    PacketKind, TAG_LEN, VoiceHeader, VoicePacket, flags,
};
use transport::{Verdict, seal};

use crate::Server;
use crate::metrics::DropReason;
use crate::session::SessionId;

/// 发出去的字节的出口。
///
/// 这一层不碰 socket，所以服务器核心可以在任何地方跑：单元测试里是个 `Vec`，
/// 生产里是 `sendto`，压测里可以是个丢弃器。零分配是刻意的。
pub trait DatagramSink {
    /// 把 `bytes` 发给 `to`。实现**不得**阻塞在慢目标上。
    fn send(&mut self, to: SocketAddr, bytes: &[u8]);
}

/// 收集到 `Vec` 里的 sink，测试与联调工具用。
#[derive(Debug, Default)]
pub struct VecSink {
    /// 按顺序记录下来的 (目标地址, 字节)。
    pub sent: Vec<(SocketAddr, Vec<u8>)>,
}

impl VecSink {
    /// 新建。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 清空。
    pub fn clear(&mut self) {
        self.sent.clear();
    }
}

impl DatagramSink for VecSink {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        self.sent.push((to, bytes.to_vec()));
    }
}

/// 一个入站报文处理完之后发生了什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// 转发给了 `targets` 个人。
    Forwarded {
        /// 收包人数。
        targets: usize,
    },
    /// 回了一个包给发送者（BindAck / ProbeAck）。
    Replied,
    /// 丢弃。
    Dropped(DropReason),
}

impl Server {
    /// 处理一个入站 UDP 报文。
    ///
    /// `datagram` 会被**原地解密**，返回后内容是明文（长度不变）。
    pub fn handle_datagram<S: DatagramSink>(
        &mut self,
        from: SocketAddr,
        datagram: &mut [u8],
        now_ms: u64,
        sink: &mut S,
    ) -> Outcome {
        self.metrics.datagrams_in += 1;
        self.metrics.bytes_in += datagram.len() as u64;
        let outcome = self.process_datagram(from, datagram, now_ms, sink);
        if let Outcome::Dropped(reason) = outcome {
            self.metrics.record_drop(reason);
        }
        outcome
    }

    fn process_datagram<S: DatagramSink>(
        &mut self,
        from: SocketAddr,
        datagram: &mut [u8],
        now_ms: u64,
        sink: &mut S,
    ) -> Outcome {
        // ---- 2. 格式校验：先扔掉一切对不上的东西，越便宜越靠前 ----
        if datagram.len() < HEADER_LEN || datagram.len() > MAX_ON_WIRE {
            return Outcome::Dropped(DropReason::Malformed);
        }
        let Ok(header) = VoiceHeader::parse(datagram) else {
            return Outcome::Dropped(DropReason::Malformed);
        };
        // Bind 是**唯一**不带 tag 的包（服务器在它之前不认密钥），长度算法不一样。
        let is_bind = header.kind() == PacketKind::Bind;
        let expected = HEADER_LEN + header.body_len() + if is_bind { 0 } else { TAG_LEN };
        if datagram.len() != expected {
            return Outcome::Dropped(DropReason::Malformed);
        }

        // ---- 3. Bind 不需要已绑定（它就是来建立绑定的） ----
        if is_bind {
            return self.handle_bind(from, datagram, now_ms, sink);
        }

        // ---- 1. 绑定检查 ----
        let Some(session_id) = self.by_addr.get(&from).copied() else {
            return Outcome::Dropped(DropReason::Unbound);
        };

        // ---- 4. 限速（在昂贵的密码学前） ----
        {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                return Outcome::Dropped(DropReason::Unbound);
            };
            if !session.voice_rate.allow(now_ms) {
                return Outcome::Dropped(DropReason::RateLimited);
            }
        }

        // ---- 5. epoch 检查 ----
        let Some(channel_id) = self.sessions.get(&session_id).and_then(|s| s.channel) else {
            return Outcome::Dropped(DropReason::NoChannel);
        };
        let Some(channel_epoch) = self.channels.get(&channel_id).map(|c| c.epoch) else {
            return Outcome::Dropped(DropReason::NoChannel);
        };
        if header.channel_epoch != channel_epoch {
            return Outcome::Dropped(DropReason::WrongEpoch);
        }

        // ---- 6. AEAD 验证 ----
        let Some(key) = self.sessions.get(&session_id).map(|s| s.keys.c2s.clone()) else {
            return Outcome::Dropped(DropReason::Unbound);
        };
        if seal::open(&key, datagram).is_err() {
            let failures = self
                .sessions
                .get_mut(&session_id)
                .map(|s| {
                    s.auth_failures += 1;
                    s.auth_failures
                })
                .unwrap_or(0);
            if failures >= self.config().auth_failure_limit {
                self.metrics.sessions_killed_bad_tag += 1;
                self.emit(crate::Event::SessionKilled {
                    session: session_id,
                    reason: crate::KillReason::BadTag,
                });
                self.close_session(session_id);
            }
            return Outcome::Dropped(DropReason::BadTag);
        }
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.auth_failures = 0;
        }

        // ---- 6b. 发送者身份：头里的 sender_id 必须就是这条会话的 sender_id ----
        //
        // 没有这一步，任何成员都能把 `sender_id` 填成别人的，服务器会原样转发 ——
        // 接收端按 sender_id 区分说话人，于是这就是一个**冒充**漏洞。
        // 放在 AEAD 之后：这一步只在「包确实由这条会话的密钥签出」之后才有意义，
        // 否则一个线路噪声造成的比特翻转会被误记成冒充。
        if self
            .sessions
            .get(&session_id)
            .is_none_or(|s| s.sender_id != header.sender_id)
        {
            return Outcome::Dropped(DropReason::SenderMismatch);
        }

        // ---- 7. 窗口去重（此时 `seq` 已经过认证，推不动了） ----
        {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                return Outcome::Dropped(DropReason::Unbound);
            };
            if session.replay.accept(header.seq) != Verdict::Fresh {
                return Outcome::Dropped(DropReason::Replay);
            }
        }

        // 解密之后再按完整语义校验一遍（flags 与载荷长度的一致性只有这时才可解释）。
        let Ok(packet) = VoicePacket::parse(datagram, Direction::ClientToServer) else {
            return Outcome::Dropped(DropReason::Malformed);
        };

        // ---- 打点包：原样回送时间戳，不进抖动缓冲 ----
        if packet.kind() == PacketKind::Probe {
            let Ok(ts_us) = wire::parse_probe_payload(packet.body()) else {
                return Outcome::Dropped(DropReason::Malformed);
            };
            self.metrics.probes += 1;
            return self.reply_probe(session_id, from, ts_us, sink);
        }

        // ---- 8. 目标集合 + fan-out ----
        if self.sessions.get(&session_id).is_some_and(|s| s.is_muted()) {
            return Outcome::Dropped(DropReason::ServerMuted);
        }
        let peers = self.channel_peers(session_id);
        if peers.is_empty() {
            return Outcome::Dropped(DropReason::NoTargets);
        }

        let plain_len = datagram.len();
        let mut sent = 0usize;
        for peer in peers {
            let Some(target) = self.sessions.get(&peer) else {
                continue;
            };
            if target.is_deaf() || !target.is_ready() {
                continue;
            }
            let Some(addr) = target.addr else {
                continue;
            };
            let key = target.keys.s2c.clone();
            // `seal` 是原地加密：每一轮都必须从明文重新拷一份，
            // 否则第二轮加密的是上一轮的密文，第二个收包人会拿到双重加密的垃圾。
            self.scratch.clear();
            self.scratch.extend_from_slice(&datagram[..plain_len]);
            if seal::seal(&key, &mut self.scratch).is_err() {
                // 长度已经校验过，这里只可能是内部不一致 —— 宁可丢包也不发垃圾。
                continue;
            }
            sink.send(addr, &self.scratch);
            self.metrics.datagrams_out += 1;
            self.metrics.bytes_out += self.scratch.len() as u64;
            self.metrics.fanout_copies += 1;
            sent += 1;
        }

        if sent == 0 {
            return Outcome::Dropped(DropReason::NoTargets);
        }
        Outcome::Forwarded { targets: sent }
    }

    /// 处理 Bind / 重绑定。
    fn handle_bind<S: DatagramSink>(
        &mut self,
        from: SocketAddr,
        datagram: &mut [u8],
        now_ms: u64,
        sink: &mut S,
    ) -> Outcome {
        // 反放大：未绑定的源地址共用一个限速器。合法客户端只需要发 1 个包。
        if !self.unbound_rate.allow(now_ms) {
            return Outcome::Dropped(DropReason::RateLimited);
        }
        let Ok(packet) = VoicePacket::parse(datagram, Direction::ClientToServer) else {
            return Outcome::Dropped(DropReason::Malformed);
        };
        let Ok(token) = wire::parse_bind(&packet) else {
            return Outcome::Dropped(DropReason::Malformed);
        };
        let Some(session_id) = self.by_token.get(&token).copied() else {
            // 未知 token：**一个字节都不回**。回应就等于把自己变成放大器。
            return Outcome::Dropped(DropReason::Unbound);
        };

        let old_addr = self.sessions.get(&session_id).and_then(|s| s.addr);
        if let Some(old) = old_addr {
            if old == from {
                // 重复 Bind（丢包重传）：幂等处理，只回 BindAck。
            } else {
                self.by_addr.remove(&old);
                self.metrics.rebinds += 1;
                self.emit(crate::Event::VoiceRebound {
                    session: session_id,
                    from: old,
                    to: from,
                });
            }
        } else {
            self.metrics.binds += 1;
            self.emit(crate::Event::VoiceBound {
                session: session_id,
                addr: from,
            });
        }
        self.by_addr.insert(from, session_id);
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.addr = Some(from);
        }

        let Some((key, sender_id)) = self
            .sessions
            .get(&session_id)
            .map(|s| (s.keys.s2c.clone(), s.sender_id))
        else {
            return Outcome::Dropped(DropReason::Unbound);
        };
        let Some(seq) = self.next_server_seq(session_id) else {
            return Outcome::Dropped(DropReason::Unbound);
        };
        let header = VoiceHeader {
            flags: flags::BIND,
            red_len: 0,
            channel_epoch: 0,
            sender_id: 0,
            seq,
            timestamp: 0,
            opus_len: BIND_ACK_PAYLOAD_LEN as u16,
        };
        let body = wire::bind_ack_payload(now_ms, sender_id);
        let mut buf = [0u8; HEADER_LEN + BIND_ACK_PAYLOAD_LEN + TAG_LEN];
        let Ok(len) = seal::seal_packet(&key, header, &body, &mut buf) else {
            return Outcome::Dropped(DropReason::Malformed);
        };
        sink.send(from, &buf[..len]);
        self.metrics.datagrams_out += 1;
        self.metrics.bytes_out += len as u64;
        Outcome::Replied
    }

    fn reply_probe<S: DatagramSink>(
        &mut self,
        session_id: SessionId,
        to: SocketAddr,
        ts_us: u64,
        sink: &mut S,
    ) -> Outcome {
        let Some(key) = self.sessions.get(&session_id).map(|s| s.keys.s2c.clone()) else {
            return Outcome::Dropped(DropReason::Unbound);
        };
        let Some(seq) = self.next_server_seq(session_id) else {
            return Outcome::Dropped(DropReason::Unbound);
        };
        let header = VoiceHeader {
            flags: flags::PROBE,
            red_len: 0,
            channel_epoch: 0,
            sender_id: 0,
            seq,
            timestamp: 0,
            opus_len: PROBE_PAYLOAD_LEN as u16,
        };
        let body = wire::probe_payload(ts_us);
        let mut buf = [0u8; HEADER_LEN + PROBE_PAYLOAD_LEN + TAG_LEN];
        let Ok(len) = seal::seal_packet(&key, header, &body, &mut buf) else {
            return Outcome::Dropped(DropReason::Malformed);
        };
        sink.send(to, &buf[..len]);
        self.metrics.datagrams_out += 1;
        self.metrics.bytes_out += len as u64;
        Outcome::Replied
    }
}
