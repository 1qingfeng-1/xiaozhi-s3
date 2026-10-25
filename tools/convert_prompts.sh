#!/usr/bin/env bash
# 从 C++ 参考工程转码提示音 (Ogg Opus → 16kHz 单声道 s16le)
#
# 源目录默认为本机 xiaozhi-esp32 参考工程 (MIT)；换机器时传第一个参数:
#   tools/convert_prompts.sh /path/to/xiaozhi-esp32/main/assets/common
# 依赖: ffmpeg
set -euo pipefail

SRC="${1:-/home/mrpeng/Documents/xiaozhi-esp32/main/assets/common}"
SRC="${SRC%/}"
OUT="$(dirname "$0")/../assets"

# 源文件:目标文件 (低电/错误/提醒/成功)
CLIPS=(
  low_battery.ogg:low_battery.raw
  exclamation.ogg:exclamation.raw
  vibration.ogg:vibration.raw
  success.ogg:success.raw
)

for pair in "${CLIPS[@]}"; do
  src="${SRC}/${pair%%:*}"
  dst="${OUT}/${pair##*:}"
  ffmpeg -y -loglevel error -i "$src" \
    -f s16le -acodec pcm_s16le -ac 1 -ar 16000 "$dst"
  printf '%s ← %s (%s bytes)\n' "$dst" "$src" "$(stat -c %s "$dst")"
done
