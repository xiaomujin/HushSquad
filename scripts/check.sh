#!/usr/bin/env bash
# HushSquad 门禁检查。任何一条失败即退出非 0。
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> cargo fmt --check"
cargo fmt --all -- --check

echo "==> cargo clippy -D warnings"
cargo clippy --workspace --all-targets -- -D warnings

echo "==> cargo test"
cargo test --workspace

echo "==> cargo deny check"
cargo deny check

echo "==> 实时 crate 依赖断言：audio-core 不得含 tokio / axum / tower"
if cargo tree -p audio-core --edges normal | grep -Eq "tokio|axum|tower|hyper"; then
  echo "FAIL: audio-core 引入了网络运行时依赖" >&2
  exit 1
fi

echo "ALL CHECKS PASSED"
