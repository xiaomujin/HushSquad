#!/usr/bin/env bash
# 本地验证：把 install.sh 里「拼 unit」的那段单独跑一遍，看插入的行到底长什么样。
# 这一步值得单独验：续行符少一层，systemd 就会把 --voice-host 当成独立一行丢掉。
set -euo pipefail
cd "$(dirname "$0")/.."
HERE=deploy
BIN_NAME=hushsquad-server
VOICE_HOST=voice.example.com

unit_tmp=$(mktemp)
awk -v host="$VOICE_HOST" '
    /^[[:space:]]*--voice-bind[[:space:]]/ {
        print
        printf "    --voice-host %s \134\n", host
        next
    }
    { print }
' "$HERE/$BIN_NAME.service" > "$unit_tmp"

echo "--- ExecStart 段（cat -A 看真实字节，\$ 是行尾）---"
sed -n '/^ExecStart=/,/^$/p' "$unit_tmp" | cat -A
rm -f "$unit_tmp"
