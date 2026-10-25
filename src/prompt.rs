//! 本地提示音播放器
//!
//! 资源为 16kHz 单声道 s16le（`assets/*.raw`）。空闲提示与唤醒应答由
//! `tools/gen_prompt.sh` 合成；低电/错误/提醒/成功音由 C++ 参考工程的
//! OGG 资源转码而来（`tools/convert_prompts.sh`）。
//! 播放时转成立体声写入 I2S TX 环形，采用**非阻塞分片**：每轮只往环形
//! 空闲区推数据，绝不阻塞音频主循环。

use crate::bsp::components::Audio;

/// 空闲提示音 PCM（单声道 i16 little-endian）
pub const IDLE_PROMPT: &[u8] = include_bytes!("../assets/idle_prompt.raw");
/// 语音唤醒应答「我在」
pub const WAKE_ACK: &[u8] = include_bytes!("../assets/wake_ack.raw");
/// 低电量提示
pub const LOW_BATTERY: &[u8] = include_bytes!("../assets/low_battery.raw");
/// 错误提示（网络/协议异常）
pub const ERROR_SOUND: &[u8] = include_bytes!("../assets/exclamation.raw");
/// 服务器 alert 提醒
pub const ALERT_SOUND: &[u8] = include_bytes!("../assets/vibration.raw");
/// 启动就绪提示
pub const SUCCESS_SOUND: &[u8] = include_bytes!("../assets/success.raw");

/// 提示音片段
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clip {
    IdlePrompt,
    WakeAck,
    LowBattery,
    Error,
    Alert,
    Success,
}

impl Clip {
    const fn pcm(self) -> &'static [u8] {
        match self {
            Clip::IdlePrompt => IDLE_PROMPT,
            Clip::WakeAck => WAKE_ACK,
            Clip::LowBattery => LOW_BATTERY,
            Clip::Error => ERROR_SOUND,
            Clip::Alert => ALERT_SOUND,
            Clip::Success => SUCCESS_SOUND,
        }
    }
}

/// 提示音播放器（本地提示音共用；同一时刻只播一段，新片段覆盖旧片段）
pub struct PromptPlayer {
    pcm: &'static [u8],
    kind: Clip,
    pos: usize,
    active: bool,
}

impl PromptPlayer {
    pub const fn new() -> Self {
        Self {
            pcm: IDLE_PROMPT,
            kind: Clip::IdlePrompt,
            pos: 0,
            active: false,
        }
    }

    /// 播放指定片段（覆盖当前片段）
    pub fn play(&mut self, clip: Clip) {
        self.pcm = clip.pcm();
        self.kind = clip;
        self.pos = 0;
        self.active = true;
    }

    /// 播放空闲提示音
    pub fn play_idle(&mut self) {
        self.play(Clip::IdlePrompt);
    }

    /// 播放唤醒应答「我在」
    pub fn play_wake_ack(&mut self) {
        self.play(Clip::WakeAck);
    }

    /// 当前片段类型
    pub fn kind(&self) -> Clip {
        self.kind
    }

    /// 当前片段是否为唤醒应答
    pub fn is_wake_ack(&self) -> bool {
        self.kind == Clip::WakeAck
    }

    /// 当前片段是否为空闲提示音
    pub fn is_idle_prompt(&self) -> bool {
        self.kind == Clip::IdlePrompt
    }

    /// 中止播放（唤醒/进入会话时调用）
    pub fn stop(&mut self) {
        self.active = false;
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// 提示音时长（毫秒）
    pub fn duration_ms(&self) -> u64 {
        (self.pcm.len() as u64 / 2) * 1000 / 16_000
    }

    /// 把提示音泵入 I2S TX 环形（立体声化），返回 `true` 表示播放完成
    ///
    /// `stereo` 为临时缓冲（≥ 4 字节的整数倍），调用方通常复用解码缓冲。
    pub fn pump(&mut self, audio: &mut Audio, stereo: &mut [u8]) -> bool {
        if !self.active {
            return true;
        }
        let samples = self.pcm.len() / 2;
        loop {
            let free = audio.tx_free();
            if free < 4 {
                break;
            }
            if self.pos >= samples {
                self.active = false;
                break;
            }
            let frames = (free / 4).min(stereo.len() / 4).min(samples - self.pos);
            if frames == 0 {
                break;
            }
            for i in 0..frames {
                let o = (self.pos + i) * 2;
                let b = [self.pcm[o], self.pcm[o + 1]];
                let d = i * 4;
                stereo[d..d + 2].copy_from_slice(&b);
                stereo[d + 2..d + 4].copy_from_slice(&b);
            }
            let pushed = audio.push_avail(&stereo[..frames * 4]);
            if pushed == 0 {
                break;
            }
            self.pos += pushed / 4;
        }
        !self.active
    }
}

impl Default for PromptPlayer {
    fn default() -> Self {
        Self::new()
    }
}
