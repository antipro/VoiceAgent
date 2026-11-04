#!/usr/bin/env bash
# 在树莓派（64 位 Raspberry Pi OS / Ubuntu 22.04+）上一键准备运行环境。
#
#   sudo ./deploy/install.sh              # 安装依赖、whisper.cpp、piper、模型、systemd
#   sudo ./deploy/install.sh --build      # 额外在本机执行 cargo build --release
#
# 可覆盖的变量：
#   SERVICE_USER=pi WHISPER_MODEL=ggml-small.bin sudo -E ./deploy/install.sh

set -euo pipefail

APP_DIR=/opt/xiaomeng
MODEL_DIR="$APP_DIR/models"
CONF_DIR=/etc/xiaomeng
DATA_DIR=/var/lib/xiaomeng
WHISPER_DIR=/opt/whisper.cpp
PIPER_DIR=/opt/piper

WHISPER_TAG="${WHISPER_TAG:-v1.9.5}"
PIPER_RELEASE="${PIPER_RELEASE:-2023.11.14-2}"
WHISPER_MODEL="${WHISPER_MODEL:-ggml-base.bin}"
PIPER_VOICE="zh_CN-huayan-medium"
VOICE_URL="https://huggingface.co/rhasspy/piper-voices/resolve/main/zh/zh_CN/huayan/medium"
MODEL_URL="https://huggingface.co/ggerganov/whisper.cpp/resolve/main"

BUILD=0
[[ "${1:-}" == "--build" ]] && BUILD=1

log() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!!\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31mxxx\033[0m %s\n' "$*" >&2; exit 1; }

[[ $EUID -eq 0 ]] || die "请用 sudo 运行"
SERVICE_USER="${SERVICE_USER:-${SUDO_USER:-root}}"
[[ "$SERVICE_USER" == "root" ]] && warn "将以 root 运行服务；建议 SERVICE_USER=pi 指定普通用户"

[[ "$(uname -m)" == "aarch64" ]] || warn "当前架构是 $(uname -m)，本脚本面向 aarch64"

log "安装系统依赖"
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
    build-essential cmake git curl ca-certificates pkg-config \
    libasound2-dev libgomp1 sox alsa-utils

log "创建目录"
install -d "$APP_DIR" "$MODEL_DIR" "$CONF_DIR" "$DATA_DIR"

# ---------------------------------------------------------------- whisper.cpp
if [[ ! -x /usr/local/bin/whisper-cli ]]; then
    log "编译 whisper.cpp ($WHISPER_TAG)"
    if [[ ! -d "$WHISPER_DIR/.git" ]]; then
        git clone --depth 1 --branch "$WHISPER_TAG" https://github.com/ggml-org/whisper.cpp "$WHISPER_DIR"
    fi
    # 静态链接，产物只有一个 whisper-cli，拷到 /usr/local/bin 即可运行。
    cmake -S "$WHISPER_DIR" -B "$WHISPER_DIR/build" \
        -DCMAKE_BUILD_TYPE=Release \
        -DBUILD_SHARED_LIBS=OFF \
        -DWHISPER_BUILD_TESTS=OFF \
        -DWHISPER_BUILD_SERVER=OFF \
        -DWHISPER_BUILD_EXAMPLES=ON
    cmake --build "$WHISPER_DIR/build" -j"$(nproc)"
    install -m 0755 "$WHISPER_DIR/build/bin/whisper-cli" /usr/local/bin/whisper-cli
else
    log "whisper-cli 已存在，跳过编译"
fi

# ---------------------------------------------------------------- 模型
if [[ ! -f "$MODEL_DIR/$WHISPER_MODEL" ]]; then
    log "下载 Whisper 模型 $WHISPER_MODEL（约 140MB）"
    curl -L --fail --retry 3 -o "$MODEL_DIR/$WHISPER_MODEL" "$MODEL_URL/$WHISPER_MODEL"
else
    log "Whisper 模型已存在"
fi

if [[ ! -x "$PIPER_DIR/piper" ]]; then
    log "安装 Piper ($PIPER_RELEASE)"
    curl -L --fail --retry 3 -o /tmp/piper.tar.gz \
        "https://github.com/rhasspy/piper/releases/download/${PIPER_RELEASE}/piper_linux_aarch64.tar.gz"
    tar -xzf /tmp/piper.tar.gz -C /opt
    rm -f /tmp/piper.tar.gz
fi
# 用包装脚本而不是软链：保证 $ORIGIN 指向 /opt/piper，运行库能被找到。
cat > /usr/local/bin/piper <<'WRAPPER'
#!/bin/sh
exec /opt/piper/piper "$@"
WRAPPER
chmod 0755 /usr/local/bin/piper

for ext in onnx onnx.json; do
    f="$MODEL_DIR/${PIPER_VOICE}.${ext}"
    if [[ ! -f "$f" ]]; then
        log "下载语音模型 ${PIPER_VOICE}.${ext}"
        curl -L --fail --retry 3 -o "$f" "$VOICE_URL/${PIPER_VOICE}.${ext}"
    fi
done

# ---------------------------------------------------------------- 二进制
if [[ $BUILD -eq 1 ]]; then
    command -v cargo >/dev/null || die "未安装 Rust，请先安装：curl https://sh.rustup.rs -sSf | sh"
    log "本机编译（树莓派 4 上大约需要 5~10 分钟）"
    (cd "$(dirname "$0")/.." && cargo build --release)
    install -m 0755 "$(dirname "$0")/../target/release/xiaomeng" "$APP_DIR/xiaomeng"
fi
if [[ ! -x "$APP_DIR/xiaomeng" ]]; then
    warn "未找到 $APP_DIR/xiaomeng，请先把二进制拷进来（见 README「部署」）"
fi

# ---------------------------------------------------------------- 配置
if [[ ! -f "$CONF_DIR/config.toml" ]]; then
    log "写入 $CONF_DIR/config.toml"
    cat > "$CONF_DIR/config.toml" <<EOF
[assistant]
name = "小蒙同学"
greeting = "我在"
active_timeout_secs = 10
system_prompt = """
你是运行在树莓派上的离线语音助手，名叫“小蒙同学”。
你通过语音与用户交流，所以回答必须口语化、简短（通常不超过 80 个字）。
不要使用 Markdown、代码块、列表、表情符号或括号注释，因为回答会被直接朗读出来。
需要实时资讯、天气、新闻、价格等你不确定的信息时，调用 web_search 工具再作答。
如果搜索没有给出可靠结果，就如实说明，不要编造。
"""

[wake]
enabled = true
words = ["小蒙同学", "小蒙", "小萌同学", "小萌", "小明同学"]

[audio]
backend = "cpal"
playback_volume = 1.0

[asr]
engine = "whisper"
command = "whisper-cli"
model = "$MODEL_DIR/$WHISPER_MODEL"
language = "zh"
threads = 4

[tts]
engine = "piper"
command = "piper"
model = "$MODEL_DIR/${PIPER_VOICE}.onnx"
length_scale = 0.95

[llm]
base_url = "https://api.deepseek.com"
model = "deepseek-chat"

[search]
provider = "tavily"

[storage]
path = "$DATA_DIR/assistant_sessions.db"
EOF
else
    log "保留已有配置 $CONF_DIR/config.toml"
fi

if [[ ! -f "$CONF_DIR/xiaomeng.env" ]]; then
    log "写入 $CONF_DIR/xiaomeng.env（请填入密钥）"
    cat > "$CONF_DIR/xiaomeng.env" <<EOF
DEEPSEEK_API_KEY=
TAVILY_API_KEY=
RUST_LOG=info
EOF
    chmod 600 "$CONF_DIR/xiaomeng.env"
else
    chmod 600 "$CONF_DIR/xiaomeng.env"
fi

chown -R "$SERVICE_USER":"$SERVICE_USER" "$DATA_DIR" "$APP_DIR"

# ---------------------------------------------------------------- systemd
log "安装 systemd 服务"
sed -e "s|__USER__|$SERVICE_USER|g" \
    -e "s|__APP_DIR__|$APP_DIR|g" \
    -e "s|__CONF_DIR__|$CONF_DIR|g" \
    "$(dirname "$0")/xiaomeng.service" > /etc/systemd/system/xiaomeng.service

systemctl daemon-reload
systemctl enable xiaomeng.service >/dev/null

log "完成。"
echo
echo "下一步："
echo "  1. sudo nano $CONF_DIR/xiaomeng.env   # 填入 DEEPSEEK_API_KEY / TAVILY_API_KEY"
echo "  2. speaker-test -c 2 -t wav           # 确认蓝牙音箱可用"
echo "  3. arecord -d 3 /tmp/t.wav && aplay /tmp/t.wav   # 确认麦克风"
echo "  4. sudo systemctl start xiaomeng && journalctl -u xiaomeng -f"
