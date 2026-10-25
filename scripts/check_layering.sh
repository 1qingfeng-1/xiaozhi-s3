#!/usr/bin/env bash
# 分层门禁: xiaozhi-core 必须保持硬件无关 (host 可测试)
#
# core 只能依赖纯逻辑 crate; 一旦引入 esp-hal / embassy / embedded-hal,
# host 测试与跨板复用都会被破坏。这里用 cargo tree 直接检查依赖图。
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

TREE=$(cargo tree --locked -p xiaozhi-core --edges normal --prefix none)
FORBIDDEN='^(esp-.+|esp_idf.+|embedded-hal.*|embassy-.+|embassy_net.*|smoltcp)$'

if echo "$TREE" | awk '{print $1}' | grep -Eq "$FORBIDDEN"; then
    echo "分层违规: xiaozhi-core 依赖了硬件/网络 crate:" >&2
    echo "$TREE" | awk '{print $1}' | grep -E "$FORBIDDEN" | sort -u >&2
    exit 1
fi

echo "core 依赖图 (硬件无关):"
echo "$TREE"
