//! 主入口 - 面向 BSP 组合对象 (Board) 的应用骨架
//!
//! 流程: 创建 Board → 主循环轮询电源状态/按键 → LED 指示

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
    main,
    time::{Duration, Instant},
};

use defmt::error;
use defmt::info;
use esp_println as _;

use xiaozhi_s3::bsp::components::{BreathSpeed, LedMode, led_mode_label};
use xiaozhi_s3::bsp::{create_board, power_label};

#[panic_handler]
fn panic(panic_info: &core::panic::PanicInfo) -> ! {
    error!("{}", panic_info);
    // 打印后软复位, 避免 loop{} 卡死掩盖问题 (调试期现场自动恢复)
    esp_hal::system::software_reset()
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
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(size: 73744);

    info!("=== 小智音箱启动 ===");

    // 组合式板子: 引脚接线/断言全部收敛在 BSP 工厂内
    let mut board = create_board(peripherals);
    info!("板子 [{}] 初始化完成", board.name);

    // 音频通路验证: 开机 250Hz 提示音 300ms (Codec init + I2S DMA + PA)
    info!("音频初始化: 播放提示音...");
    board.audio.beep(300);
    let id = board.audio.es8311_chip_id();
    info!(
        "音频通路 OK (音量 {}%, ES8311 ID=0x{:02X}0x{:02X})",
        board.audio.volume(),
        id[0],
        id[1]
    );

    let mut last_power = board.power.status();
    info!("电源状态: {}", power_label(last_power));

    let mut last_boot = board.buttons.boot_pressed();
    let mut last_up = board.buttons.volume_up_pressed();
    let mut last_down = board.buttons.volume_down_pressed();
    let mut tick = 0u32;

    // 初始读取一次电池 (对齐参考实现: 每 10s 刷新)
    let mv = board.battery.read_voltage_mv();
    info!(
        "电池: {} mV, {}%",
        mv,
        board.battery.percentage().unwrap_or(0)
    );

    // LED 灯语演示: 无键=慢呼吸(待机) / 音量-=中呼吸(识别) / 音量+=快呼吸(唤醒) / BOOT=常亮(回答)
    let mut last_mode = LedMode::Breathing(BreathSpeed::Slow);
    board.leds.set_mode(last_mode);
    info!("LED 灯语: 待机=慢呼吸 音量-=中呼吸 音量+=快呼吸 BOOT=常亮");

    info!("系统初始化完成, 进入主循环");

    loop {
        // 1. 电源状态变化时打印 (分类逻辑在 xiaozhi-core, host 单测覆盖)
        let st = board.power.status();
        if st != last_power {
            info!(
                "电源状态: {} -> {}",
                power_label(last_power),
                power_label(st)
            );
            last_power = st;
        }

        // 2. 按键边沿检测
        let boot = board.buttons.boot_pressed();
        if boot != last_boot {
            info!("BOOT 键 {}", if boot { "按下" } else { "松开" });
            last_boot = boot;
        }
        let up = board.buttons.volume_up_pressed();
        if up != last_up {
            info!("音量+ 键 {}", if up { "按下" } else { "松开" });
            last_up = up;
        }
        let down = board.buttons.volume_down_pressed();
        if down != last_down {
            info!("音量- 键 {}", if down { "按下" } else { "松开" });
            last_down = down;
        }

        // 3. LED 灯语: 按键切换模式 (LEDC 硬件渐变)
        //    BOOT=常亮(回答) / 音量+=快呼吸(唤醒) / 音量-=中呼吸(识别) / 无键=慢呼吸(待机)
        let mode = if boot {
            LedMode::On
        } else if up {
            LedMode::Breathing(BreathSpeed::Fast)
        } else if down {
            LedMode::Breathing(BreathSpeed::Medium)
        } else {
            LedMode::Breathing(BreathSpeed::Slow)
        };
        if mode != last_mode {
            info!("灯语切换 -> {}", led_mode_label(mode));
            board.leds.set_mode(mode);
            last_mode = mode;
        }
        board.leds.tick();

        // 4. 电池状态: 每 10 秒刷新一次 (50ms tick × 200)
        tick += 1;
        if tick.is_multiple_of(200) {
            let mv = board.battery.read_voltage_mv();
            info!(
                "电池: {} mV, {}%",
                mv,
                board.battery.percentage().unwrap_or(0)
            );
        }

        let delay_start = Instant::now();
        while delay_start.elapsed() < Duration::from_millis(50) {}
    }
}
