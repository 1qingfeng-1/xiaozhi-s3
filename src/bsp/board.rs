//! 板级配置契约 — 所有板子的公共类型定义（"基类"）
//!
//! [`Board`] 是唯一出口，内部按功能细分子 struct。
//! 每块板只需在本目录下提供一个 `pub const BOARD: Board = Board { ... };`。

/// 板级配置契约 (纯数据, 编译期常量)
///
/// 用法示例:
/// ```ignore
/// use xiaozhi_s3::bsp::active::BOARD;
/// let sda = BOARD.codec.sda_gpio;
/// ```
pub struct BoardConfig {
    /// 板名 (用于日志)
    pub name: &'static str,
    /// 音频 I2S 与功放
    pub audio: AudioBus,
    /// 音频 Codec 控制总线 (I2C)
    pub codec_ctrl: CodecCtrlBus,
    /// 按键
    pub buttons: Buttons,
    /// 电源与电池
    pub power: Power,
    /// LED
    pub leds: Leds,
    /// 显示屏
    pub display: Display,
}

// ---------------------------------------------------------------------------
// 音频数据通路 (I2S)
// ---------------------------------------------------------------------------

/// 音频 I2S 总线引脚与采样率
pub struct AudioBus {
    /// 输入(录音)采样率 Hz
    pub input_sample_rate: u32,
    /// 输出(播放)采样率 Hz
    pub output_sample_rate: u32,
    pub mclk_gpio: u8,
    pub ws_gpio: u8,
    pub bclk_gpio: u8,
    /// 录音数据输入 (ADC → ESP32, I2S RX)
    pub din_gpio: u8,
    /// 播放数据输出 (ESP32 → DAC, I2S TX)
    pub dout_gpio: u8,
    /// 功放(PA)使能脚；部分板子无 PA，恒为 None 时驱动自动跳过
    pub pa_enable_gpio: Option<u8>,
}

impl AudioBus {
    /// PA 是否需要驱动
    pub fn has_pa(&self) -> bool {
        self.pa_enable_gpio.is_some()
    }
}

// ---------------------------------------------------------------------------
// Codec 控制 (I2C)
// ---------------------------------------------------------------------------

/// 音频 Codec I2C 控制总线
///
/// 命名按**功能角色**(DAC=播放通道 / ADC=录音通道)，与具体芯片型号解耦；
/// 型号 (如 ES8311/ES7210) 由各板的驱动实现选择，不在公共契约中出现。
pub struct CodecCtrlBus {
    pub sda_gpio: u8,
    pub scl_gpio: u8,
    /// 播放通道 Codec 的 7 位 I2C 地址
    pub dac_addr: u8,
    /// 录音通道 Codec 的 7 位 I2C 地址
    pub adc_addr: u8,
    /// 播放通道芯片型号描述 (如 "ES8311")，仅供日志展示，不参与逻辑判断
    pub dac_chip: &'static str,
    /// 录音通道芯片型号描述 (如 "ES7210")
    pub adc_chip: &'static str,
    /// 是否采集回采参考信号用于 AEC 回声消除
    pub input_reference: bool,
}

// ---------------------------------------------------------------------------
// 人机交互
// ---------------------------------------------------------------------------

/// 按键组 (低电平有效)
pub struct Buttons {
    pub boot_gpio: u8,
    pub volume_up_gpio: u8,
    pub volume_down_gpio: u8,
}

/// 可直接驱动的 LED
pub struct Leds {
    /// 板载 LED
    pub builtin_gpio: u8,
    /// Some(载波频率Hz) = LEDC PWM 调光 (支持呼吸效果);
    /// None = 普通 IO 开关 (不支持调光)
    pub builtin_pwm_hz: Option<u32>,
}

// ---------------------------------------------------------------------------
// 电源
// ---------------------------------------------------------------------------

/// 电源状态输入与电池检测
pub struct Power {
    /// 充电中指示 (低电平 = 充电中)
    pub charging_gpio: u8,
    /// 充电完成指示 (低电平 = 已充满)
    pub charge_done_gpio: u8,
    /// 电池电压分压检测脚
    pub battery_adc_gpio: u8,
    /// 电压还原系数: 实际mV = ADC结果(mV) × 还原系数 (由板级分压电阻决定)
    pub battery_voltage_multiplier: u16,
    /// 电量 0% 对应的电池端电压 (mV)
    pub battery_min_mv: u16,
    /// 电量 100% 对应的电池端电压 (mV)
    pub battery_max_mv: u16,
}

// ---------------------------------------------------------------------------
// 显示
// ---------------------------------------------------------------------------

/// SPI 显示屏
pub struct Display {
    pub sck_gpio: u8,
    pub mosi_gpio: u8,
    pub cs_gpio: u8,
    pub dc_gpio: u8,
    pub reset_gpio: u8,
    /// 背光脚
    pub backlight_gpio: u8,
    pub width_px: u16,
    pub height_px: u16,
}
