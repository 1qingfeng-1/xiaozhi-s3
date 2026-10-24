//! 播报播放管线：PSRAM 环形 PCM 队列 → I2S DMA 环形泵送
//!
//! 队列吸收服务器发帧抖动；DMA 环形始终填满（没数据补静音），
//! 避免欠载时 DMA 绕回重放旧音频。预缓冲/排空门控的纯逻辑在
//! [`xiaozhi_core::audio::playback`]。
//!
//! 队列必须放 PSRAM：内部 RAM 只有 128KB，128KB 队列会把内部堆吃满。

use core::alloc::Layout;

use xiaozhi_core::audio::playback::PlaybackFlow;

use crate::bsp::components::Audio;

/// PCM 队列上限 (立体声 16bit, 128KB ≈ 2s, 走 PSRAM)
pub const QUEUE_MAX: usize = 128 * 1024;
/// PSRAM 分配失败时的内部 RAM 回退容量 (0.5s)
const QUEUE_FALLBACK: usize = 32 * 1024;
/// 预缓冲阈值: 0.5s 立体声
pub const PREBUFFER: usize = 16_000 / 1000 * 500 * 4;
/// 单帧最大 PCM 字节数 (60ms 立体声 16bit)
pub const FRAME_MAX: usize = 960 * 4;
/// TtsStop 后等 DMA 播完的余量
const DRAIN_MS: u64 = 150;

/// 优先从 PSRAM (External) 分配；失败回退内部 RAM (容量缩半)
fn alloc_ring(size: usize) -> (&'static mut [u8], usize) {
    let layout = Layout::from_size_align(size, 4).expect("bad layout");
    // SAFETY: 布局非零且对齐; 分配块由本程序独占并常驻 (泄漏至重启)
    let ptr =
        unsafe { esp_alloc::HEAP.alloc_caps(esp_alloc::MemoryCapability::External.into(), layout) };
    if ptr.is_null() {
        let buf = alloc::vec![0u8; QUEUE_FALLBACK].leak();
        (buf, QUEUE_FALLBACK)
    } else {
        // SAFETY: alloc_caps 返回 size 字节可写内存; 必须清零
        unsafe {
            core::ptr::write_bytes(ptr, 0, size);
            (core::slice::from_raw_parts_mut(ptr, size), size)
        }
    }
}

/// 播放管线 (固定容量环形缓冲)
pub struct Playback {
    buf: &'static mut [u8],
    cap: usize,
    read: usize,
    write: usize,
    len: usize,
    flow: PlaybackFlow,
    underruns: u32,
    tx_bytes: u64,
}

impl Playback {
    pub fn new() -> Self {
        let (buf, cap) = alloc_ring(QUEUE_MAX);
        Self {
            buf,
            cap,
            read: 0,
            write: 0,
            len: 0,
            flow: PlaybackFlow::new(PREBUFFER, DRAIN_MS),
            underruns: 0,
            tx_bytes: 0,
        }
    }

    pub fn on_tts_start(&mut self) {
        self.flow.on_tts_start(self.len);
    }

    pub fn on_tts_stop(&mut self) {
        self.flow.on_tts_stop();
    }

    /// 队列是否还有空间放一帧（无空间时调用方应停止取包 → TCP 反压）
    pub fn has_space(&self) -> bool {
        self.len + FRAME_MAX <= self.cap
    }

    /// 写入 PCM，返回实际写入字节数（环形，自动回绕）
    pub fn push(&mut self, pcm: &[u8]) -> usize {
        let n = pcm.len().min(self.cap - self.len);
        let first = n.min(self.cap - self.write);
        self.buf[self.write..self.write + first].copy_from_slice(&pcm[..first]);
        if n > first {
            self.buf[..n - first].copy_from_slice(&pcm[first..n]);
        }
        self.write = (self.write + n) % self.cap;
        self.len += n;
        n
    }

    /// 队首连续可读切片（到环尾为止）
    fn front(&self) -> &[u8] {
        let end = if self.write > self.read {
            self.write
        } else {
            self.cap
        };
        &self.buf[self.read..end]
    }

    fn consume(&mut self, n: usize) {
        self.read = (self.read + n) % self.cap;
        self.len -= n;
    }

    /// 泵入 DMA 环形；返回 (是否欠载, 写入字节数)
    pub fn pump(&mut self, audio: &mut Audio, silence: &[u8], active: bool) -> (bool, usize) {
        let allow = self.flow.allow_audio(self.len);
        let mut underran = false;
        let mut total = 0usize;
        loop {
            let free = audio.tx_free();
            if free == 0 {
                break;
            }
            if active && allow && self.len == 0 && !self.flow.pending_stop() {
                underran = true;
            }
            let pushed = if !allow || self.len == 0 {
                audio.push_avail(&silence[..free.min(silence.len())])
            } else {
                let src = self.front();
                let n = free.min(src.len());
                let w = audio.push_avail(&src[..n]);
                self.consume(w);
                w
            };
            if pushed == 0 {
                break;
            }
            total += pushed;
        }
        if underran {
            self.underruns += 1;
        }
        self.tx_bytes += total as u64;
        (underran, total)
    }

    /// 排空到点则返回 true（调用方派发 TtsStop）
    pub fn poll_stop(&mut self, now_ms: u64) -> bool {
        self.flow.poll_drained(self.len == 0, now_ms)
    }

    pub fn take_prebuffer_done(&mut self) -> bool {
        self.flow.take_prebuffer_done()
    }

    pub fn queue_len(&self) -> usize {
        self.len
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    pub fn underruns(&self) -> u32 {
        self.underruns
    }

    pub fn tx_bytes(&self) -> u64 {
        self.tx_bytes
    }

    pub fn reset(&mut self) {
        self.read = 0;
        self.write = 0;
        self.len = 0;
        self.flow.reset();
    }
}
