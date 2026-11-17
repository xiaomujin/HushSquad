//! 网络模拟器（P4-1）：在离线工具里模拟丢包 / 抖动 / 乱序，**固定种子可复现**。
//!
//! 为什么先做它：弱网结论如果不可复现，后面所有「丢包 3% 还能听」的判断都不可信。所以这里
//! 用自带的 xorshift 而不是 `rand`——种子的推进过程完全确定，跨平台、跨版本逐位一致。
//!
//! 语义（刻意做得简单、可断言）：
//!
//! - **丢包**：以 `loss` 概率直接丢弃，不进在途队列；
//! - **抖动**：给每个包一个 `0..=jitter_ms` 的均匀延迟，投递时刻 = 到达时刻 + 延迟；
//! - **乱序**：以 `reorder` 概率把该包的延迟强制为 0，于是它会越过还在路上的包——这是真实网络里
//!   「后发先至」的最小模型，比「交换相邻包」更好断言（交换会让乱序率恰好等于配置值，反而失真）。
//!
//! `T` 是泛型：现在还没有语音包类型（P5 才有），用泛型可以让模拟器先独立测试，
//! 将来直接 `NetSim<VoicePacket>`。

/// 可复现的伪随机数（xorshift64*）。
#[derive(Debug, Clone)]
struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        // 种子为 0 会让 xorshift 卡死在 0，先打散并保证非零。
        Self {
            state: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
        }
    }

    fn next_u32(&mut self) -> u32 {
        self.state ^= self.state >> 12;
        self.state ^= self.state << 25;
        self.state ^= self.state >> 27;
        (self.state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u32
    }

    /// `[0, 1)` 均匀分布。
    fn unit(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 / 16_777_216.0
    }
}

/// 一条链路的网络条件。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NetSimConfig {
    /// 丢包概率（0.0–1.0）。
    pub loss: f32,
    /// 抖动上限（毫秒）：每个包的额外延迟在 `0..=jitter_ms` 内均匀分布。
    pub jitter_ms: u32,
    /// 乱序概率（0.0–1.0）：该概率下把包的延迟强制为 0。
    pub reorder: f32,
    /// 随机种子。同种子 + 同配置 + 同调用序列 ⇒ 逐位相同的投递时刻。
    pub seed: u64,
}

impl NetSimConfig {
    /// 理想链路（不丢、不抖、不乱）。
    #[must_use]
    pub fn ideal(seed: u64) -> Self {
        Self {
            loss: 0.0,
            jitter_ms: 0,
            reorder: 0.0,
            seed,
        }
    }
}

/// 模拟器本体。
#[derive(Debug)]
pub struct NetSim<T> {
    config: NetSimConfig,
    rng: Rng,
    /// 在途包：`(投递时刻 ms, 载荷)`，按入队顺序保存。
    in_flight: Vec<(u64, T)>,
    /// 统计：投递 / 丢弃，用来验证「实测丢包率 ≈ 配置值」。
    delivered: u64,
    dropped: u64,
}

impl<T> NetSim<T> {
    /// 按配置构造。
    #[must_use]
    pub fn new(config: NetSimConfig) -> Self {
        Self {
            rng: Rng::new(config.seed),
            config,
            in_flight: Vec::new(),
            delivered: 0,
            dropped: 0,
        }
    }

    /// 送入一个包，返回**此刻已到期**的全部投递（可能不止一个：抖动的包会积压后一起到）。
    ///
    /// 返回值按投递时刻排序；同一时刻保持入队顺序（稳定排序，便于断言）。
    pub fn step(&mut self, payload: T, now_ms: u64) -> Vec<(u64, T)> {
        if self.rng.unit() < self.config.loss {
            self.dropped += 1;
        } else {
            let mut delay = if self.config.jitter_ms == 0 {
                0
            } else {
                // 闭区间 [0, jitter_ms]。
                (self.rng.next_u32() % (self.config.jitter_ms + 1)) as u64
            };
            if self.config.reorder > 0.0 && self.rng.unit() < self.config.reorder {
                delay = 0;
            }
            self.in_flight.push((now_ms + delay, payload));
        }
        self.drain_due(now_ms)
    }

    /// 取走所有到期包（不送新包）。长跑收尾、或推进虚拟时钟时用。
    pub fn drain(&mut self, now_ms: u64) -> Vec<(u64, T)> {
        self.drain_due(now_ms)
    }

    fn drain_due(&mut self, now_ms: u64) -> Vec<(u64, T)> {
        if self.in_flight.is_empty() {
            return Vec::new();
        }
        let mut due: Vec<(u64, T)> = Vec::new();
        let mut index = 0;
        while index < self.in_flight.len() {
            if self.in_flight[index].0 <= now_ms {
                due.push(self.in_flight.remove(index));
            } else {
                index += 1;
            }
        }
        // 稳定排序：同一投递时刻保持入队顺序。
        due.sort_by_key(|(at, _)| *at);
        self.delivered += due.len() as u64;
        due
    }

    /// 当前在途包数（还没到期的）。
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// 累计投递数。
    #[must_use]
    pub fn delivered(&self) -> u64 {
        self.delivered
    }

    /// 累计丢弃数。
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// 实测丢包率（丢弃 ÷ 送入）。
    #[must_use]
    pub fn measured_loss(&self) -> f64 {
        let total = self.delivered + self.dropped;
        if total == 0 {
            0.0
        } else {
            self.dropped as f64 / total as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ideal_link_delivers_everything_immediately() {
        let mut sim = NetSim::new(NetSimConfig::ideal(7));
        for seq in 0..100_u32 {
            let out = sim.step(seq, 1_000);
            assert_eq!(out, vec![(1_000, seq)], "理想链路应当即时投递");
        }
        assert_eq!(sim.in_flight(), 0);
        assert_eq!(sim.delivered(), 100);
        assert_eq!(sim.dropped(), 0);
    }

    #[test]
    fn same_seed_gives_identical_schedules() {
        let config = NetSimConfig {
            loss: 0.1,
            jitter_ms: 20,
            reorder: 0.1,
            seed: 42,
        };
        let run = |seed| {
            let mut sim = NetSim::new(NetSimConfig { seed, ..config });
            let mut log = Vec::new();
            for seq in 0..500_u32 {
                for (at, payload) in sim.step(seq, u64::from(seq) * 10) {
                    log.push((at, payload));
                }
            }
            log
        };
        assert_eq!(run(42), run(42), "同种子必须逐位复现");
        assert_ne!(run(42), run(43), "不同种子应当不同");
    }

    #[test]
    fn measured_loss_matches_configuration() {
        // 10% 丢包，10 000 个包：允许 ±1.5 个百分点（统计涨落）。
        let mut sim = NetSim::new(NetSimConfig {
            loss: 0.1,
            jitter_ms: 0,
            reorder: 0.0,
            seed: 1_234,
        });
        for seq in 0..10_000_u32 {
            let _ = sim.step(seq, u64::from(seq));
        }
        let measured = sim.measured_loss();
        assert!(
            (measured - 0.1).abs() < 0.015,
            "实测丢包率 {measured:.4}，与配置的 0.1 差太多"
        );
        assert_eq!(sim.delivered() + sim.dropped(), 10_000, "包不许凭空消失");
    }

    #[test]
    fn jitter_never_exceeds_the_configured_bound() {
        let jitter = 20_u64;
        let mut sim = NetSim::new(NetSimConfig {
            loss: 0.0,
            jitter_ms: jitter as u32,
            reorder: 0.0,
            seed: 99,
        });
        for seq in 0..2_000_u32 {
            let now = u64::from(seq) * 10;
            for (at, payload) in sim.step(seq, now) {
                // 投递时刻 = 送入时刻 + 抖动，所以下界用「该包自己的送入时刻」，不是本次调用的 now
                // （积压的包会在后续调用里被取走，那时 at 已经早于 now）。
                let enqueued = u64::from(payload) * 10;
                assert!(at >= enqueued, "投递时刻早于送入时刻：{at} < {enqueued}");
                assert!(
                    at <= enqueued + jitter,
                    "抖动超过上限：{at} > {}",
                    enqueued + jitter
                );
            }
        }
        // 收尾把还在路上的取出来，同样要满足上界。
        for (at, _) in sim.drain(1_000_000) {
            assert!(at <= 1_000_000);
        }
        assert_eq!(sim.in_flight(), 0);
    }

    #[test]
    fn reorder_produces_out_of_order_delivery() {
        let mut sim = NetSim::new(NetSimConfig {
            loss: 0.0,
            jitter_ms: 50,
            reorder: 0.3,
            seed: 2026,
        });
        let mut delivery_order = Vec::new();
        for seq in 0..1_000_u32 {
            for (_, payload) in sim.step(seq, u64::from(seq) * 10) {
                delivery_order.push(payload);
            }
        }
        for (_, payload) in sim.drain(1_000_000) {
            delivery_order.push(payload);
        }
        let inversions = delivery_order
            .windows(2)
            .filter(|pair| pair[0] > pair[1])
            .count();
        assert!(inversions > 0, "配了 30% 乱序，却一个逆序都没有");
        // 乱序不该把包搞丢或搞重复。
        let mut sorted = delivery_order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..1_000).collect::<Vec<u32>>());
    }

    #[test]
    fn jittered_packets_accumulate_then_arrive() {
        // 抖动 > 帧间隔时，包会在途积压；把时钟推够，它们必须全部到齐。
        let mut sim = NetSim::new(NetSimConfig {
            loss: 0.0,
            jitter_ms: 100,
            reorder: 0.0,
            seed: 5,
        });
        let mut total = 0;
        for seq in 0..200_u32 {
            total += sim.step(seq, u64::from(seq) * 10).len();
        }
        assert!(sim.in_flight() > 0, "抖动 100 ms 时应当有包在途");
        total += sim.drain(1_000_000).len();
        assert_eq!(total, 200, "推进时钟后所有包都必须到齐");
    }
}
