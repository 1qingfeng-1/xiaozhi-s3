#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

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

// This creates a default app-descriptor required by the esp-idf bootloader.
esp_bootloader_esp_idf::esp_app_desc!();

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[main]
fn main() -> ! {
    // generator version: 1.3.0
    // generator parameters: --chip esp32s3 -o unstable-hal -o alloc -o defmt

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);

    info!("=== 小智音箱启动 ===");
    info!("ESP32-S3 AI-2 智能音箱");
    info!("硬件: ESP32-S3R8 + ES8311 + ES7210");
    info!("Flash: 16MB");
    info!("================================");

    // LED 电源指示 (GPIO4) - 高电平点亮
    let led_config = OutputConfig::default();
    let mut led = Output::new(peripherals.GPIO4, Level::High, led_config);
    info!("电源 LED 已开启");

    // 配置按键 (低电平有效, 上拉输入)
    let btn_config = InputConfig::default().with_pull(Pull::Up);
    let _btn_vol_down = Input::new(peripherals.GPIO18, btn_config); // SW1
    let _btn_vol_up = Input::new(peripherals.GPIO1, btn_config); // SW2
    let _btn_config = Input::new(peripherals.GPIO2, btn_config); // SW3
    info!("按键已配置: 音量+/音量-/配网");

    // 充电状态检测 (LGS4056HDA)
    let _chg_done = Input::new(peripherals.GPIO28, InputConfig::default()); // 充电完成
    let _chg_chrg = Input::new(peripherals.GPIO29, InputConfig::default()); // 充电中
    info!("充电检测已配置");

    info!("================================");
    info!("系统初始化完成");
    info!("================================");

    // 主循环
    loop {
        // 1. 检查充电状态
        // if _chg_chrg.is_low() {
        //     info!("充电中...");
        // }

        // 2. 检查按键
        // if _btn_vol_down.is_low() {
        //     info!("音量减");
        // }
        // if _btn_vol_up.is_low() {
        //     info!("音量加");
        // }

        // 3. 音频处理
        // TODO: I2S 音频采集和播放

        // 4. WiFi 连接
        // TODO: 连接 WiFi

        // 5. 网络通信
        // TODO: MQTT/HTTP 通信

        // 6. 语音识别
        // TODO: ASR

        // 7. 语音合成
        // TODO: TTS

        // 8. 大语言模型
        // TODO: LLM 对话

        let delay_start = Instant::now();
        while delay_start.elapsed() < Duration::from_millis(100) {}

        // LED 呼吸灯效果
        // 简化版: 闪烁
        led.set_low();
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(50) {}
        led.set_high();
    }

    // for inspiration have a look at the examples at https://github.com/esp-rs/esp-hal/tree/esp-hal-v1.1.0/examples
}
