//! LED 组件 — PWM 调光 (LEDC 硬件渐变实现呼吸) / 普通 IO 开关
//!
//! 呼吸效果由 LEDC 硬件 duty fade 完成 (`start_duty_fade`)，CPU 仅在
//! 渐变结束时调用 [`Leds::tick`] 反转方向，零轮询开销。
//!
//! # 灯语约定 (后续语音子系统按此映射)
//!
//! | 语音状态   | 灯语                  |
//! |------------|-----------------------|
//! | 待机/空闲  | [`LedMode::Breathing`]([`BreathSpeed::Slow`]) |
//! | 唤醒成功   | [`LedMode::Breathing`]([`BreathSpeed::Fast`]) |
//! | 聆听/识别  | [`LedMode::Breathing`]([`BreathSpeed::Medium`]) |
//! | 回答/播放  | [`LedMode::On`]       |
//! | 静音/关闭  | [`LedMode::Off`]      |

use esp_hal::{
    gpio::Output,
    ledc::{
        LowSpeed,
        channel::{Channel, ChannelIFace},
    },
};

/// 呼吸速度档位
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreathSpeed {
    /// 慢: 全周期 3.6s — 待机
    Slow,
    /// 中: 全周期 1.8s — 聆听/识别
    Medium,
    /// 快: 全周期 0.8s — 唤醒/处理中
    Fast,
}

impl BreathSpeed {
    /// 半周期时长 (ms)
    pub fn half_cycle_ms(self) -> u16 {
        match self {
            BreathSpeed::Slow => 1800,
            BreathSpeed::Medium => 900,
            BreathSpeed::Fast => 400,
        }
    }
}

/// LED 工作模式 ("灯语"原语)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedMode {
    /// 熄灭
    Off,
    /// 常亮 (最亮)
    On,
    /// 呼吸调光, 速度可选 (需周期调用 [`Leds::tick`])
    Breathing(BreathSpeed),
}

/// LedMode 的日志标签 (灯语语义)
pub fn led_mode_label(m: LedMode) -> &'static str {
    match m {
        LedMode::Off => "熄灭",
        LedMode::On => "常亮",
        LedMode::Breathing(BreathSpeed::Slow) => "慢呼吸",
        LedMode::Breathing(BreathSpeed::Medium) => "中呼吸",
        LedMode::Breathing(BreathSpeed::Fast) => "快呼吸",
    }
}

/// LED 内部驱动方式
enum Inner {
    /// LEDC PWM 调光
    Pwm {
        channel: Channel<'static, LowSpeed>,
        breath_up: bool,
    },
    /// 普通 IO 开关
    Plain { out: Output<'static>, lit: bool },
}

/// 板载 LED (以低电平/高电平有效性参数化)
pub struct Leds {
    inner: Inner,
    active_low: bool,
    mode: LedMode,
}

impl Leds {
    /// PWM 调光模式 (channel 已由工厂完成 LEDC timer/channel 配置)
    pub fn new_pwm(channel: Channel<'static, LowSpeed>, active_low: bool) -> Self {
        let mut leds = Self {
            inner: Inner::Pwm {
                channel,
                breath_up: true,
            },
            active_low,
            mode: LedMode::Off,
        };
        leds.apply_off();
        leds
    }

    /// 普通 IO 开关模式 (无 PWM 能力的板子)
    pub fn new_plain(out: Output<'static>, active_low: bool) -> Self {
        let mut leds = Self {
            inner: Inner::Plain { out, lit: false },
            active_low,
            mode: LedMode::Off,
        };
        leds.apply_off();
        leds
    }

    /// 逻辑亮度 pct → 硬件 duty pct (低电平有效时反转)
    fn duty_pct(&self, brightness_pct: u8) -> u8 {
        if self.active_low {
            100 - brightness_pct
        } else {
            brightness_pct
        }
    }

    fn apply_off(&mut self) {
        if let Inner::Plain { out, .. } = &mut self.inner {
            if self.active_low {
                out.set_high();
            } else {
                out.set_low();
            }
        }
    }

    /// 当前模式
    pub fn mode(&self) -> LedMode {
        self.mode
    }

    /// 切换工作模式 (相同模式重复设置无操作, 呼吸不卡顿)
    pub fn set_mode(&mut self, mode: LedMode) {
        if mode == self.mode {
            return;
        }
        self.mode = mode;
        let (d_dark, d_bright) = (self.duty_pct(0), self.duty_pct(100));
        match (&mut self.inner, mode) {
            (Inner::Pwm { channel, .. }, LedMode::Breathing(speed)) => {
                let _ = channel.start_duty_fade(d_dark, d_bright, speed.half_cycle_ms());
            },
            (Inner::Pwm { channel, .. }, target) => {
                // 直接写 duty 定点 (禁止 fade(start==end): steps=0 除零 panic)
                let d = match target {
                    LedMode::On => d_bright,
                    _ => d_dark,
                };
                let _ = channel.set_duty(d);
            },
            (Inner::Plain { lit, .. }, LedMode::On) => *lit = true,
            (Inner::Plain { lit, .. }, _) => *lit = false,
        }
    }

    /// 周期驱动: Breathing 模式下渐变结束则反转方向 (其余模式无操作)
    pub fn tick(&mut self) {
        let LedMode::Breathing(speed) = self.mode else {
            return;
        };
        let active_low = self.active_low;
        let Inner::Pwm { channel, breath_up } = &mut self.inner else {
            return;
        };
        if !channel.is_duty_fade_running() {
            *breath_up = !*breath_up;
            let (from, to) = if *breath_up {
                (0u8, 100u8)
            } else {
                (100u8, 0u8)
            };
            let (d_from, d_to) = if active_low {
                (100 - from, 100 - to)
            } else {
                (from, to)
            };
            let _ = channel.start_duty_fade(d_from, d_to, speed.half_cycle_ms());
        }
    }

    /// 简化别名: 点亮 (切 On 模式)
    pub fn on(&mut self) {
        self.set_mode(LedMode::On);
    }

    /// 简化别名: 熄灭 (切 Off 模式)
    pub fn off(&mut self) {
        self.set_mode(LedMode::Off);
    }

    /// 简化别名: 亮灭切换
    pub fn toggle(&mut self) {
        self.set_mode(if self.mode == LedMode::On {
            LedMode::Off
        } else {
            LedMode::On
        });
    }
}
