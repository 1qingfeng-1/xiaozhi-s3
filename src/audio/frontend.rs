//! 音频前端选择 — feature `afe` 时启用 NS/AGC/VAD，否则直通。
//!
//! 主循环只依赖这里的 `FrontEnd` API；关闭 feature 时零成本回退原始音频。

#[cfg(feature = "afe")]
pub type FrontEnd = crate::audio::afe::Afe;

/// 无 AFE 时的直通实现（仅用于保持接口一致；main 不会创建它）
#[cfg(not(feature = "afe"))]
pub struct FrontEnd;

#[cfg(not(feature = "afe"))]
impl FrontEnd {
    pub fn new() -> Result<Self, &'static str> {
        Err("afe feature 未启用")
    }

    pub fn process(&mut self, input: &[i16], out: &mut [i16]) -> usize {
        let n = input.len().min(out.len());
        out[..n].copy_from_slice(&input[..n]);
        n
    }

    pub fn vad_active(&self) -> bool {
        false
    }

    pub fn voiced_ms(&self) -> u32 {
        0
    }
}
