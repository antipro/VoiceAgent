#!/usr/bin/env bash
# 从 macOS 构建树莓派（aarch64）二进制，并可选地部署过去。
#
#   ./scripts/build-pi.sh                      # 只构建
#   ./scripts/build-pi.sh <用户名>@<主机>       # 构建 + scp + 远程重启
#   ./scripts/build-pi.sh --cross <用户名>@<主机>  # 改用 cross 交叉编译（注意 glibc）
#
# 默认走 Docker：在 Ubuntu 22.04 / arm64 容器里原生编译，
# 产物的 glibc 与目标系统一致，不会出现 GLIBC_2.36 not found。
# 需要 Docker Desktop 正在运行。
#
# 产物：target/linux-arm64/release/xiaomeng

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="xiaomeng-builder:22.04"
TARGET_DIR="$ROOT/target/linux-arm64"
BIN="$TARGET_DIR/release/xiaomeng"

USE_CROSS=0
HOST=""

for arg in "$@"; do
    case "$arg" in
        --cross) USE_CROSS=1 ;;
        -h | --help)
            sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            if [[ -n "$HOST" ]]; then
                echo "多余的参数: $arg" >&2
                exit 2
            fi
            HOST="$arg"
            ;;
    esac
done

build_docker() {
    command -v docker >/dev/null || {
        echo "需要 Docker（macOS 上装 Docker Desktop）。" >&2
        echo "不想用 Docker 的话，直接在树莓派上编译：sudo ./deploy/install.sh --build" >&2
        exit 1
    }
    docker info >/dev/null 2>&1 || {
        echo "Docker 没在运行，先启动 Docker Desktop。" >&2
        exit 1
    }

    if [[ "$(uname -s)" == "Darwin" && "$(uname -m)" != "arm64" ]]; then
        echo "提示：Intel Mac 上会用 QEMU 模拟 arm64，编译很慢；建议在树莓派上编译。" >&2
    fi

    echo "==> 准备构建镜像（首次会下载 Ubuntu 22.04 + Rust，约 1GB）"
    docker build --platform linux/arm64 -t "$IMAGE" -f "$ROOT/deploy/Dockerfile.build" "$ROOT/deploy"

    echo "==> 在 Ubuntu 22.04 / arm64 容器里编译"
    docker run --rm \
        --platform linux/arm64 \
        -v "$ROOT:/app" \
        -v xiaomeng-cargo-registry:/root/.cargo/registry \
        -e CARGO_TARGET_DIR=/app/target/linux-arm64 \
        -w /app \
        "$IMAGE" cargo build --release
}

build_cross() {
    command -v cross >/dev/null || {
        echo "请先安装 cross: cargo install cross" >&2
        exit 1
    }
    echo "==> cross 交叉编译（注意：镜像 glibc 必须 <= 目标系统的 glibc）"
    (cd "$ROOT" && cross build --release --target aarch64-unknown-linux-gnu)
    mkdir -p "$TARGET_DIR"
    cp "$ROOT/target/aarch64-unknown-linux-gnu/release/xiaomeng" "$BIN"
}

if [[ $USE_CROSS -eq 1 ]]; then
    build_cross
else
    build_docker
fi

[[ -f "$BIN" ]] || {
    echo "没有找到产物 $BIN" >&2
    exit 1
}

echo
file "$BIN"
ls -lh "$BIN"

if [[ -z "$HOST" ]]; then
    echo
    echo "只构建，没有部署。部署示例："
    echo "  $0 <用户名>@<树莓派地址>"
    exit 0
fi

echo "==> 传输到 $HOST"
scp "$BIN" "$HOST:/tmp/xiaomeng"

echo "==> 远端安装并重启"
ssh "$HOST" 'sudo install -m 0755 /tmp/xiaomeng /opt/xiaomeng/xiaomeng && sudo systemctl restart xiaomeng && sleep 1 && sudo systemctl --no-pager --lines=15 status xiaomeng'

echo "完成。日志: ssh $HOST journalctl -u xiaomeng -f"
