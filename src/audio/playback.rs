//! 播报播放管线：PSRAM 环形 PCM 队列 → I2S DMA 环形泵送
//!
//! 队列吸收服务器发帧抖动；DMA 环形始终填满（没数据补静音），
//! 避免欠载时 DMA 绕回重放旧音频。预缓冲/排空门控的纯逻辑在
//! [`xiaozhi_core::audio::playback`]。
//!
//! 队列必须放 PSRAM：内部 RAM 只有 128KB，128KB 队列会把内部堆吃满。

use core::alloc::Layout;

use xiaozhi_core::audio::playback::{PlaybackFlow, PlayoutTimestamps};
use xiaozhi_core::protocol::AecClock;

use crate::bsp::components::Audio;

/// PCM 队列上限 (立体声 16bit, 512KB ≈ 8s @64KB/s, 走 PSRAM)
///
/// 服务端 TTS 通常快于实时、成串下发; 3s 队列会在长句尾部反压到
/// UDP/downlink 并丢包。8s 队列留足突发吸收空间 (PSRAM 8MB, 代价可接受)。
pub const QUEUE_MAX: usize = 512 * 1024;
/// PSRAM 分配失败时的内部 RAM 回退容量 (≈0.5s)
const QUEUE_FALLBACK: usize = 32 * 1024;
/// 预缓冲阈值: 350ms 立体声 (64KB/s → 22.4KB)
///
/// 原 1.0s 让每个回复都多等约 1 秒 (用户反馈"回答偏慢")。350ms 足以
/// 吸收 WS 首包突发与服务器分句间隔; 真正做到"长句不断流"靠的是环形
/// 永不欠载 (见 `pump` 的欠载淡出), 而不是把预缓冲堆到 1 秒。
pub const PREBUFFER: usize = 16_000 / 1000 * 350 * 4;
/// 单帧最大 PCM 字节数 (60ms 立体声 16bit)
pub const FRAME_MAX: usize = 960 * 4;
/// TtsStop 后等 DMA 播完的余量: 必须 ≥ TX 环形时长 (8184B ≈128ms)
const DRAIN_MS: u64 = 200;
/// TX DMA 卡死兜底: 队列满时长 (≈3.07s) + TX 环 + 余量
const FORCE_MS: u64 = (QUEUE_MAX as u64 * 1000) / 64_000 + 1_000;
/// 欠载淡出长度上限 (4ms 立体声 16bit): 只覆盖断点, 不拖长静音
const SILENCE_CHUNK: usize = 16_000 / 1000 * 4 * 4;
/// 静音后恢复播放的淡入长度 (4ms): 消除 0→满幅阶跃
const FADE_IN_BYTES: usize = SILENCE_CHUNK;

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
    /// 服务器时间戳按**实际播出**进度入队 (服务端 AEC 对齐用)
    ts: PlayoutTimestamps,
    /// 上一块写入 DMA 的最后一个样本 (L/R 交错, 用于欠载淡出起点)
    last_l: i16,
    last_r: i16,
    /// 还需淡入的字节数 (欠载补静音后 >0, 音频恢复时递减)
    fade_in_bytes: usize,
}

impl Default for Playback {
    fn default() -> Self {
        Self::new()
    }
}

impl Playback {
    pub fn new() -> Self {
        let (buf, cap) = alloc_ring(QUEUE_MAX);
        // PSRAM 失败时回退 32KB: 预缓冲必须小于回退容量, 否则永远不开始播放
        let prebuffer = PREBUFFER.min(cap / 2);
        Self {
            buf,
            cap,
            read: 0,
            write: 0,
            len: 0,
            flow: PlaybackFlow::new(prebuffer, DRAIN_MS, FORCE_MS),
            underruns: 0,
            tx_bytes: 0,
            ts: PlayoutTimestamps::new(),
            last_l: 0,
            last_r: 0,
            fade_in_bytes: 0,
        }
    }

    pub fn on_tts_start(&mut self) {
        // 新回复开始: 上一轮若仍有残留 (例如服务器提前发 tts start),
        // 必须丢弃, 否则新旧音频会拼接成延迟/重复播放。
        if self.flow.pending_stop() || self.len > 0 {
            self.reset();
        }
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
    ///
    /// `timestamp` 为服务器帧时间戳 (v1/无时间戳传 0): 只有真正播出时才
    /// 交给服务端 AEC, 所以先随字节数入队, 由 [`Self::pump`] 按消费进度弹出。
    pub fn push(&mut self, pcm: &[u8], timestamp: u32) -> usize {
        let n = pcm.len().min(self.cap - self.len);
        let first = n.min(self.cap - self.write);
        self.buf[self.write..self.write + first].copy_from_slice(&pcm[..first]);
        if n > first {
            self.buf[..n - first].copy_from_slice(&pcm[first..n]);
        }
        self.write = (self.write + n) % self.cap;
        self.len += n;
        self.ts.push_frame(n, timestamp);
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
    ///
    /// 欠载时**不直接写零**: 波形被硬切会产生阶跃, 听感就是"啪"的破音
    /// (真机长句 `播报欠载` 连发时明显)。这里对欠载前最后 4ms 做线性淡出、
    /// 恢复后 4ms 淡入, 把断点变成可听感忽略的过零过渡。
    pub fn pump(
        &mut self,
        audio: &mut Audio,
        silence: &[u8],
        active: bool,
        clock: &mut AecClock,
    ) -> (bool, usize) {
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
                self.pump_silence(audio, silence, free)
            } else {
                self.pump_audio(audio, free, clock)
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

    /// 欠载/预缓冲期: 先补一段最短淡出, 其余全是静音
    ///
    /// 避免硬切产生破音, 又不至于把正常静音拖成一段长渐弱。同时标记
    /// 下一块数据需要淡入, 否则 0→满幅的阶跃仍是"啪"的一声
    /// (真机长句偶现短杂音的真正来源)。
    fn pump_silence(&mut self, audio: &mut Audio, silence: &[u8], free: usize) -> usize {
        let n = free.min(silence.len()).min(SILENCE_CHUNK);
        let (mut ll, mut rr) = (self.last_l, self.last_r);
        let mut buf = [0u8; SILENCE_CHUNK];
        Self::fade_out_to_zero(&mut buf[..n], &mut ll, &mut rr);
        self.last_l = ll;
        self.last_r = rr;
        self.fade_in_bytes = FADE_IN_BYTES;
        audio.push_avail(&buf[..n])
    }

    /// 推送一块真实音频; 静音恢复窗口内对开头做短淡入
    fn pump_audio(&mut self, audio: &mut Audio, free: usize, clock: &mut AecClock) -> usize {
        // 先读出会变异的字段, 避免与 `front()` 的只读借用冲突
        let fade_pending = self.fade_in_bytes;
        let src = self.front();
        let n = free.min(src.len());
        let tail = Self::tail_of(&src[..n]);
        let w = if fade_pending == 0 {
            audio.push_avail(&src[..n])
        } else {
            let len = n.min(SILENCE_CHUNK);
            let fade = fade_pending.min(len);
            let mut buf = [0u8; SILENCE_CHUNK];
            buf[..len].copy_from_slice(&src[..len]);
            Self::fade_in(&mut buf[..fade]);
            self.fade_in_bytes = fade_pending.saturating_sub(len);
            audio.push_avail(&buf[..len])
        };
        // 记录末尾样本, 供下一次欠载淡出使用
        self.last_l = tail.map_or(self.last_l, |(l, _)| l);
        self.last_r = tail.map_or(self.last_r, |(_, r)| r);
        self.consume(w);
        self.ts.consume(w, clock);
        w
    }

    /// 生成从上次播出电平线性淡到 0 的静音段 (L/R 交错)
    fn fade_out_to_zero(dst: &mut [u8], last_l: &mut i16, last_r: &mut i16) {
        let frames = dst.len() / 4;
        if frames == 0 {
            return;
        }
        for i in 0..frames {
            let t = (frames - i) as i32;
            let l = (*last_l as i32 * t / frames as i32) as i16;
            let r = (*last_r as i32 * t / frames as i32) as i16;
            dst[i * 4..i * 4 + 2].copy_from_slice(&l.to_le_bytes());
            dst[i * 4 + 2..i * 4 + 4].copy_from_slice(&r.to_le_bytes());
        }
        *last_l = 0;
        *last_r = 0;
    }

    /// 立体声缓冲开头做线性淡入 (0 → 原幅度)
    fn fade_in(dst: &mut [u8]) {
        let frames = dst.len() / 4;
        if frames == 0 {
            return;
        }
        for i in 0..frames {
            let g = (i as i32 * 1000) / frames as i32;
            for ch in 0..2 {
                let o = i * 4 + ch * 2;
                let s = i16::from_le_bytes([dst[o], dst[o + 1]]) as i32;
                let v = (s * g / 1000) as i16;
                dst[o..o + 2].copy_from_slice(&v.to_le_bytes());
            }
        }
    }

    /// 取一块立体声音频的末尾样本 (L, R)
    fn tail_of(src: &[u8]) -> Option<(i16, i16)> {
        if src.len() < 4 {
            return None;
        }
        let n = src.len();
        Some((
            i16::from_le_bytes([src[n - 4], src[n - 3]]),
            i16::from_le_bytes([src[n - 2], src[n - 1]]),
        ))
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
        self.ts.clear();
        self.flow.reset();
    }
}
