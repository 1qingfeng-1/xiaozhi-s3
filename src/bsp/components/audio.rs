//! 音频组件 — ES8311 播放 + ES7210 录音控制 + I2S TX 数据通路 + PA 功放
//!
//! 分层: 寄存器序列来自 xiaozhi-core (纯逻辑, host 可测),
//! 本组件只做 I2C 写入、I2S DMA、PA 开关。
//! 录音通路 (I2S RX TDM) 下阶段接入。

use esp_hal::{
    Blocking,
    dma::{DmaChannelFor, DmaDescriptor},
    i2c::master::I2c,
    i2s::{
        AnyI2s,
        master::{Channels, Config, DataFormat, I2s, I2sTx},
    },
    peripherals::{GPIO39, GPIO40, GPIO45, GPIO48},
    time::{Duration, Instant},
};

use alloc::boxed::Box;
use alloc::vec;
use xiaozhi_core::audio::codec::{
    self as codec_mod, RegWrite, es7210_init_sequence, es8311_init_sequence, es8311_volume_write,
};

/// DMA 描述符数量 (每个最大管理 ~4092B)
const TX_DESCRIPTOR_COUNT: usize = 64;
/// 发送缓冲 16KB ≈ 0.25s @ 16kHz 立体声 16bit
const TX_BUF_SIZE: usize = 16 * 1024;

/// 音频子系统
pub struct Audio {
    i2c: I2c<'static, Blocking>,
    i2s_tx: I2sTx<'static, Blocking>,
    _pa: esp_hal::gpio::Output<'static>,
    tx_buf: &'static mut [u8],
    volume: u8,
}

impl Audio {
    /// 组装音频子系统 (完成 I2S 引脚接线 + Codec 寄存器初始化 + PA 使能)
    ///
    /// # 参数
    /// - `i2c`: 已初始化的 Codec 控制总线
    /// - 引脚: BCLK=39, WS(LRCK)=45, DOUT=48, MCLK=40 (gezipai, 见 BoardConfig)
    /// - `pa`: 功放使能脚 (高电平开启)
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mut i2c: I2c<'static, Blocking>,
        i2s0: esp_hal::peripherals::I2S0<'static>,
        dma: impl DmaChannelFor<AnyI2s<'static>>,
        bclk: GPIO39<'static>,
        ws: GPIO45<'static>,
        dout: GPIO48<'static>,
        mclk: GPIO40<'static>,
        pa: esp_hal::gpio::Output<'static>,
        sample_rate_hz: u32,
    ) -> Self {
        // 1. Codec 寄存器初始化 (序列来自 xiaozhi-core)
        let es8311_seq = es8311_init_sequence();
        // 复位保持 (0x1F) → delay 5ms → 其余寄存器
        if let Err(e) = Self::write_seq(&mut i2c, &es8311_seq[..1]) {
            defmt::error!("ES8311 复位写失败: {:?}", e);
        }
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(5) {}

        // 2. I2S 引脚接线与 DMA (先于音量写入, 构造结构体后补日志)
        let cfg = Config::new_tdm_philips()
            .with_sample_rate(esp_hal::time::Rate::from_hz(sample_rate_hz))
            .with_data_format(DataFormat::Data16Channel16)
            .with_channels(Channels::STEREO);
        let i2s = I2s::new(i2s0, dma, cfg)
            .expect("I2S init failed")
            .with_mclk(mclk);
        let descriptors: &'static mut [DmaDescriptor] =
            Box::leak(vec![DmaDescriptor::EMPTY; TX_DESCRIPTOR_COUNT].into_boxed_slice());
        let tx_buf: &'static mut [u8] = Box::leak(vec![0u8; TX_BUF_SIZE].into_boxed_slice());
        let i2s_tx = i2s
            .i2s_tx
            .with_bclk(bclk)
            .with_ws(ws)
            .with_dout(dout)
            .build(descriptors);

        // 3. 组装 (PA 默认关, init 完成后统一拉高)
        let mut audio = Self {
            i2c,
            i2s_tx,
            _pa: pa,
            tx_buf,
            volume: 60,
        };
        audio.write_seq_logged("ES8311", &es8311_seq[1..]);
        audio.write_seq_logged("ES7210", es7210_init_sequence());
        audio.write_seq_logged("音量", &es8311_volume_write(60));
        // 功放使能 (高电平)
        audio._pa.set_high();

        audio
    }

    /// 读取 ES8311 芯片 ID (寄存器 0xFD/0xFE), 用于 I2C 通路自检
    pub fn es8311_chip_id(&mut self) -> [u8; 2] {
        let mut id = [0u8; 2];
        let _ = self
            .i2c
            .write_read(codec_mod::CodecAddr::Es8311.value(), &[0xFD], &mut id);
        id
    }

    /// 排查用: 回读 ES8311 关键寄存器并与期望值比对。
    /// 全 OK 而仍无声 => 问题在电气层 (MCLK/PA/喇叭); 有 != => 配置写入未生效。
    pub fn dump_registers(&mut self) {
        const CHECK: [(&str, u8, u8); 11] = [
            ("RESET slave 运行", 0x00, 0x80),
            ("CLK MCLK直入", 0x01, 0x3F),
            ("CLK 预分频", 0x02, 0x00),
            ("模拟上电!", 0x0D, 0x01),
            ("时钟门控", 0x0E, 0x02),
            ("DAC 使能", 0x12, 0x00),
            ("模拟无DMIC", 0x14, 0x1A),
            ("解除数字静音", 0x31, 0x00),
            ("音量60%=0x99", 0x32, 0x99),
            ("DAC输出路径", 0x37, 0x08),
            ("GP控制", 0x45, 0x00),
        ];
        for (name, reg, exp) in CHECK {
            let v = self.read_reg(reg);
            if v == exp {
                defmt::info!("ES8311 0x{:02X}=0x{:02X} OK {}", reg, v, name);
            } else {
                defmt::error!(
                    "ES8311 0x{:02X}=0x{:02X} != 期望 0x{:02X} ({})",
                    reg,
                    v,
                    exp,
                    name
                );
            }
        }
        defmt::info!("PA 使能脚电平: {}", self._pa.is_set_high());
    }

    /// 读取单个 ES8311 寄存器 (失败返回 0xFF)
    fn read_reg(&mut self, reg: u8) -> u8 {
        let mut v = [0u8; 1];
        if self
            .i2c
            .write_read(codec_mod::CodecAddr::Es8311.value(), &[reg], &mut v)
            .is_err()
        {
            return 0xFF;
        }
        v[0]
    }

    /// 设置音量 (0~100), 0 为数字静音
    pub fn set_volume(&mut self, percent: u8) {
        self.volume = percent.clamp(0, 100);
        let _ = Self::write_seq(&mut self.i2c, &es8311_volume_write(self.volume));
    }

    /// 当前音量
    pub fn volume(&self) -> u8 {
        self.volume
    }

    /// 播放提示音 (250Hz 正弦, 指定时长) — 扬声器通路验证
    pub fn beep(&mut self, ms: u32) {
        let mut remaining = ms;
        while remaining > 0 {
            let chunk_ms = remaining.min(250); // 16KB ≈ 250ms
            Self::fill_sine(self.tx_buf, chunk_ms);
            match self.i2s_tx.write_dma(&self.tx_buf) {
                Ok(t) => {
                    let _ = t.wait();
                },
                Err(e) => defmt::error!("I2S DMA 发送失败: {:?}", e),
            }
            remaining -= chunk_ms;
        }
        // 收尾静音, 防止残余爆音
        self.tx_buf.fill(0);
        if let Ok(t) = self.i2s_tx.write_dma(&self.tx_buf) {
            let _ = t.wait();
        }
    }

    /// 填充正弦波 (250Hz @16kHz: 64 采样/周期, 立体声 16bit)
    fn fill_sine(buf: &mut [u8], ms: u32) {
        const TAB: [i16; 64] = build_sine_table();
        let samples = (ms as usize * 16).min(buf.len() / 4); // 16 采样/ms @16kHz
        for i in 0..samples {
            let v = TAB[i % 64].to_le_bytes();
            let o = i * 4;
            buf[o..o + 2].copy_from_slice(&v); // 左
            buf[o + 2..o + 4].copy_from_slice(&v); // 右
        }
    }

    fn write_seq(
        i2c: &mut I2c<'static, Blocking>,
        seq: &[RegWrite],
    ) -> Result<(), esp_hal::i2c::master::Error> {
        for w in seq {
            i2c.write(w.codec.value(), [w.reg, w.value].as_slice())?;
        }
        Ok(())
    }

    /// 序列写入 (失败打印日志, 不中断初始化)
    fn write_seq_logged(&mut self, name: &str, seq: &[RegWrite]) {
        if let Err(e) = Self::write_seq(&mut self.i2c, seq) {
            defmt::error!("Codec {} 初始化失败: {:?}", name, e);
        }
    }
}

/// 64 点正弦表: 32767 * sin(2*pi*n/64), 250Hz @ 16kHz
const fn build_sine_table() -> [i16; 64] {
    [
        0, 3212, 6393, 9512, 12539, 15446, 18204, 20787, 23170, 25329, 27245, 28898, 30273, 31356,
        32137, 32609, 32767, 32609, 32137, 31356, 30273, 28898, 27245, 25329, 23170, 20787, 18204,
        15446, 12539, 9512, 6393, 3212, 0, -3212, -6393, -9512, -12539, -15446, -18204, -20787,
        -23170, -25329, -27245, -28898, -30273, -31356, -32137, -32609, -32767, -32609, -32137,
        -31356, -30273, -28898, -27245, -25329, -23170, -20787, -18204, -15446, -12539, -9512,
        -6393, -3212,
    ]
}
