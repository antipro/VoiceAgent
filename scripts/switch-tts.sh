#!/usr/bin/env bash
# 切换语音引擎（改了配置后重启进程生效）。
#
#   ./switch-tts.sh edge    # 在线，音质好，断网自动回退 piper
#   ./switch-tts.sh piper   # 离线，快，有口音
set -euo pipefail

ENGINE="${1:-}"
case "$ENGINE" in
    edge | piper) ;;
    *)
        echo "用法: $0 edge|piper" >&2
        exit 1
        ;;
esac

CONF="${XIAOMENG_CONFIG:-$HOME/xiaomeng/config.toml}"
[[ -f "$CONF" ]] || {
    echo "找不到配置: $CONF" >&2
    exit 1
}

python3 - "$CONF" "$ENGINE" <<'PY'
import re
import sys

path, engine = sys.argv[1], sys.argv[2]
text = open(path).read()
new, count = re.subn(
    r'(\[tts\][^\[]*?engine\s*=\s*")[^"]+(")',
    r'\g<1>' + engine + r'\g<2>',
    text,
    count=1,
    flags=re.S,
)
if count != 1:
    raise SystemExit("没有在 [tts] 下找到 engine")
open(path, "w").write(new)
print(f"tts.engine = {engine}")
PY

if systemctl --user is-active --quiet xiaomeng 2>/dev/null; then
    systemctl --user restart xiaomeng
    echo "用户服务已重启"
else
    echo "服务未运行：当前进程重启后生效"
fi
