//! 分配计数：证明热路径零分配（P1-4 的门禁）。
//!
//! 用一个包在系统分配器外面的 [`GlobalAlloc`] 计数器，在「每帧处理」循环前后取差值。
//! 这条比任何代码评审都有效：`Vec::push` 扩容、`format!`、`Box::new` 只要混进热路径，差值立刻非 0。
//!
//! 计数是**线程本地**的：`cargo test` 会并行跑测试，全局计数会被别的线程污染（甚至算出负数）。
//! 线程本地之后，「这个线程在这次循环里分配了几次」才是可断言的事实。

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

/// 统计分配次数的全局分配器（按线程分别计数）。
pub struct CountingAlloc;

/// 自增当前线程的计数。线程销毁期间的分配（TLS 已失效）直接忽略，不 panic、不重入。
fn bump() {
    let _ = ALLOCATIONS.try_with(|cell| cell.set(cell.get() + 1));
}

// SAFETY: 四个方法都只是把调用原样转发给 `System`，参数与契约与 `GlobalAlloc` 完全一致；
// 额外动作只有一次线程本地自增，不改变内存布局、不分配、不改变生命周期。
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump();
        // SAFETY: 契约由调用方（分配器协议）保证，这里原样转发。
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: 契约由调用方保证，这里原样转发。
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        bump();
        // SAFETY: 契约由调用方保证，这里原样转发。
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump();
        // SAFETY: 契约由调用方保证，这里原样转发。
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// 当前线程累计分配次数（含 realloc / alloc_zeroed）。
#[must_use]
pub fn count() -> usize {
    ALLOCATIONS.try_with(Cell::get).unwrap_or(0)
}
