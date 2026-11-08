//! 唤醒检测器选择 — 默认能量 VAD; feature `wakenet` 时优先本地关键词。
//!
//! 统一成一种具体类型, 让 `app::run` 里的 `WakeEngine<S>` 不必为两种检测器
//! 各写一套主循环。WakeNet 创建失败 (PSRAM 不足/模型损坏) 时自动回退 VAD,
//! 保证设备仍可用。

#[cfg(feature = "wakenet")]
use defmt::warn;
use xiaozhi_core::audio::wake::{VadWakeDetector, WakeWordSource};

/// 唤醒检测器: 能量 VAD 或 WakeNet 关键词
pub enum WakeDetector {
    Vad(VadWakeDetector),
    /// 本地关键词检测 (feature `wakenet`)
    #[cfg(feature = "wakenet")]
    Net(crate::audio::wake_net::WakeNetSource),
}

impl WakeDetector {
    /// 创建检测器: 优先 WakeNet (feature 开启时), 失败回退能量 VAD
    ///
    /// `wn_gain_percent`: 送 WakeNet 的软件增益; AFE 开启时应传 100。
    pub fn new(vad_threshold: i16, wn_gain_percent: i32) -> Self {
        #[cfg(not(feature = "wakenet"))]
        let _ = wn_gain_percent;
        #[cfg(feature = "wakenet")]
        match crate::audio::wake_net::WakeNetSource::new(wn_gain_percent) {
            Ok(net) => {
                defmt::info!("唤醒源: WakeNet 关键词「你好小智」");
                return Self::Net(net);
            },
            Err(e) => warn!("WakeNet 初始化失败, 回退能量 VAD: {}", e),
        }
        defmt::info!("唤醒源: 能量 VAD");
        Self::Vad(VadWakeDetector::new(vad_threshold))
    }

    /// 动态阈值仅对能量 VAD 有意义 (WakeNet 阈值由模型/模式决定)
    pub fn set_threshold(&mut self, threshold: i16) {
        match self {
            Self::Vad(det) => det.set_threshold(threshold),
            #[cfg(feature = "wakenet")]
            Self::Net(_) => {},
        }
    }

    /// 是否本地关键词检测器: 命中即已确认关键词, 无需服务端 ASR 门控
    pub fn is_keyword(&self) -> bool {
        #[cfg(feature = "wakenet")]
        if matches!(self, Self::Net(_)) {
            return true;
        }
        false
    }

    /// 检测器期望的输入通道数 (能量 VAD = 1, WakeNet 2CH = 2)
    pub fn channels(&self) -> usize {
        match self {
            Self::Vad(_) => 1,
            #[cfg(feature = "wakenet")]
            Self::Net(net) => net.channels(),
        }
    }
}

impl WakeWordSource for WakeDetector {
    fn detect(&mut self, pcm: &[i16]) -> bool {
        match self {
            Self::Vad(det) => det.detect(pcm),
            #[cfg(feature = "wakenet")]
            Self::Net(net) => net.detect(pcm),
        }
    }
}

/// 主循环以借用方式复用常驻检测器 (CaptureFault 重入时不重复加载模型)
impl WakeWordSource for &mut WakeDetector {
    fn detect(&mut self, pcm: &[i16]) -> bool {
        (**self).detect(pcm)
    }
}
