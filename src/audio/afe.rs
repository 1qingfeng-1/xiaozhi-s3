//! AFE 信号前端 — esp-sr v1.9.5 算法层 (NS + AGC + VAD)
//!
//! 不链接 `libesp_audio_front_end.a` 的 `afe_se_task`（依赖 FreeRTOS
//! 任务/事件组/阻塞环形缓冲，esp-hal/esp-rtos 下没有对应运行时），
//! 而是直接调用同一套同步算法：
//!
//! - `ns_pro_create/ns_process`：谱减法噪声抑制（10ms / 16kHz）
//! - `esp_agc_open/esp_agc_process`：WebRTC AGC（10ms / 16kHz）
//! - `vad_create/vad_process`：WebRTC VAD（10ms / 16kHz）
//!
//! **AEC 暂不启用**：ES7210 当前是双麦非 TDM，没有喇叭回采参考通道；
//! 算法层的 `esp_aec3_*` 需要 ref 输入，等硬件 TDM 参考通道接通后再加。
//!
//! 算法状态（NS/AGC/VAD 内部 malloc）由 `build.rs` 的 objcopy 补丁
//! 重命名为 `esp_sr_*`，统一落 PSRAM（内部堆只有 128KB）。

use core::ffi::c_void;

use defmt::info;

/// 10ms @ 16kHz
const FRAME_SAMPLES: usize = 160;
const SAMPLE_RATE: i32 = 16000;

type NsHandle = *mut c_void;
type VadHandle = *mut c_void;
type AgcHandle = *mut c_void;

unsafe extern "C" {
    fn ns_pro_create(frame_length: i32, mode: i32, sample_rate: i32) -> NsHandle;
    fn ns_process(inst: NsHandle, indata: *mut i16, outdata: *mut i16);
    fn ns_destroy(inst: NsHandle);

    fn vad_create(vad_mode: i32) -> VadHandle;
    fn vad_process(inst: VadHandle, data: *mut i16, sample_rate_hz: i32, one_frame_ms: i32) -> i32;
    fn vad_destroy(inst: VadHandle);

    fn esp_agc_open(agc_mode: i32, sample_rate: i32) -> AgcHandle;
    fn set_agc_config(handle: AgcHandle, gain_db: i32, limiter_enable: i32, target_level_dbfs: i32);
    fn esp_agc_process(
        handle: AgcHandle,
        in_pcm: *mut i16,
        out_pcm: *mut i16,
        frame_size: i32,
        sample_rate: i32,
    ) -> i32;
    fn esp_agc_close(handle: AgcHandle);
}

/// AFE 实例：NS → AGC → VAD，按 10ms 帧处理
pub struct Afe {
    ns: NsHandle,
    agc: AgcHandle,
    vad: VadHandle,
    /// 跨调用残留（不足一帧的样本）
    carry: [i16; FRAME_SAMPLES],
    carry_len: usize,
    frame_in: [i16; FRAME_SAMPLES],
    ns_out: [i16; FRAME_SAMPLES],
    agc_out: [i16; FRAME_SAMPLES],
    vad_last: bool,
    /// 本次 process 中 VAD 判为语音的时长 (每帧 10ms)
    vad_ms: u32,
}

impl Afe {
    /// 创建 NS/AGC/VAD；任一失败返回 Err，调用方回退原始音频
    pub fn new() -> Result<Self, &'static str> {
        // SAFETY: 算法层句柄由 Drop 统一销毁
        unsafe {
            let ns = ns_pro_create(10, 2, SAMPLE_RATE); // 10ms, 激进降噪
            if ns.is_null() {
                return Err("ns_pro_create 失败");
            }
            let agc = esp_agc_open(2, SAMPLE_RATE); // WebRTC 自适应数字 AGC
            if agc.is_null() {
                ns_destroy(ns);
                return Err("esp_agc_open 失败");
            }
            // 15dB 压缩增益 / 限幅 / 目标 -3dBFS (后续真机标定)
            set_agc_config(agc, 15, 1, 3);
            let vad = vad_create(3); // VAD_MODE_3
            if vad.is_null() {
                ns_destroy(ns);
                esp_agc_close(agc);
                return Err("vad_create 失败");
            }
            info!("AFE: NS/AGC/VAD 就绪 (10ms/16k, 状态在 PSRAM)");
            Ok(Self {
                ns,
                agc,
                vad,
                carry: [0; FRAME_SAMPLES],
                carry_len: 0,
                frame_in: [0; FRAME_SAMPLES],
                ns_out: [0; FRAME_SAMPLES],
                agc_out: [0; FRAME_SAMPLES],
                vad_last: false,
                vad_ms: 0,
            })
        }
    }

    /// 处理任意长度 mono PCM，写入 `out`，返回输出样本数
    ///
    /// 不足一帧的尾部会保留到下次调用，因此 `n <= input.len()`。
    /// `out` 必须至少与 `input` 等长。
    pub fn process(&mut self, input: &[i16], out: &mut [i16]) -> usize {
        self.vad_last = false;
        self.vad_ms = 0;
        let mut in_pos = 0usize;
        let mut out_pos = 0usize;

        // 先补齐上次残留的帧
        if self.carry_len > 0 {
            let need = FRAME_SAMPLES - self.carry_len;
            let take = need.min(input.len());
            self.frame_in[..self.carry_len].copy_from_slice(&self.carry[..self.carry_len]);
            self.frame_in[self.carry_len..self.carry_len + take].copy_from_slice(&input[..take]);
            in_pos += take;
            if self.carry_len + take < FRAME_SAMPLES {
                self.carry[self.carry_len..self.carry_len + take].copy_from_slice(&input[..take]);
                self.carry_len += take;
                return 0;
            }
            self.carry_len = 0;
            unsafe { self.process_frame(out, &mut out_pos) };
        }

        // 完整帧
        while input.len() - in_pos >= FRAME_SAMPLES {
            self.frame_in
                .copy_from_slice(&input[in_pos..in_pos + FRAME_SAMPLES]);
            in_pos += FRAME_SAMPLES;
            unsafe { self.process_frame(out, &mut out_pos) };
        }

        // 保存不足一帧的尾部
        let rem = input.len() - in_pos;
        if rem > 0 {
            self.carry[..rem].copy_from_slice(&input[in_pos..]);
        }
        self.carry_len = rem;
        out_pos
    }

    /// 本次 `process` 是否检测到语音 (任一 10ms 帧命中)
    pub fn vad_active(&self) -> bool {
        self.vad_last
    }

    /// 本次 `process` 中 VAD 判为语音的时长 (ms)
    pub fn voiced_ms(&self) -> u32 {
        self.vad_ms
    }

    /// 处理一帧 10ms：NS → AGC → VAD，结果写入 out
    ///
    /// # Safety
    /// 句柄在 new() 中校验非空；数组长度固定为 FRAME_SAMPLES。
    unsafe fn process_frame(&mut self, out: &mut [i16], out_pos: &mut usize) {
        unsafe {
            ns_process(
                self.ns,
                self.frame_in.as_mut_ptr(),
                self.ns_out.as_mut_ptr(),
            );
            // VAD 必须在 AGC 之前: AGC 会把环境噪声抬到接近满幅,
            // 在 AGC 之后跑能量/帧 VAD 会造成误唤醒与 Listening 永不收尾。
            let v = vad_process(self.vad, self.ns_out.as_mut_ptr(), SAMPLE_RATE, 10);
            let agc_ok = esp_agc_process(
                self.agc,
                self.ns_out.as_mut_ptr(),
                self.agc_out.as_mut_ptr(),
                FRAME_SAMPLES as i32,
                SAMPLE_RATE,
            );
            // AGC 失败时退回 NS 输出，保证音频不断流
            let src = if agc_ok == 0 {
                self.agc_out.as_ptr()
            } else {
                self.ns_out.as_ptr()
            };
            if v == 1 {
                self.vad_last = true;
                self.vad_ms += 10;
            }
            let room = out.len().saturating_sub(*out_pos);
            let n = FRAME_SAMPLES.min(room);
            out[*out_pos..*out_pos + n].copy_from_slice(core::slice::from_raw_parts(src, n));
            *out_pos += n;
        }
    }
}

impl Drop for Afe {
    fn drop(&mut self) {
        unsafe {
            ns_destroy(self.ns);
            esp_agc_close(self.agc);
            vad_destroy(self.vad);
        }
    }
}

/// 启动期自检: 创建 → 处理 60ms 静音 → 销毁 (debug-diag 用)
pub fn afe_selftest() -> Result<(), &'static str> {
    let mut afe = Afe::new()?;
    let input = [0i16; 960];
    let mut out = [0i16; 960];
    let n = afe.process(&input, &mut out);
    info!("AFE 自检: out={} vad={}", n, afe.vad_active());
    if n != input.len() {
        return Err("AFE 输出长度异常");
    }
    Ok(())
}
