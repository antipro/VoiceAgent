#!/usr/bin/env bash
# 从 macOS 交叉编译并部署到树莓派。
#
#   ./scripts/build-pi.sh <用户名>@raspberrypi.local
#
# 前置：rustup target add aarch64-unknown-linux-gnu && cargo install cross
#       （cross 需要 Docker 在运行）
#
# 重要：cross 的镜像基于较新的 glibc，产物无法在 Ubuntu 18.04 上运行。
#       目标系统请使用 64 位 Raspberry Pi OS Bookworm 或 Ubuntu 22.04+。

set -euo pipefail

HOST="${1:-}"
if [[ -z "$HOST" ]]; then
    echo "用法: $0 user@host" >&2
    exit 1
fi

TARGET=aarch64-unknown-linux-gnu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

command -v cross >/dev/null || { echo "请先安装 cross: cargo install cross"; exit 1; }

echo "==> 交叉编译 $TARGET"
(cd "$ROOT" && cross build --release --target "$TARGET")

BIN="$ROOT/target/$TARGET/release/xiaomeng"
[[ -f "$BIN" ]] || { echo "未找到产物 $BIN"; exit 1; }
ls -lh "$BIN"

echo "==> 传输到 $HOST"
scp "$BIN" "$HOST:/tmp/xiaomeng"
scp "$ROOT/deploy/xiaomeng.service" "$HOST:/tmp/xiaomeng.service"

echo "==> 在远端安装"
ssh "$HOST" 'sudo install -m 0755 /tmp/xiaomeng /opt/xiaomeng/xiaomeng && sudo systemctl restart xiaomeng && sleep 1 && sudo systemctl --no-pager --lines=20 status xiaomeng'

echo "完成。日志: ssh $HOST journalctl -u xiaomeng -f"
