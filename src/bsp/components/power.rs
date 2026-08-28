//! 电源组件 — 充电 IC 状态脚读取, 分类逻辑在 xiaozhi-core

use esp_hal::gpio::Input;

use xiaozhi_core::board::{PowerStatus, charge_status};

/// 电源状态输入 (低电平有效)
pub struct Power {
    charging: Input<'static>,
    charge_done: Input<'static>,
}

impl Power {
    pub fn new(charging: Input<'static>, charge_done: Input<'static>) -> Self {
        Self {
            charging,
            charge_done,
        }
    }

    /// 读取当前电源状态
    pub fn status(&self) -> PowerStatus {
        charge_status(self.charging.is_low(), self.charge_done.is_low())
    }
}

/// PowerStatus 的日志标签
pub fn power_label(s: PowerStatus) -> &'static str {
    match s {
        PowerStatus::Charging => "充电中",
        PowerStatus::Charged => "充电完成",
        PowerStatus::Discharging => "放电中",
        PowerStatus::Idle => "空闲",
    }
}
