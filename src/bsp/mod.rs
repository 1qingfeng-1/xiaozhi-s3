//! 板级支持包 (BSP)
//!
//! 参照 xiaozhi-esp32 的编译时选板架构，Rust 版采用**组合优先于继承**：
//!
//! | xiaozhi-esp32 (C++)           | 本项目 (Rust)                          |
//! |-------------------------------|----------------------------------------|
//! | main/boards/<board>/config.h  | src/bsp/<board>.rs 的 `BOARD` 常量      |
//! | Kconfig: CONFIG_BOARD_TYPE_X  | cargo feature: "board-x"                |
//! | Board 基类 + GetXxx() 虚函数  | components.rs 的 `Board` 组合各组件      |
//! | 子类 override GetXxx()        | 各板 `create_board()` 工厂组装           |
//! | DECLARE_BOARD(XxxBoard) 宏    | feature 门控的 `create_board` 转发       |
//! | GetInstance() 单例            | main 创建一次, `&mut Board` 传递         |
//!
//! # 编译选板
//!
//! ```bash
//! # 默认板 (gezipai)
//! cargo +esp build --release
//!
//! # 显式指定某块板 (无默认 feature 时需关闭 default)
//! cargo +esp build --no-default-features --features board-<name>
//! ```
//!
//! # 应用入口 (main.rs)
//!
//! ```ignore
//! let mut board = xiaozhi_s3::bsp::create_board(peripherals);
//! info!("板子: {}", board.name);
//! board.leds.builtin.toggle();
//! if board.buttons.boot_pressed() { /* ... */ }
//! let st = board.power.status();
//! ```
//!
//! # 新增一块板
//!
//! 1. 复制任一 `<board>.rs` 为新文件，填写 [`BoardConfig`] 各字段（导出名固定 `BOARD`），
//!    并实现 `create_board()` 工厂（含硬接线编译期断言）
//! 2. 在本文件加 `mod <board>;` 与两行 `#[cfg(feature = "board-<board>")]` 转发
//! 3. 在 Cargo.toml `[features]` 加 `board-<board> = []`
//! 4. 若作为默认板，加入 `default = [...]`
//!
//! # 使用契约
//!
//! - 引脚/地址等配置只允许通过 `active::BOARD` 读取，禁止在应用/驱动层硬编码；
//! - 硬件接线断言 (`const _: () = assert!(...)`) 只允许出现在板子工厂内；
//! - 组件行为差异: 封闭集合用枚举，开放扩展用 trait (如未来多种 Display)。

pub mod board;
pub mod components;
pub mod gezipai;

// ---------------------------------------------------------------------------
// 类型导出：板子文件只需 `use super::{BoardConfig, ...}`
// ---------------------------------------------------------------------------

pub use board::{AudioBus, BoardConfig, Buttons, CodecCtrlBus, Display, Leds, Power};
pub use components::{Board, power_label};

// ---------------------------------------------------------------------------
// 板子注册区：每块板两行，新增板子时同步更新此处与选板守卫
// ---------------------------------------------------------------------------

#[cfg(feature = "board-gezipai")]
pub use gezipai as active;
#[cfg(feature = "board-gezipai")]
pub use gezipai::create_board;

// ---------------------------------------------------------------------------
// 选板守卫：未选任何板时报错
// ---------------------------------------------------------------------------

#[cfg(not(any(feature = "board-gezipai")))]
compile_error!(
    "未选择开发板! 请启用一个 board-* feature, 例如: cargo build --features board-gezipai"
);

// 多板互斥守卫: 当前仅 1 块板无需检查。
// 有 >=2 块板后启用并补全条件, 例如:
// #[cfg(all(feature = "board-gezipai", feature = "board-xxx"))]
// compile_error!("只能选择一块开发板! 请勿同时启用多个 board-* feature");
