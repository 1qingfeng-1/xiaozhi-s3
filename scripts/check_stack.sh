#!/usr/bin/env bash
# 主任务栈预算守卫
#
# esp-rtos 0.3 的 start() 直接把链接器符号
# `_stack_start_cpu0 .. _stack_end_cpu0` 之间的 DRAM 剩余空间作为主任务栈。
# 任何 `.bss` 增长 (任务 POOL / 常驻缓冲 / 堆) 都会直接吃掉音频栈,
# 而 opus SILK 60ms 编码需要数十 KB 栈。这里在 CI 做硬性下限检查。
#
# 用法: scripts/check_stack.sh [elf]
# 环境变量:
#   XTENSA_NM        nm 可执行文件 (默认 xtensa-esp32s3-elf-nm)
#   MIN_STACK_BYTES  下限字节数 (默认 98304 = 96KiB)
set -euo pipefail

ELF=${1:-target/xtensa-esp32s3-none-elf/release/xiaozhi-s3}
NM=${XTENSA_NM:-xtensa-esp32s3-elf-nm}
MIN_STACK_BYTES=${MIN_STACK_BYTES:-98304}

if [ ! -f "$ELF" ]; then
    echo "找不到固件: $ELF" >&2
    exit 1
fi

if ! command -v "$NM" >/dev/null 2>&1; then
    echo "找不到 $NM (请安装 espup xtensa 工具链)" >&2
    exit 1
fi

start=$("$NM" -n "$ELF" | awk '$3=="_stack_start_cpu0"{print $1}')
end=$("$NM" -n "$ELF" | awk '$3=="_stack_end_cpu0"{print $1}')

if [ -z "${start:-}" ] || [ -z "${end:-}" ]; then
    echo "未找到 _stack_start_cpu0/_stack_end_cpu0 符号" >&2
    exit 1
fi

# nm 输出为无前缀十六进制
size=$((0x$start - 0x$end))
echo "主任务栈: $size B (下限 $MIN_STACK_BYTES B)"

if [ "$size" -lt "$MIN_STACK_BYTES" ]; then
    echo "错误: 主任务栈不足。请减少 .bss (任务池/常驻缓冲) 或缩小内部堆。" >&2
    exit 1
fi
