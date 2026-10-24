//! 主循环看门狗 (TIMG0 MWDT) — 长尾稳定性保护
//!
//! 只在音频主循环里喂狗: 主循环卡死 (DMA/驱动/状态机异常) > 8s 即硬复位。
//! 会话任务的网络阻塞不影响喂狗 (它在独立任务里, 不阻塞音频循环), 因此不会
//! 误触发。TIMG0 的 timer0 已交给 embassy 时基, MWDT 是独立计数器, 互不影响。
//!
//! 启用时机在 OTA/配网之后: 开机阶段的长操作 (下载 875KB、擦写 flash)
//! 不纳入看门狗窗口。

use core::cell::RefCell;

use critical_section::Mutex;
use esp_hal::{
    peripherals::TIMG0,
    timer::timg::{MwdtStage, Wdt},
};

/// 看门狗超时 (秒): 音频循环一轮 < 100ms, 8s 留足异常余量
const TIMEOUT_SECS: u64 = 8;

type MainWdt = Wdt<TIMG0<'static>>;

static WDT: Mutex<RefCell<Option<MainWdt>>> = Mutex::new(RefCell::new(None));

/// 启用 MWDT 并交给主循环喂狗
pub fn init(mut wdt: MainWdt) {
    wdt.set_timeout(
        MwdtStage::Stage0,
        esp_hal::time::Duration::from_secs(TIMEOUT_SECS),
    );
    wdt.enable();
    critical_section::with(|cs| WDT.borrow_ref_mut(cs).replace(wdt));
}

/// 主循环每轮调用 (极短临界区)
pub fn feed() {
    critical_section::with(|cs| {
        if let Some(w) = WDT.borrow_ref_mut(cs).as_mut() {
            w.feed();
        }
    });
}
