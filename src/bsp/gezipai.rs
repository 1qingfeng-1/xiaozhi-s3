//! gezipai 开发板配置 (源自官方 config.h / gezipai_board.cc)
//!
//! ESP32-S3-WROOM-1U, 8MB Octal PSRAM (esp-hal 自动探测, 占用 GPIO33-37)。
//! 注意: GPIO12/13 是 LIGHT 串口, 并非充电检测 (此前误判已纠正)。

use super::BoardConfig;
use super::{AudioBus, Buttons, CodecCtrlBus, Display, Leds, Power};

/// gezipai 板级配置
pub const BOARD: BoardConfig = BoardConfig {
    name: "gezipai",

    // -----------------------------------------------------------------------
    // 音频数据通路: I2S + 功放
    // -----------------------------------------------------------------------
    audio: AudioBus {
        input_sample_rate: 16_000,
        output_sample_rate: 16_000,
        mclk_gpio: 40,
        ws_gpio: 45,
        bclk_gpio: 39,
        din_gpio: 21,             // 麦克风 → ESP32 (I2S RX)
        dout_gpio: 48,            // ESP32 → DAC (I2S TX)
        pa_enable_gpio: Some(46), // LPA2103A 功放使能, 高电平开启
    },

    // -----------------------------------------------------------------------
    // Codec 控制总线: I2C
    // 播放通道 = ES8311 @0x18, 录音通道 = ES7210 @0x40 (型号仅在注释体现,
    // 上层驱动按 dac/adc 角色寻址, 不感知具体型号)
    // -----------------------------------------------------------------------
    codec_ctrl: CodecCtrlBus {
        sda_gpio: 38,
        scl_gpio: 47,
        dac_addr: 0x18,
        adc_addr: 0x40,
        dac_chip: "ES8311",
        adc_chip: "ES7210",
        input_reference: true,
    },

    // -----------------------------------------------------------------------
    // 按键 (低电平有效)
    // -----------------------------------------------------------------------
    buttons: Buttons {
        boot_gpio: 1,
        volume_up_gpio: 18,
        volume_down_gpio: 2,
    },

    // -----------------------------------------------------------------------
    // 电源与电池
    // -----------------------------------------------------------------------
    power: Power {
        charging_gpio: 41,
        charge_done_gpio: 42,
        battery_adc_gpio: 3,           // ADC1_CH2
        battery_voltage_multiplier: 3, // 分压比 1/3, 读数 x3 还原 mV
        battery_min_mv: 3000,          // 3.0V ~ 4.2V 锂电池
        battery_max_mv: 4200,
    },

    leds: Leds {
        builtin_gpio: 17,             // 低电平点亮
        builtin_pwm_hz: Some(19_531), // 80MHz/2^12, PWM 12bit 分辨率
    },

    // -----------------------------------------------------------------------
    // 显示屏 ST7789 SPI 240x240
    // -----------------------------------------------------------------------
    display: Display {
        sck_gpio: 7,
        mosi_gpio: 6,
        cs_gpio: 15,
        dc_gpio: 16,
        reset_gpio: 5,
        backlight_gpio: 4,
        width_px: 240,
        height_px: 240,
    },
};

// 板级扩展记录 (尚未接入公共契约, 实现时再按功能加入 Board):
// - 灯效串口: RX=GPIO12 / TX=GPIO13
// - PWM LED: GPIO11, 其 UART TX 脚=GPIO10

// ===========================================================================
// 板子组装工厂 (DECLARE_BOARD 等价物)
// 硬接线与 BOARD 配置的一致性断言集中在此, bins 无需再各自断言
// ===========================================================================

use esp_hal::{
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull},
    ledc::{
        LSGlobalClkSource, Ledc, LowSpeed,
        channel::{self, Channel, ChannelIFace},
        timer::{self, TimerIFace},
    },
    peripherals::Peripherals,
    time::Rate,
};

use super::components::{self, Board};
use alloc::boxed::Box;

/// 组装 gezipai 板子运行时对象
pub fn create_board(peripherals: Peripherals) -> Board {
    // 内部堆必须先于任何分配注册 (所有 bin 共用本工厂):
    // 音频 DMA 缓冲必须落内部 RAM (S3 GDMA 访问不了 PSRAM)。
    // 尺寸 128KB: 主任务栈 = RWDATA 剩余空间, opus 编码实测需 ~87KB
    // (SILK NSQ 栈上数组按 MAX 帧长定容), 缩堆即扩栈; opus 大状态块
    // 与会话 socket 缓冲为纯 CPU 访问, 自动落 PSRAM。
    esp_alloc::heap_allocator!(size: 128_000);

    // 编译期校验: 硬接线与所选板子的 BSP 配置一致 (换板时改错立即报错)
    const _: () = assert!(
        BOARD.leds.builtin_gpio == 17
            && BOARD.buttons.boot_gpio == 1
            && BOARD.buttons.volume_up_gpio == 18
            && BOARD.buttons.volume_down_gpio == 2
            && BOARD.power.charging_gpio == 41
            && BOARD.power.charge_done_gpio == 42
            && BOARD.power.battery_adc_gpio == 3
    );

    // LED17: 低电平点亮, LEDC PWM 12bit (呼吸调光)
    let leds = match BOARD.leds.builtin_pwm_hz {
        Some(hz) => {
            let mut ledc = Ledc::new(peripherals.LEDC);
            ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);
            // Channel::configure 要求 timer 引用 'static, 泄漏一次 (板上常驻资源)
            let timer0: &'static mut timer::Timer<'static, LowSpeed> =
                Box::leak(Box::new(ledc.timer::<LowSpeed>(timer::Number::Timer0)));
            timer0
                .configure(timer::config::Config {
                    duty: timer::config::Duty::Duty12Bit,
                    clock_source: timer::LSClockSource::APBClk,
                    frequency: Rate::from_hz(hz),
                })
                .unwrap();
            let mut ch: Channel<'static, LowSpeed> =
                ledc.channel(channel::Number::Channel0, peripherals.GPIO17);
            ch.configure(channel::config::Config {
                timer: timer0,
                duty_pct: 0,
                drive_mode: esp_hal::gpio::DriveMode::PushPull,
            })
            .unwrap();
            components::Leds::new_pwm(ch, true)
        },
        None => components::Leds::new_plain(
            Output::new(peripherals.GPIO17, Level::High, OutputConfig::default()),
            true,
        ),
    };

    // 按键: 上拉输入, 低电平有效 (BOOT=1 / VOL+=18 / VOL-=2)
    let btn_cfg = InputConfig::default().with_pull(Pull::Up);
    let buttons = components::Buttons::new(
        Input::new(peripherals.GPIO1, btn_cfg),
        Input::new(peripherals.GPIO18, btn_cfg),
        Input::new(peripherals.GPIO2, btn_cfg),
    );

    // 电源状态: CHARGING=41 / DONE=42 (低电平有效)
    let power = components::Power::new(
        Input::new(peripherals.GPIO41, InputConfig::default()),
        Input::new(peripherals.GPIO42, InputConfig::default()),
    );

    // 电池: GPIO3 (ADC1_CH2), 分压还原 x3, 量程 3.0V~4.2V
    // (对齐官方 gezipai_board.cc ReadBatteryVoltage 的 voltage * 3;
    //  其注释"分压电阻1:2所以乘以2"与代码矛盾, 以代码为准)
    let battery = components::Battery::new(
        peripherals.ADC1,
        peripherals.GPIO3,
        BOARD.power.battery_voltage_multiplier,
        BOARD.power.battery_min_mv,
        BOARD.power.battery_max_mv,
    );

    // Codec 控制总线: SDA=38 / SCL=47, 100kHz, 内部上拉 (对齐官方配置)
    let mut i2c = {
        use esp_hal::i2c::master::{Config as I2cConfig, I2c};
        let cfg = I2cConfig::default().with_frequency(Rate::from_khz(100));
        I2c::new(peripherals.I2C0, cfg).expect("I2C0 init failed")
    };
    i2c = i2c
        .with_sda(peripherals.GPIO38)
        .with_scl(peripherals.GPIO47);

    // 功放使能: PA=GPIO46, 高电平开启
    let pa = Output::new(peripherals.GPIO46, Level::Low, OutputConfig::default());

    // 音频: I2S0 全双工 + DMA_CH0, MCLK=40/BCLK=39/WS=45/DOUT=48/DIN=21, 16kHz
    let (audio, capture) = components::Audio::new(
        i2c,
        peripherals.I2S0,
        peripherals.DMA_CH0,
        peripherals.GPIO39,
        peripherals.GPIO45,
        peripherals.GPIO48,
        peripherals.GPIO40,
        peripherals.GPIO21,
        pa,
        BOARD.audio.output_sample_rate,
    );

    // 显示: ST7789 240x240, SPI2 (SCK=7 MOSI=6 CS=15 DC=16 RST=5 BL=4)
    let display = {
        let spi = components::make_spi(
            peripherals.SPI2,
            peripherals.GPIO7,
            peripherals.GPIO6,
            peripherals.GPIO15,
        );
        let dc = Output::new(peripherals.GPIO16, Level::High, OutputConfig::default());
        let rst = Output::new(peripherals.GPIO5, Level::High, OutputConfig::default());
        let bl = Output::new(peripherals.GPIO4, Level::Low, OutputConfig::default());
        components::Display::new(spi, dc, rst, bl)
    };

    // 板载 8MB Octal PSRAM 接入全局堆 (esp-hal 自动探测模式)。
    // ⚠️ 必须在音频 DMA 缓冲分配之后注册: S3 的 GDMA 访问不了 PSRAM,
    // 先注册 PSRAM 会让 I2S 缓冲落进外部 RAM 导致 DmaError(UnsupportedMemoryRegion)。
    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram);

    Board {
        name: BOARD.name,
        audio,
        capture,
        leds,
        buttons,
        power,
        battery,
        display,
    }
}
