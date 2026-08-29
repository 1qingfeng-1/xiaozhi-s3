//! I2C 扫描 - 验证 Codec 是否在线
//! 引脚/地址来自 bsp::active::BOARD, 换板自动适配

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use esp_hal::{
    clock::CpuClock,
    i2c::master::{Config, I2c},
    main,
    time::Rate,
};

use defmt::info;
use esp_println as _;

use xiaozhi_s3::bsp::active::BOARD;

#[panic_handler]
fn panic(panic_info: &core::panic::PanicInfo) -> ! {
    info!("!!! PANIC: {}", panic_info);
    loop {}
}

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // esp-radio 已无条件入链: 调度器必须先于任何 radio 符号使用启动
    esp_alloc::heap_allocator!(size: 64 * 1024);
    let timg0 = esp_hal::timer::timg::TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);
    let _ = spawner;

    esp_alloc::heap_allocator!(size: 73744);

    // 编译期校验: 本 bin 的硬接线与所选板子的 BSP 配置一致 (换板时改错立即报错)
    const _: () = assert!(BOARD.codec_ctrl.sda_gpio == 38 && BOARD.codec_ctrl.scl_gpio == 47);

    info!(
        "=== I2C 扫描 [{}] (SDA=GPIO{}, SCL=GPIO{}) ===",
        BOARD.name, BOARD.codec_ctrl.sda_gpio, BOARD.codec_ctrl.scl_gpio
    );

    let i2c_config = Config::default().with_frequency(Rate::from_khz(100));
    let mut i2c = match I2c::new(peripherals.I2C0, i2c_config) {
        Ok(i) => i,
        Err(e) => {
            info!("I2C 初始化失败: {:?}", e);
            loop {}
        },
    };
    i2c = i2c.with_sda(peripherals.GPIO38);
    i2c = i2c.with_scl(peripherals.GPIO47);
    info!("I2C0 初始化成功");

    // 扫描 7 位地址 (1..=112)
    info!("开始扫描 112 个地址...");
    let mut found = 0u8;
    for addr in 1u8..=112 {
        // 读 1 字节探测, 器件应答则成功
        match i2c.read(addr, &mut [0u8]) {
            Ok(_) => {
                info!("  [0x{:02X}] 应答!", addr);
                found += 1;
                // 按功能角色识别板级 Codec (型号仅展示)
                if addr == BOARD.codec_ctrl.dac_addr {
                    info!("    → 播放通道 {} (DAC)", BOARD.codec_ctrl.dac_chip);
                } else if addr == BOARD.codec_ctrl.adc_addr {
                    info!("    → 录音通道 {} (ADC)", BOARD.codec_ctrl.adc_chip);
                }
            },
            Err(_) => {},
        }
    }

    if found == 0 {
        info!("!!! 未发现任何 I2C 器件, 检查 SDA/SCL 接线");
    } else {
        info!("=== 扫描完成, 发现 {} 个器件 ===", found);
    }

    // 循环: 持续尝试读 Codec 状态
    info!(
        "循环读取 DAC({}@0x{:02X}) / ADC({}@0x{:02X})...",
        BOARD.codec_ctrl.dac_chip,
        BOARD.codec_ctrl.dac_addr,
        BOARD.codec_ctrl.adc_chip,
        BOARD.codec_ctrl.adc_addr
    );
    loop {
        let mut buf = [0u8; 2];
        match i2c.read(BOARD.codec_ctrl.dac_addr, &mut buf) {
            Ok(_) => info!(
                "  DAC {}(0x{:02X}): {:02X} {:02X}",
                BOARD.codec_ctrl.dac_chip, BOARD.codec_ctrl.dac_addr, buf[0], buf[1]
            ),
            Err(e) => info!(
                "  DAC {}(0x{:02X}): ERR {:?}",
                BOARD.codec_ctrl.dac_chip, BOARD.codec_ctrl.dac_addr, e
            ),
        }
        match i2c.read(BOARD.codec_ctrl.adc_addr, &mut buf) {
            Ok(_) => info!(
                "  ADC {}(0x{:02X}): {:02X} {:02X}",
                BOARD.codec_ctrl.adc_chip, BOARD.codec_ctrl.adc_addr, buf[0], buf[1]
            ),
            Err(e) => info!(
                "  ADC {}(0x{:02X}): ERR {:?}",
                BOARD.codec_ctrl.adc_chip, BOARD.codec_ctrl.adc_addr, e
            ),
        }
        let start = esp_hal::time::Instant::now();
        while start.elapsed() < esp_hal::time::Duration::from_millis(1000) {}
    }
}
