//! 播报播放管线：软件 PCM 队列 → I2S DMA 环形泵送
//!
//! 队列吸收服务器发帧抖动；DMA 环形始终填满（没数据补静音），
//! 避免欠载时 DMA 绕回重放旧音频。预缓冲/排空门控的纯逻辑在
//! [`xiaozhi_core::audio::playback`]。

use alloc::collections::VecDeque;

use xiaozhi_core::audio::playback::PlaybackFlow;

use crate::bsp::components::Audio;

/// PCM 队列上限 (立体声 16bit, 128KB ≈ 2s, 走 PSRAM)
///
/// 延迟上界 ≈ 预缓冲 + 队列 ≈ 2.5s; 再大只会拉长回复滞后。
pub const QUEUE_MAX: usize = 128 * 1024;
/// 预缓冲阈值: 0.5s 立体声
pub const PREBUFFER: usize = 16_000 / 1000 * 500 * 4;
/// 单帧最大 PCM 字节数 (60ms 立体声 16bit)
pub const FRAME_MAX: usize = 960 * 4;
/// TtsStop 后等 DMA 播完的余量
const DRAIN_MS: u64 = 150;

/// 播放管线
pub struct Playback {
    q: VecDeque<u8>,
    flow: PlaybackFlow,
    underruns: u32,
    tx_bytes: u64,
}

impl Playback {
    pub fn new() -> Self {
        Self {
            q: VecDeque::with_capacity(QUEUE_MAX),
            flow: PlaybackFlow::new(PREBUFFER, DRAIN_MS),
            underruns: 0,
            tx_bytes: 0,
        }
    }

    pub fn on_tts_start(&mut self) {
        self.flow.on_tts_start(self.q.len());
    }

    pub fn on_tts_stop(&mut self) {
        self.flow.on_tts_stop();
    }

    /// 队列是否还有空间放一帧（无空间时调用方应停止取包 → TCP 反压）
    pub fn has_space(&self) -> bool {
        self.q.len() + FRAME_MAX <= QUEUE_MAX
    }

    pub fn push(&mut self, pcm: &[u8]) {
        self.q.extend(pcm.iter().copied());
    }

    /// 泵入 DMA 环形；返回 (是否欠载, 写入字节数)
    pub fn pump(&mut self, audio: &mut Audio, silence: &[u8], active: bool) -> (bool, usize) {
        let allow = self.flow.allow_audio(self.q.len());
        let mut underran = false;
        let mut total = 0usize;
        loop {
            let free = audio.tx_free();
            if free == 0 {
                break;
            }
            if active && allow && self.q.is_empty() && !self.flow.pending_stop() {
                underran = true;
            }
            let pushed = if !allow || self.q.is_empty() {
                audio.push_avail(&silence[..free.min(silence.len())])
            } else {
                // 不调用 make_contiguous: 直接取队首连续分片, 避免 192KB 搬移
                let (front, back) = self.q.as_slices();
                let src = if front.is_empty() { back } else { front };
                let n = free.min(src.len());
                let w = audio.push_avail(&src[..n]);
                self.q.drain(..w);
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
        self.flow.poll_drained(self.q.is_empty(), now_ms)
    }

    pub fn take_prebuffer_done(&mut self) -> bool {
        self.flow.take_prebuffer_done()
    }

    pub fn queue_len(&self) -> usize {
        self.q.len()
    }

    pub fn underruns(&self) -> u32 {
        self.underruns
    }

    pub fn tx_bytes(&self) -> u64 {
        self.tx_bytes
    }

    pub fn reset(&mut self) {
        self.q.clear();
        self.flow.reset();
    }
}
