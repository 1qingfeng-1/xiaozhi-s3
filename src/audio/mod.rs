//! 音频编解码 — libopus 定点 C 库封装 (M4.2 opus 集成)

pub mod opus;
pub mod playback;

/// 关键词唤醒 (feature `wakenet`): 链接级验证待完成, 见 vendor/esp-sr/README.md
#[cfg(feature = "wakenet")]
pub mod wake_net;
