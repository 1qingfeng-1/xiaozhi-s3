/* esp-hal no_std 环境垫片: 版本宏 (对齐 esp-dsp 1.5 的 IDF5 假设) */
#pragma once
#define ESP_IDF_VERSION_VAL(major, minor, patch) ((major << 16) | (minor << 8) | patch)
#define ESP_IDF_VERSION ESP_IDF_VERSION_VAL(5, 1, 0)
