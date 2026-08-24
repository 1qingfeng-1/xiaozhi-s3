//! 音频模块 - ES8311 Codec + ES7210 麦克风阵列
//! I2S 音频接口配置

use esp_hal::{
    i2s::{I2s, Mode, Standard, Config},
    i2c::{I2c, Config as I2cConfig, Frequency},
    peripherals::{I2S0, I2C0},
};

/// 音频配置
pub struct Audio {
    _i2s: I2s<I2S0>,
    _i2c: I2c<I2C0>,
}

impl Audio {
    /// 初始化音频模块
    /// - I2S0: 音频数据传输
    /// - I2C0: 控制 ES8311/ES7210
    pub fn init(i2s: I2S0, i2c: I2C0) -> Self {
        // 配置 I2C (用于控制 ES8311/ES7210)
        let i2c_config = I2cConfig::new(Frequency::MHz(400));
        let i2c = I2c::new(i2c, i2c_config);

        // 配置 I2S (音频数据)
        let i2s_config = Config {
            mode: Mode::FullDuplex,
            standard: Standard::Philips,
            ..Default::default()
        };
        let i2s = I2s::new(i2s, i2s_config);

        // TODO: 初始化 ES8311 Codec
        // TODO: 初始化 ES7210 麦克风阵列
        // TODO: 配置 I2S 时钟 (BCLK, LRCK)
        // TODO: 配置 I2C 地址 (ES8311: 0x18, ES7210: 0x40)

        Self { _i2s: i2s, _i2c: i2c }
    }

    /// 开始录音 (麦克风采集)
    pub fn start_record(&mut self) {
        // 配置 ES7210 为录音模式
        // 启动 I2S 接收
        // 配置 ES8311 的 ADC 通道
    }

    /// 开始播放 (扬声器输出)
    pub fn start_playback(&mut self) {
        // 配置 ES8311 为播放模式
        // 启动 I2S 发送
        // 配置 LPA2103A 功放
    }

    /// 停止音频
    pub fn stop(&mut self) {
        // 停止 I2S
        // 关闭功放
    }
}
