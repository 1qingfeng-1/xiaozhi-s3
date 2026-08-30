//! WakeNet 关键词唤醒 (feature `wakenet`, esp-sr v1.9.5 预编译库 FFI)
//!
//! # 状态 (M7)
//!
//! 代码就绪但**默认未启用**: 链接层尚差运行时垫片的最后验证
//! (esp_kiss_fftndr/vvadd_shift_1_int16 的语义、mfcc 任务行为),
//! 无真机条件下无法闭环, 见 `vendor/esp-sr/README.md` 与 ROADMAP。
//! 启用后与 [`crate::audio::wake::VadWakeDetector`] 同接口切换。
//!
//! # 内存
//!
//! 模型 (srmodels.bin ~284KB hufzip 压缩) 以 `include_bytes!` 嵌入
//! flash rodata, 经 `srmodel_load` 原地解析 (官方 RAM 加载路径),
//! 无需模型分区; 推理状态 ~80KB 落内部堆。

#![allow(dead_code)]

use defmt::info;

use xiaozhi_core::audio::wake::WakeWordSource;

/// 检测模式 (对齐 C++ afe_wake_word.cc: DET_MODE_3CH_95 最激进)
const DET_MODE: i32 = 5; // DET_MODE_3CH_95

/// wakenet 检测结论
const WN_NO_DETECT: i32 = 0;
const WN_DETECTED: i32 = 1;

/// C 侧不透明模型句柄
#[repr(C)]
pub struct WnModelData {
    _opaque: [u8; 0],
}

/// esp_wn_iface_t 函数表 (esp_wn_iface.h; 仅声明用到的成员)
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
    reset_det_threshold: unsafe extern "C" fn(*mut WnModelData) -> i32,
    get_det_threshold: unsafe extern "C" fn(*mut WnModelData, word_index: i32) -> f32,
    get_triggered_channel: unsafe extern "C" fn(*mut WnModelData) -> i32,
    get_vol_gain: unsafe extern "C" fn(*mut WnModelData, target_db: f32) -> f32,
    clean: unsafe extern "C" fn(*mut WnModelData),
    destroy: unsafe extern "C" fn(*mut WnModelData),
    // detect 系列位于表尾, 依据 esp_wn_iface.h 完整布局补齐
    detect: unsafe extern "C" fn(*mut WnModelData, samples: *mut i16) -> i32,
}

unsafe extern "C" {
    fn esp_wn_handle_from_name(model_name: *const u8) -> *const WnIface;
}

/// C 侧模型上下文 (嵌入的 srmodels.bin 由 model_path shim 解析)
struct Ctx {
    iface: *const WnIface,
    model: *mut WnModelData,
    chunk: usize,
}

// 嵌入的模型包 (hufzip 压缩, 官方 pack_model.py 打包, ~284KB flash rodata)
static SRMODELS_BIN: &[u8] = include_bytes!("../../vendor/esp-sr/model/srmodels.bin");
static MODEL_NAME: &[u8] = b"wn9_nihaoxiaozhi\0";

unsafe extern "C" {
    /// vendor/esp-sr/src/model_path_shim.c: 解析内嵌 srmodels.bin
    fn shim_srmodel_init(root: *const u8, len: usize);
}

/// 创建唤醒词检测器 ("你好小智")
pub fn WakeNetSource_new() -> Result<WakeNetSource, &'static str> {
    unsafe {
        info!("wakenet: srmodel 注入...");
        shim_srmodel_init(SRMODELS_BIN.as_ptr(), SRMODELS_BIN.len());
        info!("wakenet: 获取函数表...");
        let iface = esp_wn_handle_from_name(MODEL_NAME.as_ptr());
        if iface.is_null() {
            return Err("wakenet 句柄获取失败");
        }
        info!("wakenet: create ({}B 内存请求)...", 0);
        let model = ((*iface).create)(MODEL_NAME.as_ptr(), DET_MODE);
        if model.is_null() {
            return Err("wakenet create 失败 (内存不足?)");
        }
        let chunk = ((*iface).get_samp_chunksize)(model) as usize;
        let rate = ((*iface).get_samp_rate)(model) as usize;
        if chunk == 0 {
            ((*iface).destroy)(model);
            return Err("wakenet chunk size 无效");
        }
        info!("wakenet: chunk={} rate={}", chunk, rate);
        Ok(WakeNetSource {
            ctx: Ctx {
                iface,
                model,
                chunk,
            },
            buf: alloc::vec::Vec::new(),
            frames_fed: 0,
        })
    }
}

/// WakeNet 检测器: 内部缓冲到 chunk 粒度后逐块送检
pub struct WakeNetSource {
    ctx: Ctx,
    buf: alloc::vec::Vec<i16>,
    frames_fed: usize,
}

impl WakeWordSource for WakeNetSource {
    fn detect(&mut self, pcm: &[i16]) -> bool {
        self.buf.extend_from_slice(pcm);
        let chunk = self.ctx.chunk;
        let mut hit = false;
        while self.buf.len() >= chunk {
            let frame: alloc::vec::Vec<i16> = self.buf.drain(..chunk).collect();
            self.frames_fed += 1;
            unsafe {
                let r = ((*self.ctx.iface).detect)(self.ctx.model, frame.as_ptr() as *mut i16);
                if r == WN_DETECTED {
                    hit = true;
                }
            }
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

pub const WN_DETECTED_CONST: i32 = WN_DETECTED;

/// 启动期自检: 创建检测器 → 喂静音帧 → 销毁 (强制链接全部符号)
///
/// 每步打日志 + 指针值: 崩溃时最后一条日志即出事的具体 C 调用。
pub fn wakenet_selftest() -> Result<(), &'static str> {
    info!("wakenet: 模型包 {}B 注入", SRMODELS_BIN.len());
    let mut src = WakeNetSource_new()?;
    info!(
        "wakenet: iface=0x{:x} model=0x{:x} chunk={}",
        src.ctx.iface as usize, src.ctx.model as usize, src.ctx.chunk
    );
    let silence = [0i16; 512];
    info!("wakenet: 首帧检测...");
    let _ = src.detect(&silence);
    info!("wakenet: 首帧 OK, 共 {} 帧", src.frames_fed);
    Ok(())
}
