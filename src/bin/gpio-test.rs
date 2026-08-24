//! GPIO 测试 - 验证 LED 和按键功能
//!
//! 预期行为:
//! - GPIO4 (LED) 闪烁
//! - 按键 GPIO18/GPIO1/GPIO2 被检测

#![no_std]
#![no_main]

use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull},
    main,
    time::{Duration, Instant},
};

use defmt::error;
use defmt::info;
use esp_println as _;

#[panic_handler]
fn panic(panic_info: &core::panic::PanicInfo) -> ! {
    error!("{}", panic_info);
    loop {}
}

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

#[main]
fn main() -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);

    info!("=== GPIO 测试开始 ===");

    // LED 电源指示 (GPIO4) - 推挽输出
    let led_config = OutputConfig::default();
    let mut led = Output::new(peripherals.GPIO4, Level::High, led_config);
    info!("LED GPIO4 已开启 (高电平)");

    // 按键配置 - 上拉输入 (低电平有效)
    let btn_config = InputConfig::default().with_pull(Pull::Up);

    let mut btn_vol_down = Input::new(peripherals.GPIO18, btn_config); // SW1
    let mut btn_vol_up = Input::new(peripherals.GPIO1, btn_config); // SW2
    let mut btn_config_pin = Input::new(peripherals.GPIO2, btn_config); // SW3

    info!("按键状态检测:");
    info!(
        "  音量减 (GPIO18): {}",
        if btn_vol_down.is_low() {
            "按下"
        } else {
            "未按下"
        }
    );
    info!(
        "  音量加 (GPIO1):  {}",
        if btn_vol_up.is_low() {
            "按下"
        } else {
            "未按下"
        }
    );
    info!(
        "  配网 (GPIO2):    {}",
        if btn_config_pin.is_low() {
            "按下"
        } else {
            "未按下"
        }
    );

    // LED 闪烁测试
    info!("LED 闪烁测试 (10次)...");
    for i in 0..10 {
        // 关闭 LED
        led.set_low();
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(200) {}

        // 开启 LED
        led.set_high();
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(200) {}

        info!("  闪烁 {}/10", i + 1);
    }

    // 关闭 LED
    led.set_low();
    info!("=== GPIO 测试完成 ===");
    info!("LED 已关闭");

    // 持续检测按键
    info!("持续检测按键 (按任意键显示状态)...");
    loop {
        let vol_down = btn_vol_down.is_low();
        let vol_up = btn_vol_up.is_low();
        let config_btn = btn_config_pin.is_low();

        if vol_down || vol_up || config_btn {
            info!("检测到按键:");
            info!("  音量减: {}", if vol_down { "按下" } else { "未按下" });
            info!("  音量加: {}", if vol_up { "按下" } else { "未按下" });
            info!("  配网:   {}", if config_btn { "按下" } else { "未按下" });
        }

        let delay_start = Instant::now();
        while delay_start.elapsed() < Duration::from_millis(100) {}
    }
}
