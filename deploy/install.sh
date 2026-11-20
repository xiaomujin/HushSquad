#!/usr/bin/env bash
# HushSquad 服务器安装脚本（Linux）。
#
# 做的事：建系统用户 → 装二进制 → 装 systemd unit → 建数据目录 → 启动。
# 账号库与自签证书都在第一次启动时自动生成，不需要准备任何文件。
#
#   sudo ./install.sh
#
# 参数：
#   --voice-host <主机>  可选。默认**不宣告**语音地址：客户端会沿用自己连控制面时用的那个
#                        地址，云主机（网卡是内网 IP、公网靠 NAT）与局域网都自动正确。
#                        只有语音面在另一台机器 / 另一个对外 IP 时才需要给。
#   --data <目录>        数据目录（账号库 + 证书），默认 /var/lib/hushsquad
#   --prefix <目录>      安装前缀，默认 /usr/local
#   --no-start           只安装，不启动服务
#   -h, --help           看这段说明

set -euo pipefail

BIN_NAME=hushsquad-server
SVC_USER=hushsquad
PREFIX=/usr/local
DATA_DIR=/var/lib/hushsquad
VOICE_HOST=
START=1
HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

usage() {
    # 打印文件开头的注释块：从第 2 行到第一个空行为止（加参数不会把 help 打歪）。
    sed -n '2,/^$/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --voice-host) VOICE_HOST=${2:?--voice-host 需要取值}; shift 2 ;;
        --data)       DATA_DIR=${2:?--data 需要取值}; shift 2 ;;
        --prefix)     PREFIX=${2:?--prefix 需要取值}; shift 2 ;;
        --no-start)   START=0; shift ;;
        -h|--help)    usage; exit 0 ;;
        *)            echo "未知参数：$1" >&2; usage >&2; exit 2 ;;
    esac
done

if [ "$(id -u)" -ne 0 ]; then
    echo "需要 root：sudo ./install.sh --voice-host <地址>" >&2
    exit 1
fi

if [ ! -f "$HERE/$BIN_NAME" ]; then
    echo "包不完整：找不到 $HERE/$BIN_NAME" >&2
    exit 1
fi

# 1. 系统用户（无登录 shell，无家目录以外的权限）
if ! id -u "$SVC_USER" >/dev/null 2>&1; then
    useradd --system --home-dir "$DATA_DIR" --shell /usr/sbin/nologin "$SVC_USER"
    echo "已创建系统用户 $SVC_USER"
fi

# 2. 二进制
install -Dm755 "$HERE/$BIN_NAME" "$PREFIX/bin/$BIN_NAME"

# 3. 文档（unit 里的 Documentation= 指向它）
if [ -f "$HERE/部署.md" ]; then
    install -Dm644 "$HERE/部署.md" "$PREFIX/share/doc/hushsquad/部署.md"
fi

# 4. 数据目录（账号库、证书都在这里；只有它能被服务写）
install -d -o "$SVC_USER" -g "$SVC_USER" -m 0750 "$DATA_DIR"

# 5. 冒烟：二进制真能在这台机器上跑起来（静态 musl 正常不会缺库，但别假设）
if ! "$PREFIX/bin/$BIN_NAME" --help >/dev/null 2>&1; then
    echo "警告：$PREFIX/bin/$BIN_NAME --help 执行失败，检查这个包是不是给本机架构的" >&2
fi

# 6. systemd unit（--voice-host 给了才拼进去；不给就是默认的「跟随客户端」）
if command -v systemctl >/dev/null 2>&1; then
    unit_tmp=$(mktemp)
    if [ -n "$VOICE_HOST" ]; then
        # 续行符用八进制 \134 在 awk 里生成：直接写反斜杠的话，
        # bash 单引号 → awk 字符串 → 输出 三处转义容易少一层，少一层 unit 就断了。
        awk -v host="$VOICE_HOST" '
            /^[[:space:]]*--voice-bind[[:space:]]/ {
                print
                printf "    --voice-host %s \134\n", host
                next
            }
            { print }
        ' "$HERE/$BIN_NAME.service" > "$unit_tmp"
        echo "语音面地址：宣告 $VOICE_HOST（--voice-host 覆盖）"
    else
        cp "$HERE/$BIN_NAME.service" "$unit_tmp"
        echo "语音面地址：跟随客户端连接地址（没有给 --voice-host，这是默认）"
    fi
    install -Dm644 "$unit_tmp" "/etc/systemd/system/$BIN_NAME.service"
    rm -f "$unit_tmp"
    systemctl daemon-reload
    systemctl enable "$BIN_NAME" >/dev/null

    if [ "$START" -eq 1 ]; then
        systemctl restart "$BIN_NAME"
        sleep 1
        systemctl --no-pager --lines=12 status "$BIN_NAME" || true
    fi
else
    echo "警告：没有 systemd，只装了二进制，没有装服务。手动跑：" >&2
    echo "  $PREFIX/bin/$BIN_NAME --db $DATA_DIR/hushsquad.db" >&2
fi

cat <<EOF

装好了。下一步：

  1. 加账号（会问两遍密码，不回显）：
       sudo -u $SVC_USER $PREFIX/bin/$BIN_NAME --db $DATA_DIR/hushsquad.db adduser 你的名字

  2. 放行端口（控制面 TCP / 语音面 UDP，两个都要）：
       ufw allow 64738/tcp && ufw allow 64739/udp        # Debian/Ubuntu
       firewall-cmd --add-port=64738/tcp --add-port=64739/udp --permanent && firewall-cmd --reload

  3. 看证书指纹（客户端第一次连会自动记住它，TOFU）：
       sudo -u $SVC_USER $PREFIX/bin/$BIN_NAME --db $DATA_DIR/hushsquad.db cert

  4. 日志：journalctl -u $BIN_NAME -f
     只有两类行：metrics {...} 与 event {...}

  客户端：hushsquad-client --server <服务器地址> --user 你的名字
  详细说明：$PREFIX/share/doc/hushsquad/部署.md
EOF
