#!/usr/bin/env bash
# 诊断/自测 feature 组合编译检查
#
# 与 CI 的 "Check feature matrix" step 保持一致；新增 feature 时只改这里。
# 需要 esp 工具链（父仓库 .cargo/config.toml 已锁定 target）。
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

COMBOS=(
    "board-gezipai"
    "board-gezipai,ps-adaptive"
    "board-gezipai,debug-diag"
    "board-gezipai,idle-selftest,session-selftest,tts-selftest"
    "board-gezipai,watchdog-selftest,ota-selftest-crash"
    "board-gezipai,aec-off,pa-off,volume-off"
    "board-gezipai,hil"
)

for features in "${COMBOS[@]}"; do
    echo "== $features =="
    cargo check --locked --no-default-features --features "$features"
done

echo "feature 矩阵全部通过。"
