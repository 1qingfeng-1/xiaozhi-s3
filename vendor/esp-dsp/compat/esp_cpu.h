/* esp-hal no_std 环境垫片: dsp_common.h 仅引用 CPU 周期计数宏 */
#pragma once
#include <stdint.h>
static inline uint32_t esp_cpu_get_cycle_count(void) { return 0; }
