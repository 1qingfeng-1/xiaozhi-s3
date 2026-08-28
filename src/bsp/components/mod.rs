//! 运行时硬件组件 — 组合进 [`Board`] 的各功能块（对应 C++ 各 GetXxx() 组件）
//!
//! 每个组件一个子模块；组件持有 esp-hal 外设对象并提供语义化 API。
//! 引脚接线由各板的 `create_board()` 工厂完成，应用层只面向组件。

mod audio;
mod battery;
mod buttons;
mod leds;
mod power;

pub use audio::Audio;
pub use battery::Battery;
pub use buttons::Buttons;
pub use leds::{BreathSpeed, LedMode, Leds, led_mode_label};
pub use power::{Power, power_label};

/// 组合式板子运行时对象
///
/// `main` 中创建一次 (`bsp::create_board(peripherals)`)，随后以
/// `&mut Board` 传递给各子系统 —— 组合优先于继承，无全局单例。
pub struct Board {
    pub name: &'static str,
    pub audio: Audio,
    pub leds: Leds,
    pub buttons: Buttons,
    pub power: Power,
    pub battery: Battery,
    // TODO: audio / display 组件实现后加入组合
}
