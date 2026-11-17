# HushSquad 门禁检查。任何一条失败即退出非 0。
# 用法：pwsh -File scripts/check.ps1   或   powershell -ExecutionPolicy Bypass -File scripts/check.ps1
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

# 若当前 shell 没有 MSVC 环境（link.exe 不在 PATH），自动经 with-msvc.bat 重新执行一次。
if ($env:OS -eq "Windows_NT" -and -not (Get-Command link.exe -ErrorAction SilentlyContinue)) {
    Write-Host "==> 未检测到 MSVC 链接器，改用 scripts\with-msvc.bat 重新执行" -ForegroundColor Yellow
    & (Join-Path $PSScriptRoot "with-msvc.bat") powershell -NoProfile -ExecutionPolicy Bypass -File $PSCommandPath
    exit $LASTEXITCODE
}

Write-Host "==> cargo fmt --check" -ForegroundColor Cyan
cargo fmt --all -- --check
if ($LASTEXITCODE -ne 0) { Write-Host "FAIL: fmt" -ForegroundColor Red; exit 1 }

Write-Host "==> cargo clippy -D warnings" -ForegroundColor Cyan
cargo clippy --workspace --all-targets -- -D warnings
if ($LASTEXITCODE -ne 0) { Write-Host "FAIL: clippy" -ForegroundColor Red; exit 1 }

Write-Host "==> cargo test" -ForegroundColor Cyan
cargo test --workspace
if ($LASTEXITCODE -ne 0) { Write-Host "FAIL: test" -ForegroundColor Red; exit 1 }

Write-Host "==> 质量档 feature（denoise-dfn3）的 clippy + test" -ForegroundColor Cyan
# DFN3 默认不编译（模型 8.4 MB + ONNX 运行时），所以单独把它的门禁也跑一遍，
# 否则这块代码可以坏着进主干而没人发现。
cargo clippy -p audio-core --features denoise-dfn3 --all-targets -- -D warnings
if ($LASTEXITCODE -ne 0) { Write-Host "FAIL: clippy (denoise-dfn3)" -ForegroundColor Red; exit 1 }
cargo test -p audio-core --features denoise-dfn3
if ($LASTEXITCODE -ne 0) { Write-Host "FAIL: test (denoise-dfn3)" -ForegroundColor Red; exit 1 }

Write-Host "==> cargo deny check（许可证 / advisory / 来源 / 禁用 crate）" -ForegroundColor Cyan
cargo deny check
if ($LASTEXITCODE -ne 0) { Write-Host "FAIL: deny" -ForegroundColor Red; exit 1 }

Write-Host "==> 实时 crate 依赖断言：audio-core 不得含 tokio / axum / tower / hyper" -ForegroundColor Cyan
$tree = cargo tree -p audio-core --edges normal | Out-String
if ($tree -match "tokio|axum|tower|hyper") {
    Write-Host "FAIL: audio-core 引入了网络运行时依赖，实时链路必须保持纯净" -ForegroundColor Red
    exit 1
}

Write-Host "ALL CHECKS PASSED" -ForegroundColor Green
