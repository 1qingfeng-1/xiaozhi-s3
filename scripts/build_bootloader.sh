#!/usr/bin/env bash
# 构建启用 OTA 自动回滚的二级引导器，产物交给 espflash.toml 注入。
#
# 依赖: ESP-IDF >= 5.4 (默认读取 $IDF_PATH, 未设置时用本机 5.5.2)
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
IDF_PATH="${IDF_PATH:-/home/mrpeng/esp/v5.5.2/esp-idf}"

if [ ! -f "$IDF_PATH/export.sh" ]; then
    echo "找不到 ESP-IDF: $IDF_PATH (设置 IDF_PATH)" >&2
    exit 1
fi

# shellcheck disable=SC1090
source "$IDF_PATH/export.sh" >/dev/null

BUILD=$(mktemp -d /tmp/xiaozhi-bootloader.XXXXXX)
trap 'rm -rf "$BUILD"' EXIT

echo "== 构建 bootloader (rollback) =="
idf.py -C "$ROOT/tools/bootloader" -B "$BUILD" \
    -D SDKCONFIG_DEFAULTS=sdkconfig.defaults set-target esp32s3
idf.py -C "$ROOT/tools/bootloader" -B "$BUILD" bootloader

OUT="$ROOT/assets/bootloader-esp32s3-rollback.bin"
mkdir -p "$ROOT/assets"
cp "$BUILD/bootloader/bootloader.bin" "$OUT"
(cd "$ROOT/assets" && sha256sum "$(basename "$OUT")" > "$(basename "$OUT").sha256")

echo "产物: $OUT"
cat "$OUT.sha256"
