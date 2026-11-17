//! 每个 sender 一个 **64 包滑动窗口**，防重放（协议草案 §3）。
//!
//! 窗口宽度 64 是协议写死的：`seq` 在 `key_epoch` 内单调递增，
//! 而 `key_epoch` 每 2^28 包或 1 小时换一次，所以窗口内不会回绕。
//! 实现仍按 `wrapping_sub as i32` 比较，把「跨 2^32 回绕」当成正常差值处理 ——
//! 这样即便将来换了轮换策略也不会悄悄失效。
//!
//! ## 位图语义
//!
//! `seen` 的第 i 位表示「`highest - i` 这个序号已经收过」，第 0 位恒为 1。

/// 一个序号的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 首次收到，已计入窗口。
    Fresh,
    /// 窗口内重复。
    Duplicate,
    /// 比窗口最老的一条还老，无法判断是否重复 —— 按重放处理（保守）。
    TooOld,
}

/// 计数器，进 P5-6 的丢弃原因分布。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReplayStats {
    /// 接受。
    pub accepted: u64,
    /// 窗口内重复。
    pub duplicate: u64,
    /// 太老。
    pub too_old: u64,
}

/// 64 包滑动窗口。
#[derive(Debug, Default, Clone)]
pub struct ReplayWindow {
    highest: u32,
    seen: u64,
    started: bool,
    stats: ReplayStats,
}

impl ReplayWindow {
    /// 窗口宽度。
    pub const WIDTH: u32 = 64;

    /// 新建（尚未收到任何包）。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 判定并更新。**只有返回 `Fresh` 时才允许继续处理这个包。**
    pub fn accept(&mut self, seq: u32) -> Verdict {
        if !self.started {
            self.started = true;
            self.highest = seq;
            self.seen = 1;
            self.stats.accepted += 1;
            return Verdict::Fresh;
        }

        let delta = seq.wrapping_sub(self.highest) as i32;
        match delta.cmp(&0) {
            core::cmp::Ordering::Greater => {
                let shift = delta as u32;
                // 前进超过一整个窗口时，窗口里没有可复用的历史，直接重置。
                self.seen = if shift >= Self::WIDTH {
                    1
                } else {
                    (self.seen << shift) | 1
                };
                self.highest = seq;
                self.stats.accepted += 1;
                Verdict::Fresh
            }
            core::cmp::Ordering::Equal => {
                self.stats.duplicate += 1;
                Verdict::Duplicate
            }
            core::cmp::Ordering::Less => {
                let offset = delta.unsigned_abs();
                if offset >= Self::WIDTH {
                    self.stats.too_old += 1;
                    return Verdict::TooOld;
                }
                let mask = 1u64 << offset;
                if self.seen & mask != 0 {
                    self.stats.duplicate += 1;
                    Verdict::Duplicate
                } else {
                    self.seen |= mask;
                    self.stats.accepted += 1;
                    Verdict::Fresh
                }
            }
        }
    }

    /// 当前窗口上沿（收到的最大序号）。
    #[must_use]
    pub const fn highest(&self) -> Option<u32> {
        if self.started {
            Some(self.highest)
        } else {
            None
        }
    }

    /// 计数器快照。
    #[must_use]
    pub const fn stats(&self) -> ReplayStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_order_is_all_fresh() {
        let mut w = ReplayWindow::new();
        for seq in 0..100 {
            assert_eq!(w.accept(seq), Verdict::Fresh);
        }
        assert_eq!(w.stats().accepted, 100);
        assert_eq!(w.highest(), Some(99));
    }

    #[test]
    fn immediate_repeat_is_duplicate() {
        let mut w = ReplayWindow::new();
        assert_eq!(w.accept(7), Verdict::Fresh);
        assert_eq!(w.accept(7), Verdict::Duplicate);
        assert_eq!(w.accept(7), Verdict::Duplicate);
        assert_eq!(w.stats().duplicate, 2);
    }

    #[test]
    fn out_of_order_inside_the_window_is_fresh_once() {
        let mut w = ReplayWindow::new();
        for seq in [10, 12, 11, 13] {
            assert_eq!(w.accept(seq), Verdict::Fresh, "seq {seq}");
        }
        assert_eq!(w.accept(11), Verdict::Duplicate);
    }

    #[test]
    fn the_window_is_exactly_64_wide() {
        let mut w = ReplayWindow::new();
        assert_eq!(w.accept(1000), Verdict::Fresh);
        // 往回 63 个还在窗口内。
        assert_eq!(w.accept(1000 - 63), Verdict::Fresh);
        // 往回 64 个刚好出界。
        assert_eq!(w.accept(1000 - 64), Verdict::TooOld);
        // 但 64 号位之后，再来一次 1000-63 就是重复了。
        assert_eq!(w.accept(1000 - 63), Verdict::Duplicate);
    }

    #[test]
    fn a_big_jump_resets_the_window() {
        let mut w = ReplayWindow::new();
        assert_eq!(w.accept(1), Verdict::Fresh);
        assert_eq!(w.accept(10_000), Verdict::Fresh);
        // 老的序号现在全部出界。
        assert_eq!(w.accept(2), Verdict::TooOld);
        // 但新窗口内的序号仍然可用。
        assert_eq!(w.accept(9_999), Verdict::Fresh);
    }

    #[test]
    fn wraparound_is_handled() {
        let mut w = ReplayWindow::new();
        assert_eq!(w.accept(u32::MAX - 1), Verdict::Fresh);
        assert_eq!(w.accept(u32::MAX), Verdict::Fresh);
        assert_eq!(w.accept(0), Verdict::Fresh);
        assert_eq!(w.accept(1), Verdict::Fresh);
        assert_eq!(w.accept(0), Verdict::Duplicate);
        // 回绕后 u32::MAX 在窗口内且**已经收过** —— 正是这个窗口该拦住的重放。
        assert_eq!(w.accept(u32::MAX), Verdict::Duplicate);
        // 而一个从没见过的、回绕之前的序号仍然是新的。
        assert_eq!(w.accept(u32::MAX - 5), Verdict::Fresh);
        assert_eq!(w.accept(u32::MAX - 5), Verdict::Duplicate);
    }

    #[test]
    fn no_sequence_is_ever_accepted_twice() {
        // 用确定性的伪随机顺序轰炸窗口：同一个 seq 绝不能被接受两次。
        let mut w = ReplayWindow::new();
        let mut accepted = std::collections::HashSet::new();
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..20_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let seq = (state % 400) as u32;
            if w.accept(seq) == Verdict::Fresh {
                assert!(accepted.insert(seq), "seq {seq} 被接受了两次");
            }
        }
        assert!(
            accepted.len() > 60,
            "测试本身没覆盖到窗口，只接受了 {}",
            accepted.len()
        );
    }
}
