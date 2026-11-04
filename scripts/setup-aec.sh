#!/usr/bin/env bash
# 在 Ubuntu 22.04 上启用 PipeWire 的 WebRTC 回声消除，并把虚拟设备设为默认。
#
#   ./scripts/setup-aec.sh            # 安装配置 + 重启 PipeWire + 设默认 + 校验
#   ./scripts/setup-aec.sh --check    # 只检查当前状态
#   ./scripts/setup-aec.sh --off      # 移除配置
#
# 重要：0.3.48 的 echo-cancel 模块只连接「当前默认」的设备。
# 请先连好蓝牙耳机（确认有麦克风），再运行本脚本。

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CONF_SRC="${XIAOMENG_AEC_CONF:-$ROOT/deploy/pipewire/51-xiaomeng-aec.conf}"
CONF_DST=/etc/pipewire/pipewire.conf.d/51-xiaomeng-aec.conf

MODE="install"
case "${1:-}" in
    --check | -c) MODE="check" ;;
    --off) MODE="off" ;;
    --help | -h)
        sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
esac

if [[ $EUID -eq 0 ]]; then
    RUN_USER="${SUDO_USER:-root}"
else
    RUN_USER="$(id -un)"
fi
RUN_UID="$(id -u "$RUN_USER")"
export XDG_RUNTIME_DIR="/run/user/$RUN_UID"

as_user() {
    if [[ $EUID -eq 0 && "$RUN_USER" != "root" ]]; then
        sudo -u "$RUN_USER" XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" "$@"
    else
        "$@"
    fi
}

need_cmd() {
    command -v "$1" >/dev/null || {
        echo "缺少命令 $1，请先安装：sudo apt install $2" >&2
        exit 1
    }
}

check() {
    need_cmd pactl pulseaudio-utils
    need_cmd pw-cli pipewire-bin

    echo "== PipeWire 状态 =="
    as_user pactl info 2>/dev/null | grep -E "Server Name|Server Version|Default Source|Default Sink" || {
        echo "pactl 连不上音频服务。确认已安装 pipewire-pulse 并启用：" >&2
        echo "  systemctl --user enable --now pipewire pipewire-pulse wireplumber" >&2
        exit 1
    }

    echo
    echo "== AEC 节点 =="
    if as_user pw-cli ls Node 2>/dev/null | grep -q "echo-cancel-source"; then
        echo "✔ echo-cancel-source / echo-cancel-sink 已加载"
    else
        echo "✘ 没有找到 echo-cancel 节点" >&2
        echo "  看日志：journalctl --user -u pipewire -n 50 --no-pager" >&2
        echo "  常见原因：蓝牙耳机没连、模块在默认设备就绪前加载、library.name 指向了不存在的引擎。" >&2
        return 1
    fi

    local def_src def_sink
    def_src="$(as_user pactl get-default-source 2>/dev/null || true)"
    def_sink="$(as_user pactl get-default-sink 2>/dev/null || true)"
    echo
    echo "默认输入: ${def_src:-<unknown>}"
    echo "默认输出: ${def_sink:-<unknown>}"

    if [[ "$def_src" == *echo-cancel* && "$def_sink" == *echo-cancel* ]]; then
        echo "✔ 默认设备已经走 AEC"
    else
        echo "✘ 默认设备没有全部走 AEC（应用会读到未处理的麦克风）" >&2
        return 1
    fi

    echo
    echo "== 应用侧建议 =="
    echo "config.toml 里两个设备都指向 PipeWire 的 ALSA PCM："
    echo "  [audio]"
    echo '  input_device  = "pipewire"'
    echo '  output_device = "pipewire"'
    echo
    echo "注意：播放也必须走 PipeWire 默认输出（echo-cancel-sink），"
    echo "否则 AEC 拿不到参考信号，等于没开。"
}

case "$MODE" in
    check)
        check
        ;;
    off)
        echo "移除 AEC 配置"
        sudo rm -f "$CONF_DST"
        as_user systemctl --user restart pipewire pipewire-pulse wireplumber || true
        echo "已移除。"
        ;;
    install)
        need_cmd pactl pulseaudio-utils
        need_cmd pw-cli pipewire-bin

        if [[ -f "$CONF_SRC" ]]; then
            echo "== 写入 $CONF_DST =="
            sudo install -Dm644 "$CONF_SRC" "$CONF_DST"
        elif [[ -f "$CONF_DST" ]]; then
            echo "== 使用已安装的 $CONF_DST =="
        else
            echo "找不到 AEC 配置（$CONF_SRC 与 $CONF_DST 都不存在）" >&2
            exit 1
        fi

        echo "== 重启 PipeWire（用户会话：$RUN_USER）=="
        as_user systemctl --user restart pipewire pipewire-pulse wireplumber
        sleep 1

        echo "== 设为默认设备 =="
        as_user pactl set-default-source echo-cancel-source || true
        as_user pactl set-default-sink echo-cancel-sink || true

        echo
        check || {
            echo
            echo "还没成功。请确认蓝牙耳机已连接并切换到 HFP（有麦克风）模式，然后重跑："
            echo "  $0"
            exit 1
        }
        ;;
esac
