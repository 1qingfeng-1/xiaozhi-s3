/* esp-hal no_std 环境垫片: ESP32-S3 (LX7) 的最小 ISA 能力集
 * (esp-dsp platform 头用其决定 SIMD 变体; compat 桥接已走 ANSI 路径) */
#pragma once
#define XCHAL_HAVE_FP 1
#define XCHAL_HAVE_LOOPS 1
#define XCHAL_HAVE_MAC16 1
