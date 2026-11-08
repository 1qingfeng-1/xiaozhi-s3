//! WakeNet 关键词唤醒 (feature `wakenet`, esp-sr v1.9.5 预编译库 FFI)
//!
//! # 根因记录 (2026-10-31 修正)
//!
//! 此前记入 `vendor/esp-sr/README.md` / ROADMAP 的「MMU block 1 映射不兼容」是
//! **误判**。真机 `InstrProhibited (PC=0x1FFE2000)` 的根因是手写 FFI 结构体
//! [`WnIface`] 成员数与 `esp_wn_iface.h` 不一致: Rust 侧多了 C 头文件里并不存在的
//! `reset_det_threshold`, 导致 `detect` 被排到偏移 0x38; 而 `esp_sr_wakenet9_quantized`
//! 函数表只有 14 项 (0x38 字节), 表尾紧邻的 rodata 常量恰好是 `0x1FFE2000`
//! (最终 ELF 中表尾字节为 `00 20 fe 1f`)。于是「detect」调用就是跳该常量。
//!
//! 修正后 [`WnIface`] 与 C 头文件逐位一致 (detect 在偏移 0x2c), 并用
//! `offset_of!` 编译期断言锁死, 防止再次错位。
//!
//! # 内存
//!
//! 模型 (srmodels.bin ~284KB hufzip 压缩) 以 `include_bytes!` 嵌入
//! flash rodata, 经 `srmodel_load` 原地解析 (官方 RAM 加载路径);
//! 解压后的权重由 esp-sr 以 `heap_caps_malloc(.., MALLOC_CAP_SPIRAM)` 申请,
//! 本模块提供 [`shim_psram_malloc`] 把它路由到 PSRAM (内部堆只有 128KB)。

#![allow(dead_code)]

use core::alloc::Layout;
use core::ptr;

use defmt::{info, warn};

use xiaozhi_core::audio::wake::WakeWordSource;

/// 检测模式: 2 通道 95% (gezipai 双麦 MIC1=L / MIC2=R, 输入必须真实交织;
/// 模型不支持时应回退到单通道模式, 见 `WakeNetSource::new`)
const DET_MODE_95: i32 = 1;

/// wakenet 检测结论 (esp_wn_iface.h: WAKENET_DETECTED = 1)
const WN_DETECTED: i32 = 1;

/// 直连麦克风的默认软件增益 (%): 麦克风数字电平仅 1~5% 满幅 (见 AGENTS.md)。
/// 600% 在真机上仍低于模型有效电平, 1500% 实测可命中; WakeNet 走原始单麦,
/// 不经过 AFE AGC。
pub const DEFAULT_GAIN_PERCENT: i32 = 1500;

/// C 侧不透明模型句柄
#[repr(C)]
pub struct WnModelData {
    _opaque: [u8; 0],
}

/// `esp_wn_iface_t` 函数表 —— **必须与 vendor/esp-sr/include/esp_wn_iface.h 逐位一致**
///
/// 顺序: create, get_start_point, get_samp_chunksize, get_channel_num,
/// get_samp_rate, get_word_num, get_word_name, set_det_threshold,
/// get_det_threshold, get_triggered_channel, get_vol_gain, detect, clean, destroy
/// (detect 位于 get_vol_gain 与 clean 之间, 共 14 项)
#[repr(C)]
pub struct WnIface {
    create: unsafe extern "C" fn(model_name: *const u8, det_mode: i32) -> *mut WnModelData,
    get_start_point: unsafe extern "C" fn(*mut WnModelData) -> i32,
    get_samp_chunksize: unsafe extern "C" fn(*mut WnModelData) -> i32,
    get_channel_num: unsafe extern "C" fn(*mut WnModelData) -> i32,
    get_samp_rate: unsafe extern "C" fn(*mut WnModelData) -> i32,
    get_word_num: unsafe extern "C" fn(*mut WnModelData) -> i32,
    get_word_name: unsafe extern "C" fn(*mut WnModelData, word_index: i32) -> *mut u8,
    set_det_threshold:
        unsafe extern "C" fn(*mut WnModelData, threshold: f32, word_index: i32) -> i32,
    get_det_threshold: unsafe extern "C" fn(*mut WnModelData, word_index: i32) -> f32,
    get_triggered_channel: unsafe extern "C" fn(*mut WnModelData) -> i32,
    get_vol_gain: unsafe extern "C" fn(*mut WnModelData, target_db: f32) -> f32,
    detect: unsafe extern "C" fn(*mut WnModelData, samples: *mut i16) -> i32,
    clean: unsafe extern "C" fn(*mut WnModelData),
    destroy: unsafe extern "C" fn(*mut WnModelData),
}

/// FFI 布局锁: detect 必须在第 11 项 (偏移 44), destroy 在第 13 项 (偏移 52)。
/// 这两条断言直接对应真机崩溃的两个特征值, 任何字段增删都会编译失败。
const DETECT_OFFSET: usize = core::mem::offset_of!(WnIface, detect);
const DESTROY_OFFSET: usize = core::mem::offset_of!(WnIface, destroy);
const _: () = assert!(DETECT_OFFSET == 11 * 4);
const _: () = assert!(DESTROY_OFFSET == 13 * 4);
const _: () = assert!(core::mem::size_of::<WnIface>() == 14 * 4);

unsafe extern "C" {
    fn esp_wn_handle_from_name(model_name: *const u8) -> *const WnIface;
    /// model_path shim: 已解析的模型列表 / 唤醒词（仅自检用）
    fn get_static_srmodels() -> *const core::ffi::c_void;
    fn esp_srmodel_get_wake_words(
        models: *const core::ffi::c_void,
        model_name: *const u8,
    ) -> *const core::ffi::c_char;
}

/// C 侧模型上下文 (嵌入的 srmodels.bin 由 model_path shim 解析)
struct Ctx {
    iface: *const WnIface,
    model: *mut WnModelData,
    /// 每次 detect 需要的样本数 (已含通道数): chunk * channels
    block: usize,
    channels: usize,
    chunk: usize,
}

// 嵌入的模型包 (hufzip 压缩, 官方 pack_model.py 打包, ~284KB flash rodata)。
// gezipai C++ 参考使用 tts 变体; 非 tts 回退路径为
// `vendor/esp-sr/model/srmodels.bin` + `b"wn9_nihaoxiaozhi\0"` (保留用于 A/B)。
static SRMODELS_BIN: &[u8] = include_bytes!("../../vendor/esp-sr/model/srmodels_tts.bin");
static MODEL_NAME: &[u8] = b"wn9_nihaoxiaozhi_tts\0";

unsafe extern "C" {
    /// vendor/esp-sr/src/model_path_shim.c: 解析内嵌 srmodels.bin
    fn shim_srmodel_init(root: *const u8, len: usize);
}

impl WakeNetSource {
    /// 创建唤醒词检测器 ("你好小智")
    ///
    /// `gain_percent`: 送 MFCC 前的软件增益。当前调用方输入原始 2 麦交织
    /// 采样 (不经 AFE AGC), 直连麦克风用 600; 若改喂已归一化电平可用 100。
    /// 失败时返回原因字符串, 调用方应回退到能量 VAD。
    pub fn new(gain_percent: i32) -> Result<WakeNetSource, &'static str> {
        unsafe {
            info!("wakenet: srmodel 注入 ({}B)...", SRMODELS_BIN.len());
            shim_srmodel_init(SRMODELS_BIN.as_ptr(), SRMODELS_BIN.len());
            let iface = esp_wn_handle_from_name(MODEL_NAME.as_ptr());
            if iface.is_null() {
                defmt::error!(
                    "wakenet: tts 模型名未找到 (wn9_nihaoxiaozhi_tts), 可回退 srmodels.bin"
                );
                return Err("wakenet tts 模型名未找到");
            }
            let model = ((*iface).create)(MODEL_NAME.as_ptr(), DET_MODE_95);
            if model.is_null() {
                defmt::error!(
                    "wakenet: tts 模型创建失败 (内存不足或 2CH 模式不支持), 可回退 srmodels.bin"
                );
                return Err("wakenet tts 模型创建失败");
            }
            let chunk = ((*iface).get_samp_chunksize)(model);
            let channels = ((*iface).get_channel_num)(model);
            let rate = ((*iface).get_samp_rate)(model);
            // 真机双麦电平较低: 降到模型允许的下限 0.5 (原 0.631)
            let words = ((*iface).get_word_num)(model).max(0);
            for i in 1..=words {
                let th = ((*iface).get_det_threshold)(model, i);
                let ret = ((*iface).set_det_threshold)(model, 0.50, i);
                info!(
                    "wakenet: word {} threshold {}/1000 -> 500 ret={}",
                    i,
                    (th * 1000.0) as i32,
                    ret
                );
            }
            if chunk <= 0 || !(1..=3).contains(&channels) {
                // 只支持 1/2/3 通道; 2ch 模式下模型应返回 2
                ((*iface).destroy)(model);
                return Err("wakenet 通道数不支持");
            }
            let chunk = chunk as usize;
            let channels = channels as usize;

            info!(
                "wakenet: chunk={} ch={} rate={} block={}",
                chunk,
                channels,
                rate,
                chunk * channels
            );
            Ok(WakeNetSource {
                ctx: Ctx {
                    iface,
                    model,
                    block: chunk * channels,
                    channels,
                    chunk,
                },
                // 预留 2 个检测块, 覆盖 60ms 输入帧 (960 样本) 的跨块余量
                buf: alloc::vec::Vec::with_capacity(chunk * channels * 2),
                frames_fed: 0,
                gain_percent,
                last_r: 0,
                last_gain: gain_percent,
            })
        }
    }

    /// 每次 detect 需要的样本数 (chunk × channels)
    pub fn block_samples(&self) -> usize {
        self.ctx.block
    }

    /// 模型期望的通道数 (2CH 模式下应为 2)
    pub fn channels(&self) -> usize {
        self.ctx.channels
    }

    /// 检测块粒度与通道数 (诊断)
    pub fn chunk_info(&self) -> (usize, usize) {
        (self.ctx.chunk, self.ctx.channels)
    }

    /// 调整软件增益 (%): 原始单麦用 1500, AFE (AGC 归一化) 输出用 100
    pub fn set_gain_percent(&mut self, gain_percent: i32) {
        self.gain_percent = gain_percent.max(1);
    }
}

/// WakeNet 检测器: 内部缓冲到 block 粒度后逐块送检
pub struct WakeNetSource {
    ctx: Ctx,
    buf: alloc::vec::Vec<i16>,
    frames_fed: usize,
    gain_percent: i32,
    /// 上一块的 detect 返回值 (诊断: 区分 0=未命中 / -1=通道校验 / 1=命中)
    last_r: i32,
    /// 上一块实际使用增益 (峰值限制后; 诊断近场大声是否触发降增益)
    last_gain: i32,
}

impl WakeWordSource for WakeNetSource {
    fn detect(&mut self, pcm: &[i16]) -> bool {
        // 原始样本入队; 增益与去 DC 都在模型 block 粒度做, 避免调用方
        // 用 960/其它块长时改变直流基准 (实测会导致同一段语音检出结果不同)。
        self.buf.reserve(pcm.len());
        self.buf.extend_from_slice(pcm);
        let block = self.ctx.block;
        let mut hit = false;
        let mut consumed = 0usize;
        while self.buf.len() - consumed >= block {
            let frame = &mut self.buf[consumed..consumed + block];
            // 条件化前的原始统计: 近场削顶排查必须看这一组 (条件化后 mean 恒 0)
            #[cfg(feature = "debug-diag")]
            let (raw_mean, raw_peak) = {
                let (m, _) = xiaozhi_core::audio::capture::level_stats(frame);
                let peak = frame
                    .iter()
                    .map(|&s| (s as i32 - m).abs())
                    .max()
                    .unwrap_or(0);
                (m, peak)
            };
            // 固定 block 粒度: 去 DC + 峰值限制增益 (纯逻辑在 core, host 可测)。
            // 旧实现 15x 后 clamp + i16 wrapping_sub 去 DC: 大声近场说话时
            // 削顶块会回绕折叠 (真机日志 mean=±8000 / rms≈18000), 模型不命中。
            let gain = xiaozhi_core::audio::capture::condition_wakenet_block(
                frame,
                self.gain_percent,
                30_000,
            );
            self.last_gain = gain;
            self.frames_fed += 1;
            #[cfg(feature = "debug-diag")]
            if self.frames_fed == 1 || self.frames_fed.is_multiple_of(50) {
                let (m, rms) = xiaozhi_core::audio::capture::level_stats(frame);
                info!(
                    "wakenet: in raw_mean={} raw_peak={} -> mean={} rms={} gain={}%->{}% last_r={} frame={}",
                    raw_mean,
                    raw_peak,
                    m,
                    rms,
                    self.gain_percent,
                    self.last_gain,
                    self.last_r,
                    self.frames_fed
                );
            }
            unsafe {
                let r = ((*self.ctx.iface).detect)(self.ctx.model, frame.as_mut_ptr());
                self.last_r = r;
                if r != 0 {
                    info!("wakenet: detect r={} frame={}", r, self.frames_fed);
                }
                if r == WN_DETECTED {
                    hit = true;
                }
            }
            consumed += block;
        }
        if consumed > 0 {
            self.buf.drain(..consumed);
        }
        if hit {
            // 该模型在 detect 命中后调用 clean 会触发崩溃; 本地残留清空,
            // 模型状态由上层冷却窗口避免连续触发。
            self.buf.clear();
        }
        hit
    }
}

impl Drop for WakeNetSource {
    fn drop(&mut self) {
        unsafe {
            ((*self.ctx.iface).destroy)(self.ctx.model);
        }
    }
}

// ---------------------------------------------------------------------------
// esp-sr 堆垫片: heap_caps_malloc(.., MALLOC_CAP_SPIRAM) → PSRAM
// ---------------------------------------------------------------------------

/// ESP-IDF `MALLOC_CAP_SPIRAM` (1 << 10): esp-sr 用它把模型权重放 PSRAM
const MALLOC_CAP_SPIRAM: u32 = 1 << 10;

/// 从外部 RAM (PSRAM) 分配, 布局与 esp-alloc 的 C `malloc` 一致:
/// 指针前 4 字节保存总大小, 因此 C 侧 `free()` 可直接释放本函数返回的块。
///
/// # Safety
/// 返回指针由 C 侧 `free()` 释放; 不检查栈上逃逸。
#[unsafe(no_mangle)]
pub extern "C" fn shim_psram_malloc(size: usize) -> *mut u8 {
    let Some(total) = size.checked_add(4) else {
        return ptr::null_mut();
    };
    let Ok(layout) = Layout::from_size_align(total, 4) else {
        return ptr::null_mut();
    };
    // SAFETY: 非零布局, 分配块由 C 侧 free 释放
    let p =
        unsafe { esp_alloc::HEAP.alloc_caps(esp_alloc::MemoryCapability::External.into(), layout) };
    if p.is_null() {
        return p;
    }
    // SAFETY: p 至少 total 字节可写, 与 esp-alloc::malloc 头部语义一致
    unsafe {
        *(p as *mut usize) = total;
        p.add(4)
    }
}

/// C 垫片入口: 按 caps 选择 PSRAM / 内部堆
///
/// 仅 SPIRAM 请求走外部 RAM; 其他 caps (含 DMA 内部) 维持 esp-alloc 默认路由。
#[unsafe(no_mangle)]
pub extern "C" fn shim_heap_caps_alloc(size: usize, caps: u32, zero: bool) -> *mut u8 {
    if caps & MALLOC_CAP_SPIRAM == 0 {
        // 非 PSRAM: 交回 C 侧 malloc/calloc (esp-alloc compat)
        return ptr::null_mut();
    }
    let p = shim_psram_malloc(size);
    if zero && !p.is_null() {
        // SAFETY: p 指向 size 字节可写块
        unsafe { ptr::write_bytes(p, 0, size) };
    }
    p
}

/// 启动期自检: 创建检测器 → 喂静音帧 → clean → 销毁
///
/// 每步打日志 + 指针值: 崩溃时最后一条日志即出事的具体 C 调用。
/// (release 构建不要常驻调用, 否则模型会被加载两次)
pub fn wakenet_selftest() -> Result<(), &'static str> {
    info!("wakenet: 模型包 {}B 注入", SRMODELS_BIN.len());
    let mut src = WakeNetSource::new(DEFAULT_GAIN_PERCENT)?;
    info!(
        "wakenet: iface=0x{:x} model=0x{:x} chunk={} ch={}",
        src.ctx.iface as usize, src.ctx.model as usize, src.ctx.chunk, src.ctx.channels
    );
    // 验证 model_info 解析 (srmodel_list_t 布局 + _MODEL_INFO_ 提取)
    unsafe {
        let models = get_static_srmodels();
        let words = esp_srmodel_get_wake_words(models, MODEL_NAME.as_ptr());
        if words.is_null() {
            warn!("wakenet: 唤醒词解析失败 (model_info 缺失)");
        } else {
            let s = core::ffi::CStr::from_ptr(words);
            info!("wakenet: 唤醒词 {}", s.to_str().unwrap_or("?"));
        }
    }
    let silence = [0i16; 512];
    info!("wakenet: 静音检测...");
    let hit = src.detect(&silence);
    info!(
        "wakenet: 静音检测完成 hit={} 累计 {} 块",
        hit, src.frames_fed
    );
    if hit {
        warn!("wakenet: 静音帧误触发 (模型状态异常?)");
        return Err("静音帧误触发");
    }
    Ok(())
}
