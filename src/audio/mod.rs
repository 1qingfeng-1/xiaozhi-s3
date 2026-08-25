//! 音频模块 - ES8311 Codec + ES7210 麦克风阵列
//!
//! 本模块是**硬件薄封装**：
//! - 纯逻辑（音量映射、时钟计算、寄存器序列、状态机）委托给 [`xiaozhi_core::audio`]
//! - 本层只负责 I2S/I2C 寄存器读写、DMA 通道、引脚绑定
//!
//! 依赖 `esp-hal` 1.1.x 的 I2S DMA/TDM 与 I2C master API。

use esp_hal::{
    Blocking,
    dma::DmaChannelFor,
    i2c::master::{Config as I2cConfig, ConfigError, I2c},
    i2s::{
        AnyI2s,
        master::{Channels, Config, DataFormat, I2s},
    },
    peripherals::{I2C0, I2S0},
    time::Rate,
};

use xiaozhi_core::audio::{
    codec::{self, RegWrite, es7210_init_sequence, es8311_init_sequence, es8311_volume_write},
    state::{self, AudioEvent, AudioState},
    volume,
};

/// 音频子系统（硬件薄封装）
///
/// `Dma` 是分配给 I2S0 的 DMA 通道类型（peripheral-specific）。
pub struct Audio<'d, Dma: DmaChannelFor<AnyI2s<'d>>> {
    _i2s: I2s<'d, Blocking>,
    _i2c: I2c<'d, Blocking>,
    /// 占位，保持 DMA 通道类型不被丢弃（实际 DMA 缓冲在 start_* 时分配）
    _dma: core::marker::PhantomData<Dma>,
    /// 纯逻辑状态机（来自 xiaozhi-core，host 可测）
    pub state: AudioState,
    /// 当前音量百分比 (0~100)
    volume_percent: u8,
}

impl<'d, Dma: DmaChannelFor<AnyI2s<'d>>> Audio<'d, Dma> {
    /// 初始化音频模块（I2S + I2C + Codec 寄存器序列）。
    pub fn init(i2s: I2S0<'d>, i2c: I2C0<'d>, dma: Dma) -> Result<Self, ConfigError> {
        let cfg = codec::AudioConfig::stereo_16bit_48k();

        // I2C：控制 ES8311/ES7210（400kHz）
        let mut i2c = I2c::new(
            i2c,
            I2cConfig::default().with_frequency(Rate::from_khz(400)),
        )?;

        // I2S：esp-hal 1.1.x 新 API —— DMA 通道 + TDM Philips 标准
        let i2s_cfg = Config::new_tdm_philips()
            .with_sample_rate(Rate::from_hz(cfg.sample_rate))
            .with_data_format(DataFormat::Data16Channel16)
            .with_channels(Channels::STEREO);
        let i2s = I2s::new(i2s, dma, i2s_cfg).expect("I2S init failed");

        // 执行纯逻辑层生成的 Codec 寄存器序列（硬件 I2C 写入）
        let _ = Self::write_codec_seq(&mut i2c, &es8311_init_sequence(cfg));
        let _ = Self::write_codec_seq(&mut i2c, &es7210_init_sequence(cfg));

        Ok(Self {
            _i2s: i2s,
            _i2c: i2c,
            _dma: core::marker::PhantomData,
            state: state::transition(AudioState::Initializing, AudioEvent::InitDone)
                .unwrap_or(AudioState::Error),
            volume_percent: 50,
        })
    }

    /// 设置音量（纯逻辑映射 + 硬件写入）。
    pub fn set_volume(&mut self, percent: u8) {
        self.volume_percent = percent;
        let writes = es8311_volume_write(percent);
        let _ = Self::write_codec_seq(&mut self._i2c, &writes);
        // 调试：验证纯逻辑映射（host 上有单测覆盖）
        let _ = volume::percent_to_codec(percent);
    }

    /// 音量 +/-（纯逻辑 adjust + 硬件写入）
    pub fn adjust_volume(&mut self, delta: i16) {
        let next = volume::adjust_volume(self.volume_percent, delta);
        self.set_volume(next);
    }

    /// 开始录音
    pub fn start_record(&mut self) {
        if let Some(next) = state::transition(self.state, AudioEvent::Start) {
            self.state = next;
        }
        // TODO: 配置 ES7210 为录音模式 + 启动 I2S DMA 接收
    }

    /// 开始播放
    pub fn start_playback(&mut self) {
        if let Some(next) = state::transition(self.state, AudioEvent::Start) {
            self.state = next;
        }
        // TODO: 配置 ES8311 为播放模式 + 启动 I2S DMA 发送 + LPA2103A 功放
    }

    /// 停止
    pub fn stop(&mut self) {
        if let Some(next) = state::transition(self.state, AudioEvent::Stop) {
            self.state = next;
        }
        // TODO: 停止 I2S + 关闭功放
    }

    /// 按序列写入 Codec 寄存器（硬件 I2C）。
    fn write_codec_seq(
        i2c: &mut I2c<'d, Blocking>,
        seq: &[RegWrite],
    ) -> Result<(), esp_hal::i2c::master::Error> {
        for w in seq {
            i2c.write(w.codec.value(), [w.reg, w.value].as_slice())?;
        }
        Ok(())
    }
}
