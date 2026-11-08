//! 单调时钟 → Unix 毫秒换算
//!
//! ESP32-S3 无 RTC 后备电池/外部 RTC, 上电后只有 `esp_hal::time::Instant`
//! 的单调计数。OTA 清单的 `server_time.timestamp` 到达时记录一次
//! `UNIX_MS_OFFSET = 服务器时间 - 开机单调时间`, 之后 `now_unix_ms()` 即可
//! 得到 Unix 毫秒。该偏移仅在本次上电有效 (重启后丢失, 等下一次 OTA 同步)。
//!
//! 目标 (xtensa-esp32s3) **不支持 64 位原子** (`target_has_atomic` 只到 32),
//! 因此偏移量用 `critical_section::Mutex<Cell<u64>>` 保存; 临界区仅两条指令,
//! 对音频主循环无影响。

use core::cell::Cell;
use core::sync::atomic::{AtomicBool, Ordering};

use critical_section::Mutex;

/// 服务器 Unix 毫秒 - 本机单调毫秒 (仅在 `SYNCED` 为真时有效)
static UNIX_MS_OFFSET: Mutex<Cell<u64>> = Mutex::new(Cell::new(0));
/// 是否已通过 server_time 同步过 (偏移 0 本身可能是合法值, 需单独标记)
static SYNCED: AtomicBool = AtomicBool::new(false);

/// 本机单调毫秒 (自复位起)
fn monotonic_ms() -> u64 {
    esp_hal::time::Instant::now()
        .duration_since_epoch()
        .as_millis()
}

/// 用服务端下发的 Unix 毫秒时间戳校准时钟
///
/// `timestamp_ms` 为 UTC Unix 毫秒 (OTA 清单 `server_time.timestamp`),
/// 时区偏移不参与换算 (仅用于本地时间显示)。
pub fn set_from_server_time(timestamp_ms: u64) {
    let now = monotonic_ms();
    // 服务器时间早于本机开机时长 (异常/回绕): 饱和到 0, 避免下溢
    let offset = timestamp_ms.saturating_sub(now);
    critical_section::with(|cs| UNIX_MS_OFFSET.borrow(cs).set(offset));
    SYNCED.store(true, Ordering::Release);
}

/// 当前 Unix 毫秒; 尚未收到过 server_time → None
pub fn now_unix_ms() -> Option<u64> {
    if !SYNCED.load(Ordering::Acquire) {
        return None;
    }
    let offset = critical_section::with(|cs| UNIX_MS_OFFSET.borrow(cs).get());
    Some(monotonic_ms().saturating_add(offset))
}

/// 是否已完成 server_time 同步
pub fn is_synced() -> bool {
    SYNCED.load(Ordering::Acquire)
}
