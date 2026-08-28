//! 按键组件 — 上拉输入, 低电平有效

use esp_hal::gpio::Input;

/// 按键组
pub struct Buttons {
    boot: Input<'static>,
    volume_up: Input<'static>,
    volume_down: Input<'static>,
}

impl Buttons {
    pub fn new(
        boot: Input<'static>,
        volume_up: Input<'static>,
        volume_down: Input<'static>,
    ) -> Self {
        Self {
            boot,
            volume_up,
            volume_down,
        }
    }

    pub fn boot_pressed(&self) -> bool {
        self.boot.is_low()
    }

    pub fn volume_up_pressed(&self) -> bool {
        self.volume_up.is_low()
    }

    pub fn volume_down_pressed(&self) -> bool {
        self.volume_down.is_low()
    }
}
