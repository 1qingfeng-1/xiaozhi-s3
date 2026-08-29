//! bins 共用的构建期工具宏

/// 常驻静态分配: 值写入 `StaticCell` 并返回 `&'static mut`
///
/// 同一调用点只能执行一次 (二次初始化会 panic); 循环体内需要重复分配时
/// 改用堆泄漏 (见 `session.rs` 重连路径注释)。
#[macro_export]
macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}
