#!/usr/bin/env bash
# 生成空闲提示音资源 (16kHz 单声道 s16le)
#
# 用法: tools/gen_prompt.sh "我先休息啦，叫我就行"
# 依赖: TTS 代理 (默认 192.168.123.14:8880) + ffmpeg
set -euo pipefail

TEXT="${1:-我先休息啦，叫我就行}"
PROXY="${TTS_PROXY:-http://192.168.123.14:8880/}"

curl -sS -m 20 -X POST "$PROXY" \
  -H 'Content-Type: application/json' \
  -d "{\"input\":\"$TEXT\"}" -o /tmp/idle_prompt.mp3
ffmpeg -y -loglevel error -i /tmp/idle_prompt.mp3 \
  -ar 16000 -ac 1 -f s16le assets/idle_prompt.raw

# 唤醒应答「我在」
curl -sS -m 20 -X POST "$PROXY" \
  -H 'Content-Type: application/json' \
  -d '{"input":"我在"}' -o /tmp/wake_ack.mp3
ffmpeg -y -loglevel error -i /tmp/wake_ack.mp3 \
  -ar 16000 -ac 1 -f s16le assets/wake_ack.raw

ls -l assets/idle_prompt.raw assets/wake_ack.raw
