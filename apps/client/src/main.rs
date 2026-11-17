//! 薄壳：解析参数 → 起运行时 → 跑。

// 命令行程序：输出就是它的功能（lib.rs 里的同一句不管这个 crate）。
#![allow(clippy::print_stdout)]

use std::process::ExitCode;

fn main() -> ExitCode {
    let config = match hushsquad_client::Config::parse(std::env::args().skip(1)) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("参数错误：{message}\n");
            eprintln!("{}", hushsquad_client::config::USAGE);
            return ExitCode::from(2);
        }
    };

    // 列设备不需要登录信息，所以在校验必填项之前处理。

    if config.list_devices {
        return match hushsquad_client::audio::devices() {
            Ok(text) => {
                print!("{text}");
                ExitCode::SUCCESS
            }
            Err(message) => {
                eprintln!("列设备失败：{message}");
                ExitCode::FAILURE
            }
        };
    }

    let config = config.with_env_password();
    if let Err(message) = config.validate() {
        eprintln!("参数错误：{message}\n");
        eprintln!("{}", hushsquad_client::config::USAGE);
        return ExitCode::from(2);
    }

    // 单线程运行时：客户端只有一条事件循环，网络与音频都不需要第二个线程
    // （音频回调由 cpal 自己起线程）。顺带免了把 opus 句柄跨线程搬。
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("起运行时失败：{e}");
            return ExitCode::FAILURE;
        }
    };

    // 汇总已经由 run 内部打印过，这里只看成败。
    match runtime.block_on(hushsquad_client::run(config)) {
        Ok(_) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("失败：{message}");
            ExitCode::FAILURE
        }
    }
}
