# 桌面外壳（apps/app）的门禁。
#
# 为什么不并进 scripts/check.ps1：见 docs/ADR/ADR-0014-桌面外壳与门禁的边界.md。
# 一句话——Tauri 的依赖树与系统库不该绑架音频门禁。这个脚本要手动跑。
#
# 用法：pwsh -File scripts/check-app.ps1

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

if ($env:OS -eq "Windows_NT" -and -not (Get-Command link.exe -ErrorAction SilentlyContinue)) {
    Write-Host "==> 未检测到 MSVC 链接器，改用 scripts\with-msvc.bat 重新执行" -ForegroundColor Yellow
    & (Join-Path $PSScriptRoot "with-msvc.bat") powershell -NoProfile -ExecutionPolicy Bypass -File $PSCommandPath
    exit $LASTEXITCODE
}

# 前端必须先构建：`frontendDist` 是编译期内嵌的，顺序反了窗口里就是上一版资源。
Write-Host "==> 前端：pnpm check + build" -ForegroundColor Cyan
Push-Location ui
try {
    pnpm check
    if ($LASTEXITCODE -ne 0) { Write-Host "FAIL: pnpm check" -ForegroundColor Red; exit 1 }
    pnpm build
    if ($LASTEXITCODE -ne 0) { Write-Host "FAIL: pnpm build" -ForegroundColor Red; exit 1 }
} finally {
    Pop-Location
}

Write-Host "==> 外壳：cargo fmt --check" -ForegroundColor Cyan
cargo fmt --manifest-path apps/app/Cargo.toml --all -- --check
if ($LASTEXITCODE -ne 0) { Write-Host "FAIL: fmt (app)" -ForegroundColor Red; exit 1 }

Write-Host "==> 外壳：cargo clippy -D warnings" -ForegroundColor Cyan
cargo clippy --manifest-path apps/app/Cargo.toml --all-targets -- -D warnings
if ($LASTEXITCODE -ne 0) { Write-Host "FAIL: clippy (app)" -ForegroundColor Red; exit 1 }

Write-Host "==> 外壳：cargo build" -ForegroundColor Cyan
cargo build --manifest-path apps/app/Cargo.toml
if ($LASTEXITCODE -ne 0) { Write-Host "FAIL: build (app)" -ForegroundColor Red; exit 1 }

Write-Host "APP CHECKS PASSED" -ForegroundColor Green
