//! 常驻内存分配工具 — 优先 PSRAM, 失败回退内部堆
//!
//! 内部堆只有 128KB, 大块 CPU 缓冲 (PCM 环/预录/网络) 必须落 PSRAM;
//! DMA 缓冲不能走这里 (必须在 `psram_allocator!` 注册前用内部堆分配)。
//! 所有分配块均常驻至重启 (泄漏语义), 调用点应只在启动/一次性路径。

use core::alloc::Layout;

/// 分配字节缓冲: 优先 External (PSRAM), 失败回退内部堆; 返回块已清零
pub fn psram_bytes(len: usize) -> &'static mut [u8] {
    let layout = Layout::from_size_align(len, 4).expect("bad layout");
    // SAFETY: 非零布局, 分配块常驻
    let p =
        unsafe { esp_alloc::HEAP.alloc_caps(esp_alloc::MemoryCapability::External.into(), layout) };
    if p.is_null() {
        alloc::vec![0u8; len].leak()
    } else {
        // SAFETY: 可写 len 字节; 清零避免脏数据 (静音/缓冲初值)
        unsafe {
            core::ptr::write_bytes(p, 0, len);
            core::slice::from_raw_parts_mut(p, len)
        }
    }
}

/// 分配 i16 缓冲 (长度为样本数, 字节数 = len*2)
pub fn psram_i16(len: usize) -> &'static mut [i16] {
    let b = psram_bytes(len * 2);
    // SAFETY: 4 字节对齐, 长度按 i16 计
    unsafe { core::slice::from_raw_parts_mut(b.as_mut_ptr().cast(), len) }
}

/// 把常驻对象放到 PSRAM, 返回 `'static` 引用。
///
/// `Channels` 等对象放 PSRAM 可把 `.bss` 让给主任务栈 (opus 编码需要)。
/// 仅用于无原子类型、无 Drop 的常驻对象; PSRAM 上的 `Atomic*` 在 S3
/// 上不可靠 (esp-alloc 文档)。
pub fn psram_static<T>(value: T) -> &'static T {
    let layout = Layout::new::<T>();
    // SAFETY: 非零布局, 分配块常驻
    let p =
        unsafe { esp_alloc::HEAP.alloc_caps(esp_alloc::MemoryCapability::External.into(), layout) };
    if p.is_null() {
        alloc::boxed::Box::leak(alloc::boxed::Box::new(value))
    } else {
        // SAFETY: 布局匹配; 写入一次后由内部可变性访问
        unsafe {
            (p as *mut T).write(value);
            &*(p as *const T)
        }
    }
}
