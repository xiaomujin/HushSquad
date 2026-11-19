//! 薄壳：把窗口跑起来。所有逻辑在 `hushsquad_app` 里（方便将来加无头测试）。

// Windows 上**任何**构建都不要顺带弹出控制台窗口。
//
// 这里原来是 `cfg_attr(not(debug_assertions), …)`：release 干净，debug 留一个黑窗口
// 方便看 panic。实际用起来那个窗口纯属噪音——从资源管理器双击时它跟着开，没人会盯着
// 它看；真出问题时它早被别的窗口盖住了，而且它不属于「客户端」的一部分，关掉窗口后
// 用户还得再关它一次。
//
// 代价是 panic 没有地方落字，所以下面装了一个钩子写日志。从终端启动时 stderr 照常输出
// （GUI 子系统的进程会继承父进程的标准句柄，只是不**新建**控制台）；双击启动时至少
// 留一份日志。
#![windows_subsystem = "windows"]

fn main() {
    log_panics();
    hushsquad_app::run();
}

/// panic 写进 `%APPDATA%\HushSquad\app.log`，然后再交给原来的钩子。
///
/// 不用日志库：这里只需要「崩了能查到一行字」，多一个依赖不值。写失败就算了——
/// 崩溃路径上再抛一次异常只会把真正的原因盖掉。
fn log_panics() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        use std::io::Write as _;

        let where_ = info
            .location()
            .map_or_else(|| "<未知位置>".to_string(), ToString::to_string);
        // 只要 payload：`PanicHookInfo` 的 Display 里已经带了一遍「panicked at 位置」，
        // 再拼一次就成了一条读不懂的重复行。
        let what = info
            .payload_as_str()
            .map_or_else(|| "<非字符串的 panic>".to_string(), ToString::to_string);
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        if let Some(path) = log_path() {
            // 反复崩溃时别让它无限长：超过 1 MiB 就从头写。
            if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1 << 20) {
                let _ = std::fs::write(&path, "[日志超过 1 MiB，已清空]\n");
            }
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let _ = writeln!(file, "[{secs}] panic at {where_}: {what}");
            }
        }
        // 没有控制台之后，「窗口没出来」这件事就没法解释自己了：一个启动期 panic
        // （比如第二个实例抢不到 WebView2）表现为「双击了没反应」。所以给一个框。
        show_error_dialog(&format!(
            "{what}\n\n位置：{where_}\n日志：%APPDATA%\\HushSquad\\app.log"
        ));
        // 原来的钩子：从终端启动时这一行会出现在终端里。
        previous(info);
    }));
}

/// 弹一个错误框（Windows）。其它平台什么都不做——那边通常是从终端启动的，stderr 看得见。
#[cfg(windows)]
fn show_error_dialog(text: &str) {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(
            hwnd: *mut core::ffi::c_void,
            text: *const u16,
            caption: *const u16,
            kind: u32,
        ) -> i32;
    }
    /// NUL 结尾的宽字符串。
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    let (text, caption) = (wide(text), wide("HushSquad 出错了"));
    // SAFETY: 两个指针都指向在调用期间存活的 NUL 结尾字符串；`hwnd` 传空表示无父窗口。
    // 0x10 = MB_ICONERROR，0x0 = MB_OK。
    unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(), 0x10) };
}

#[cfg(not(windows))]
fn show_error_dialog(_text: &str) {}

/// `%APPDATA%\HushSquad\app.log`（与客户端的 `client.db` 放同一个目录）。
fn log_path() -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(std::env::var_os("APPDATA")?).join("HushSquad");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("app.log"))
}
