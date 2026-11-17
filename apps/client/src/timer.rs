//! 定时器精度：Windows 上必须显式调到 1 ms。
//!
//! # 为什么这不是「差不多」而是「不能用」
//!
//! Windows 默认的定时器粒度是 **15.6 ms**（64 Hz）。一个 10 ms 的 `interval` 在这个粒度上
//! 实际会变成 15.6 ms 触发一次，也就是每秒只有 64 个 tick：
//!
//! - 上行每秒只发得出 64 帧 × 10 ms = 640 ms 的音频，剩下的 360 ms 是空的；
//! - 下行每秒少拉 36 帧，而网络仍按 100 帧/秒送到 —— 抖动缓冲于是**单调上涨**。
//!
//! 实测（本机环回，两个客户端，3 秒）：不调这个，上行 193 帧而不是 300 帧，
//! 接收端缓冲从 20 ms 目标涨到 95 ms。调了之后两者都对上了。
//!
//! # 为什么用 `timeBeginPeriod` 而不是别的
//!
//! - `thread::sleep` 与 tokio 的定时器**共用**同一个系统粒度，绕不过去；
//! - 自旋等待能到亚毫秒，但会烧掉一个核心；
//! - `timeBeginPeriod(1)` 是语音软件几十年来的做法（TS3/Discord/OBS 都调）。
//!   Windows 10 2004 之后它只影响**调用进程**，所以对整机功耗的影响是可控的。
//!
//! 退出时调 `timeEndPeriod` 复原；[`TimerResolution`] 用 Drop 保证这件事一定发生。

#[cfg(windows)]
mod imp {
    /// 把本进程的定时器精度提到 1 ms。返回是否成功。
    #[allow(unsafe_code)] // 唯一的 FFI 调用：无指针、无生命周期，失败只返回错误码
    pub fn begin() -> bool {
        // SAFETY: `timeBeginPeriod` 只设置进程级定时器精度，参数是 u32，
        // 返回值 0 表示成功。不存在内存安全问题。
        unsafe { windows::Win32::Media::timeBeginPeriod(1) == 0 }
    }

    /// 复原。
    #[allow(unsafe_code)] // 与 begin 对称
    pub fn end() {
        // SAFETY: 同上，`timeEndPeriod` 只是取消一次 begin。
        unsafe {
            let _ = windows::Win32::Media::timeEndPeriod(1);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    /// 其它平台的定时器本来就够细，不需要额外处理。
    pub fn begin() -> bool {
        true
    }

    /// 无事可做。
    pub fn end() {}
}

/// 定时器精度守卫：构造时提升到 1 ms，drop 时复原。
pub struct TimerResolution {
    raised: bool,
}

impl TimerResolution {
    /// 提升本进程的定时器精度。
    #[must_use]
    pub fn raise() -> Self {
        Self {
            raised: imp::begin(),
        }
    }

    /// 是否真的提升成功（失败时链路仍能跑，只是节奏会松）。
    #[must_use]
    pub const fn is_raised(&self) -> bool {
        self.raised
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        if self.raised {
            imp::end();
        }
    }
}
