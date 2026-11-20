#!/usr/bin/env bash
# 组装 Linux 服务器发布包。
#
#   scripts/package-server.sh <二进制> <目标三元组> [版本] [输出目录]
#
# 例：
#   scripts/package-server.sh target/x86_64-unknown-linux-musl/release/hushsquad-server \
#       x86_64-unknown-linux-musl 0.0.0-g7c92625 dist
#
# 产物：dist/hushsquad-server-<版本>-<三元组>.tar.gz（+ .sha256）
# 包内：二进制、systemd unit、install.sh、部署.md —— 解包后 sudo ./install.sh 就能跑。

set -euo pipefail

BIN=${1:?用法: package-server.sh <二进制> <目标三元组> [版本] [输出目录]}
TRIPLE=${2:?用法: package-server.sh <二进制> <目标三元组> [版本] [输出目录]}
VERSION=${3:-0.0.0}
OUTDIR=${4:-dist}

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)

if [ ! -f "$BIN" ]; then
    echo "找不到二进制：$BIN" >&2
    exit 1
fi

NAME=hushsquad-server-$VERSION-$TRIPLE
STAGE=$OUTDIR/$NAME

rm -rf "$STAGE"
mkdir -p "$STAGE"

# 二进制：一律以 hushsquad-server 这个名字进包（unit 和 install.sh 都按这个名字找）
install -m755 "$BIN" "$STAGE/hushsquad-server"

# 剥符号：workspace 的 release profile 故意留 debug=1（本机做性能分析要用符号），
# 但发布包不需要 —— 41 MB 带符号的静态二进制，剥完 ~6 MB，装到服务器上也干净。
# 只在交叉目标是 ELF 且本机有 strip 时做，别在 Windows 上对着 PE 文件跑。
if command -v strip >/dev/null 2>&1 && file -b "$STAGE/hushsquad-server" 2>/dev/null | grep -q 'ELF'; then
    size_before=$(du -h "$STAGE/hushsquad-server" | cut -f1)
    strip -s "$STAGE/hushsquad-server"
    echo "已剥符号：$size_before -> $(du -h "$STAGE/hushsquad-server" | cut -f1)"
fi

install -m644 "$ROOT/deploy/hushsquad-server.service" "$STAGE/hushsquad-server.service"
install -m755 "$ROOT/deploy/install.sh" "$STAGE/install.sh"
install -m644 "$ROOT/docs/部署.md" "$STAGE/部署.md"

# 版本记录：不依赖 git 也能看出这个包是哪次构建的
{
    echo "version: $VERSION"
    echo "target:  $TRIPLE"
    echo "built:   $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
    echo "sha256:  $(sha256sum "$STAGE/hushsquad-server" | cut -d' ' -f1)"
} > "$STAGE/VERSION"

tar -C "$OUTDIR" -czf "$OUTDIR/$NAME.tar.gz" "$NAME"
rm -rf "$STAGE"

( cd "$OUTDIR" && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256" )

echo "打包完成："
echo "  $OUTDIR/$NAME.tar.gz"
echo "  $OUTDIR/$NAME.tar.gz.sha256"
tar -tzf "$OUTDIR/$NAME.tar.gz"
