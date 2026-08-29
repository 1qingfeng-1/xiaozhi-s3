/* esp-hal no_std 环境垫片: 日志宏全静默 */
#pragma once
#define ESP_LOGI(tag, ...) do {} while (0)
#define ESP_LOGW(tag, ...) do {} while (0)
#define ESP_LOGE(tag, ...) do {} while (0)
#define ESP_LOGD(tag, ...) do {} while (0)
#define ESP_LOGV(tag, ...) do {} while (0)
