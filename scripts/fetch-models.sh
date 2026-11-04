#!/usr/bin/env bash
# 下载开发/运行所需的模型到 ./models（Mac 或树莓派都能用）。
#
#   ./scripts/fetch-models.sh                 # base + huayan-medium
#   WHISPER_MODEL=ggml-tiny.bin ./scripts/fetch-models.sh

set -euo pipefail

MODEL_DIR="$(cd "$(dirname "$0")/.." && pwd)/models"
WHISPER_MODEL="${WHISPER_MODEL:-ggml-base.bin}"
PIPER_VOICE="zh_CN-huayan-medium"

mkdir -p "$MODEL_DIR"

fetch() {
    local url="$1" dest="$2"
    if [[ -f "$dest" ]]; then
        echo "已存在: $dest"
        return
    fi
    echo "下载: $url"
    curl -L --fail --retry 3 --progress-bar -o "$dest.part" "$url"
    mv "$dest.part" "$dest"
}

fetch "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/$WHISPER_MODEL" \
    "$MODEL_DIR/$WHISPER_MODEL"

for ext in onnx onnx.json; do
    fetch "https://huggingface.co/rhasspy/piper-voices/resolve/main/zh/zh_CN/huayan/medium/${PIPER_VOICE}.${ext}" \
        "$MODEL_DIR/${PIPER_VOICE}.${ext}"
done

echo
echo "完成，模型位于 $MODEL_DIR"
ls -lh "$MODEL_DIR"
