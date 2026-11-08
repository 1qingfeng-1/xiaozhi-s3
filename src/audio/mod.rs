//! 音频编解码 — libopus 定点 C 库封装 (M4.2 opus 集成)

pub mod frontend;
pub mod opus;
pub mod playback;
pub mod wake_source;

/// 关键词唤醒 (feature `wakenet`): esp-sr WakeNet「你好小智」
#[cfg(feature = "wakenet")]
pub mod wake_net;

/// AFE 信号前端 (feature `afe`): esp-sr NS/AGC/VAD 算法层
#[cfg(feature = "afe")]
pub mod afe;
