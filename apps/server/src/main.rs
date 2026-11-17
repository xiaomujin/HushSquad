//! HushSquad 服务器二进制入口。逻辑全在 `hushsquad_server` 库里。

fn main() -> std::process::ExitCode {
    hushsquad_server::run_cli()
}
