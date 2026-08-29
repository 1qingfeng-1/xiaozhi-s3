//! libopus (vendor/libopus, 定点 C 库) 的最小安全封装
//!
//! 只暴露固件链路用到的 API: 16kHz 单声道、60ms 帧的编码与解码。
//! C 库内部状态经 esp-alloc 的 compat 堆函数 (malloc/free) 分配 ——
//! 编码器 ~25KB / 解码器 ~5KB, 落在内部 RAM。
//!
//! 采样率说明: 解码器的输出采样率与编码端无关 (libopus 内部重采样),
//! 服务器 24kHz 的 TTS 流直接以 16kHz 解码, 无需外部重采样器。

use core::ffi::c_int;
use core::ptr::NonNull;

use defmt::Format;

/// RFC 6716: 单个 opus 包最大字节数
pub const MAX_PACKET_BYTES: usize = 1276;

const OPUS_OK: c_int = 0;
const OPUS_APPLICATION_VOIP: c_int = 2048;
const OPUS_SET_COMPLEXITY_REQUEST: c_int = 4010;
const OPUS_SET_DTX_REQUEST: c_int = 4016;

/// C 侧不透明句柄 (Rust 侧零尺寸, 仅作指针类型)
#[repr(C)]
struct OpusEncoderCtx {
    _opaque: [u8; 0],
}
#[repr(C)]
struct OpusDecoderCtx {
    _opaque: [u8; 0],
}

unsafe extern "C" {
    fn opus_encoder_create(
        fs: c_int,
        channels: c_int,
        application: c_int,
        error: *mut c_int,
    ) -> *mut OpusEncoderCtx;
    fn opus_encoder_ctl(st: *mut OpusEncoderCtx, request: c_int, ...) -> c_int;
    fn opus_encoder_destroy(st: *mut OpusEncoderCtx);
    fn opus_encode(
        st: *mut OpusEncoderCtx,
        pcm: *const i16,
        frame_size: c_int,
        data: *mut u8,
        max_data_bytes: c_int,
    ) -> c_int;

    fn opus_decoder_create(fs: c_int, channels: c_int, error: *mut c_int) -> *mut OpusDecoderCtx;
    fn opus_decoder_destroy(st: *mut OpusDecoderCtx);
    fn opus_decode(
        st: *mut OpusDecoderCtx,
        data: *const u8,
        len: c_int,
        pcm: *mut i16,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
}

/// libopus 错误码 (负值), defmt 打印用
#[derive(Debug, Clone, Copy, Format, PartialEq, Eq)]
pub struct OpusError(pub c_int);

impl OpusError {
    fn describe(self) -> &'static str {
        match self.0 {
            -1 => "参数无效",
            -2 => "缓冲区过小",
            -3 => "内部错误",
            -4 => "压缩包损坏",
            -5 => "未实现",
            -6 => "状态无效",
            -7 => "内存分配失败",
            _ => "未知错误",
        }
    }
}

/// opus 编码器 (RAII: Drop 时销毁 C 侧状态)
pub struct OpusEncoder {
    st: NonNull<OpusEncoderCtx>,
    frame_samples: usize,
}

impl OpusEncoder {
    /// 创建单声道 VoIP 编码器, 并对齐 C++ 参考实现配置 (complexity 0 + DTX)
    pub fn new_voip_mono(sample_rate: u32, frame_ms: u32) -> Result<Self, OpusError> {
        let mut err = OPUS_OK;
        // SAFETY: C 库入口, 结果经 err 返回; 非 OK 时指针为空不使用
        let st = unsafe {
            opus_encoder_create(sample_rate as c_int, 1, OPUS_APPLICATION_VOIP, &mut err)
        };
        if err != OPUS_OK || st.is_null() {
            return Err(OpusError(if err == OPUS_OK { -3 } else { err }));
        }
        // SAFETY: 上方已排除 null
        let st = unsafe { NonNull::new_unchecked(st) };
        let enc = Self {
            st,
            frame_samples: (sample_rate / 1000 * frame_ms) as usize,
        };
        // SAFETY: 指针来自 create 成功返回; DTX 在极老版本可能不支持, 失败不致命
        unsafe {
            opus_encoder_ctl(st.as_ptr(), OPUS_SET_COMPLEXITY_REQUEST, 0);
            opus_encoder_ctl(st.as_ptr(), OPUS_SET_DTX_REQUEST, 1);
        }
        Ok(enc)
    }

    /// 一帧的样本数 (采样率 × 帧长)
    pub fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    /// 编码一帧 PCM, 返回压缩字节数
    ///
    /// `pcm` 长度必须等于 [`frame_samples`](Self::frame_samples);
    /// `out` 至少 [`MAX_PACKET_BYTES`] 字节。
    pub fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize, OpusError> {
        assert_eq!(pcm.len(), self.frame_samples, "编码帧样本数不匹配");
        assert!(out.len() >= MAX_PACKET_BYTES, "输出缓冲过小");
        // SAFETY: 指针有效性由调用方缓冲保证, 长度经断言校验
        let n = unsafe {
            opus_encode(
                self.st.as_ptr(),
                pcm.as_ptr(),
                self.frame_samples as c_int,
                out.as_mut_ptr(),
                out.len() as c_int,
            )
        };
        if n < 0 {
            Err(OpusError(n))
        } else {
            Ok(n as usize)
        }
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        // SAFETY: 仅在 Drop 中执行一次销毁, 指针此后不再使用
        unsafe { opus_encoder_destroy(self.st.as_ptr()) }
    }
}

/// opus 解码器 (RAII: Drop 时销毁 C 侧状态)
pub struct OpusDecoder {
    st: NonNull<OpusDecoderCtx>,
}

impl OpusDecoder {
    /// 创建单声道解码器, 输出采样率可与编码端不同 (内部自动重采样)
    pub fn new_mono(sample_rate: u32) -> Result<Self, OpusError> {
        let mut err = OPUS_OK;
        // SAFETY: 同编码器
        let st = unsafe { opus_decoder_create(sample_rate as c_int, 1, &mut err) };
        if err != OPUS_OK || st.is_null() {
            return Err(OpusError(if err == OPUS_OK { -3 } else { err }));
        }
        Ok(Self {
            // SAFETY: 上方已排除 null
            st: unsafe { NonNull::new_unchecked(st) },
        })
    }

    /// 解码一个压缩包, 返回输出样本数 (单声道 i16)
    ///
    /// `out` 需能容纳帧长 (16kHz 下 60ms = 960 样本, 留余量更好)。
    pub fn decode(&mut self, packet: &[u8], out: &mut [i16]) -> Result<usize, OpusError> {
        // SAFETY: packet/out 均为调用方持有缓冲; 空包也合法 (丢包补偿)
        let n = unsafe {
            opus_decode(
                self.st.as_ptr(),
                packet.as_ptr(),
                packet.len() as c_int,
                out.as_mut_ptr(),
                out.len() as c_int,
                0,
            )
        };
        if n < 0 {
            Err(OpusError(n))
        } else {
            Ok(n as usize)
        }
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        // SAFETY: 同编码器
        unsafe { opus_decoder_destroy(self.st.as_ptr()) }
    }
}

impl From<OpusError> for &'static str {
    fn from(e: OpusError) -> &'static str {
        e.describe()
    }
}
