//! 音频组件 — ES8311 播放 + ES7210 录音 + I2S 全双工数据通路 + PA 功放
//!
//! 分层: 寄存器序列来自 xiaozhi-core (纯逻辑, host 可测),
//! 本组件只做 I2C 写入、I2S DMA、PA 开关。
//!
//! 全双工: 播放 (TX) 与采集 (RX) 共用 I2S0。TX 为时钟主机,
//! 经 `SIG_LOOPBACK` 把 BCLK/WS 共享给 RX (RX 为 slave 跟随,
//! 对齐 IDF 全双工 `i2s_ll_share_bck_ws`)。线格式统一 2ch 立体声
//! 16bit @16k (ES7210 非-TDM, MIC1→L / MIC2→R);
//! TDM 4ch + 参考通道留待 AEC 阶段 (M5) 启用。

use esp_hal::{
    Blocking,
    dma::{DmaChannelFor, DmaDescriptor, DmaTransferRxCircular, DmaTransferTxCircular},
    i2c::master::I2c,
    i2s::{
        AnyI2s,
        master::{Channels, Config, DataFormat, Error, I2s, I2sRx, I2sTx},
    },
    peripherals::{GPIO21, GPIO39, GPIO40, GPIO45, GPIO48},
    time::{Duration, Instant},
};

use alloc::boxed::Box;
use alloc::vec;
use core::sync::atomic::{AtomicU32, Ordering};
use xiaozhi_core::audio::codec::{
    self as codec_mod, RegWrite, es7210_init_sequence, es8311_init_sequence, es8311_volume_write,
};

/// TX DMA 描述符数量 (每个最大管理 4092B; 环形模式按需切分即可)
const TX_DESCRIPTOR_COUNT: usize = 8;
/// 发送环形缓冲 ≈0.13s @ 16kHz 立体声 16bit
///
/// 环形 DMA 会**循环播放缓冲内容**: 必须始终把缓冲填满 (空闲补静音),
/// 否则 DMA 追到写指针后会读到上一圈的旧数据 (提示音/TTS 残片 → 周期爆音)。
const TX_BUF_SIZE: usize = DMA_CHUNK_SIZE * 2;
/// 公共只读: 环形缓冲总字节数 (调用方按需换算低水位)
pub const TX_RING_BYTES: usize = TX_BUF_SIZE;
/// beep 生成分块缓冲 (一整个 DMA 描述符)
const BEEP_CHUNK_SIZE: usize = DMA_CHUNK_SIZE;
/// 单个 DMA 描述符管理的最大字节数
const DMA_CHUNK_SIZE: usize = 4092;
/// TX 环形欠载计数 (诊断: 播报期队列空 → DMA 回放旧数据)
static UNDERRUNS: AtomicU32 = AtomicU32::new(0);
/// RX 循环缓冲 = 8 个整描述符 ≈ 0.25s @ 16kHz 立体声 16bit。
/// 描述符必须与缓冲精确等分: esp-hal 循环采集的软件状态回溯从链尾
/// 描述符出发, 数量不匹配 (多余空描述符) 会导致空指针崩溃
const RX_BUF_SIZE: usize = DMA_CHUNK_SIZE * 8;
const RX_DESCRIPTOR_COUNT: usize = 8;

/// 音频播放子系统 (I2S TX **环形**常流 + Codec 控制 + PA)
///
/// ⚠️ 播放必须走环形 DMA 常流: 早期一次性 `write_dma` 每块停/启一次,
/// 使 BCLK/WS 周期性停摆 —— 播放端表现为「滴滴答答」爆音, 且 RX 从机
/// (ES7210) 随共享时钟停摆导致上行数据损坏 (真机 2026-09-26 定位)。
pub struct Audio {
    i2c: I2c<'static, Blocking>,
    /// 启动环形传输前持有的 I2S TX 外设 ('static: 环形句柄借用它, 泄漏保活)
    i2s_tx: Option<&'static mut I2sTx<'static, Blocking>>,
    /// 常流句柄 (首次播放/提示音时惰性启动, 避开启动期 OTA 长阻塞)
    tx: Option<DmaTransferTxCircular<'static, I2sTx<'static, Blocking>>>,
    /// TX 环形缓冲 (**必须在 PSRAM 注册前分配**, 否则落 PSRAM 触发 DmaError)
    tx_buf: Option<&'static mut [u8]>,
    /// beep 分块生成缓冲 (堆分配, 避免大栈帧)
    beep_buf: &'static mut [u8],
    _pa: esp_hal::gpio::Output<'static>,
    volume: u8,
}

/// 录音子系统 (I2S RX, ES7210 麦克风)
pub struct Capture {
    i2s_rx: I2sRx<'static, Blocking>,
    rx_buf: &'static mut [u8],
}

impl Audio {
    /// 组装音频子系统 (I2S 全双工接线 + Codec 寄存器初始化 + PA 使能)
    ///
    /// 返回 (播放, 采集) 两个独立组件, 可分别借用 (回环同时收发)。
    ///
    /// # 参数
    /// - `i2c`: 已初始化的 Codec 控制总线
    /// - 引脚: BCLK=39, WS(LRCK)=45, DOUT=48, MCLK=40, DIN=21 (gezipai, 见 BoardConfig)
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
        din: GPIO21<'static>,
        pa: esp_hal::gpio::Output<'static>,
        sample_rate_hz: u32,
    ) -> (Self, Capture) {
        // 1. Codec 寄存器初始化 (序列来自 xiaozhi-core)
        let es8311_seq = es8311_init_sequence();
        // 复位保持 (0x1F) → delay 5ms → 其余寄存器
        if let Err(e) = Self::write_seq(&mut i2c, &es8311_seq[..1]) {
            defmt::error!("ES8311 复位写失败: {:?}", e);
        }
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(5) {}

        // 2. I2S 全双工: TX 主机驱动时钟, SIG_LOOPBACK 共享给 RX (slave 跟随)
        let cfg = Config::new_tdm_philips()
            .with_sample_rate(esp_hal::time::Rate::from_hz(sample_rate_hz))
            .with_data_format(DataFormat::Data16Channel16)
            .with_channels(Channels::STEREO)
            .with_signal_loopback(true);
        let i2s = I2s::new(i2s0, dma, cfg)
            .expect("I2S init failed")
            .with_mclk(mclk);
        let tx_descriptors: &'static mut [DmaDescriptor] =
            Box::leak(vec![DmaDescriptor::EMPTY; TX_DESCRIPTOR_COUNT].into_boxed_slice());
        let i2s_tx = i2s
            .i2s_tx
            .with_bclk(bclk)
            .with_ws(ws)
            .with_dout(dout)
            .build(tx_descriptors);

        // RX 只接 DIN; BCLK/WS 经 SIG_LOOPBACK 跟随 TX (同一物理线, 无需引脚)
        let rx_descriptors: &'static mut [DmaDescriptor] =
            Box::leak(vec![DmaDescriptor::EMPTY; RX_DESCRIPTOR_COUNT].into_boxed_slice());
        let rx_buf: &'static mut [u8] = Box::leak(vec![0u8; RX_BUF_SIZE].into_boxed_slice());
        let i2s_rx = i2s.i2s_rx.with_din(din).build(rx_descriptors);

        // (保持两槽 I2S: ES8311/ES7210 共用 32bit/帧; 早期尝试 4 槽因 esp-hal
        //  apply_config 不触发 tx_update/rx_update 实际未生效, 已回退)

        // 3. 组装 (PA 默认关, init 完成后统一拉高)
        //    ES7210 立体声 (非 TDM): 与 2 槽位线格式匹配, AEC 阶段切 TDM
        //    TX 外设泄漏保活: 环形句柄需 'static 借用 (板上常驻, 全生命周期一个)
        let mut audio = Self {
            i2c,
            i2s_tx: Some(Box::leak(Box::new(i2s_tx))),
            tx: None,
            tx_buf: Some(Box::leak(vec![0u8; TX_BUF_SIZE].into_boxed_slice())),
            beep_buf: Box::leak(vec![0u8; BEEP_CHUNK_SIZE].into_boxed_slice()),
            _pa: pa,
            volume: 60,
        };
        audio.write_seq_logged("ES8311", &es8311_seq[1..]);
        // ES7210: 先复位, 再读回 REG08 默认值 (绝不能整字节写 0x00: 会清掉
        // bit4 → ADC 只有 fs/8 采样点, 真机 2026-10-22 定位), 最后只改 bit0。
        let es7210_seq = es7210_init_sequence(false);
        audio.write_seq_logged("ES7210 复位", &es7210_seq[..2]);
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(5) {}
        let r08_default = audio.read_reg_at(codec_mod::CodecAddr::Es7210.value(), 0x08);
        defmt::info!("ES7210 REG08 复位默认=0x{:02X}", r08_default);
        audio.write_seq_logged("ES7210", &es7210_seq[2..]);
        let r08 = audio.read_reg_at(codec_mod::CodecAddr::Es7210.value(), 0x08);
        defmt::info!("ES7210 REG08 序列后=0x{:02X}", r08);
        // slave 模式: 只清 bit0; 保留 bit4 (ADC/MCLK 使能) 及其它默认位
        let fixed = (r08 & !0x01) | 0x10;
        audio.write_seq_logged(
            "ES7210 REG08",
            &[RegWrite::new(codec_mod::CodecAddr::Es7210, 0x08, fixed)],
        );
        defmt::info!(
            "ES7210 REG08 修正=0x{:02X}",
            audio.read_reg_at(codec_mod::CodecAddr::Es7210.value(), 0x08)
        );
        audio.write_seq_logged("音量", &es8311_volume_write(60));
        // 功放使能 (高电平)
        audio._pa.set_high();

        let capture = Capture { i2s_rx, rx_buf };
        (audio, capture)
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
        self.read_reg_at(codec_mod::CodecAddr::Es8311.value(), reg)
    }

    /// 读取任意 Codec 寄存器 (失败返回 0xFF)
    fn read_reg_at(&mut self, addr: u8, reg: u8) -> u8 {
        let mut v = [0u8; 1];
        if self.i2c.write_read(addr, &[reg], &mut v).is_err() {
            return 0xFF;
        }
        v[0]
    }

    /// 回读 I2S0 关键寄存器 (排查 TDM 槽数/位宽/帧长, 仅调试用)
    ///
    /// 直接读 S3 I2S0 寄存器窗口 (0x6000_F000), 避免依赖 esp-hal 私有 API。
    pub fn i2s_debug_regs() -> [(u8, u32); 7] {
        const BASE: usize = 0x6000_f000;
        let mut out = [(0u8, 0u32); 7];
        for (i, off) in [0x20u8, 0x24, 0x28, 0x2c, 0x50, 0x54, 0x64]
            .into_iter()
            .enumerate()
        {
            // SAFETY: I2S0 寄存器窗口固定映射, 读操作无副作用
            let v = unsafe { core::ptr::read_volatile((BASE + off as usize) as *const u32) };
            out[i] = (off, v);
        }
        out
    }

    /// 写单个 ES7210 寄存器 (临时诊断/扫描用)
    pub fn es7210_write_reg(&mut self, reg: u8, val: u8) {
        let _ = Self::write_seq(
            &mut self.i2c,
            &[RegWrite::new(codec_mod::CodecAddr::Es7210, reg, val)],
        );
    }

    /// 回读 ES7210 关键寄存器 (排查槽位/字长/麦克风供电是否与预期一致)
    pub fn es7210_debug_regs(&mut self) -> [(u8, u8); 12] {
        const ADDR: u8 = 0x40;
        let mut out = [(0u8, 0u8); 12];
        for (i, reg) in [
            0x11u8, 0x12, 0x02, 0x07, 0x01, 0x40, 0x43, 0x44, 0x47, 0x4B, 0x4C, 0x08,
        ]
        .into_iter()
        .enumerate()
        {
            out[i] = (reg, self.read_reg_at(ADDR, reg));
        }
        out
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

    /// 惰性启动 TX 环形常流 (首次播放/提示音时; 避开启动期 OTA 长阻塞)
    fn ensure_tx(&mut self) {
        if self.tx.is_some() {
            return;
        }
        let Some(i2s_tx) = self.i2s_tx.take() else {
            return;
        };
        let Some(buf) = self.tx_buf.take() else {
            return;
        };
        let shared: &'static [u8] = buf;
        // write_dma_circular 形参为 &'t impl ReadBuffer: ReadBuffer 实现在 &[u8] 上,
        // 需传 &'static &[u8] (泄漏 8B 引用以满足 'static 句柄)
        let words: &'static &'static [u8] = Box::leak(Box::new(shared));
        match i2s_tx.write_dma_circular(words) {
            Ok(t) => {
                self.tx = Some(t);
                defmt::info!("I2S TX 环形常流已启动 ({}B)", TX_BUF_SIZE);
            },
            Err(e) => {
                // 启动失败无法重试 (外设借用已移出): 记录后静默, 由调用方现象暴露
                defmt::error!("I2S TX 环形启动失败: {:?}", e);
            },
        }
    }

    /// 推入数据直到全部写入环形 DMA (空间不足时自旋等 DMA 消费, 即节流)
    fn push_all(
        tx: &mut Option<DmaTransferTxCircular<'static, I2sTx<'static, Blocking>>>,
        data: &[u8],
    ) {
        let Some(t) = tx.as_mut() else {
            return;
        };
        let mut off = 0;
        while off < data.len() {
            match t.push(&data[off..]) {
                // push 是全量语义: 空间不足返回 Overflow, 等消费后重试
                Ok(n) => off += n,
                Err(esp_hal::dma::DmaError::Overflow) => core::hint::spin_loop(),
                Err(e) => {
                    defmt::warn!("I2S TX push 失败: {:?} — 丢弃本块", e);
                    return;
                },
            }
        }
    }

    /// 填满环形缓冲到指定**待播字节数** (空闲补静音): 时钟常开 + 杜绝旧数据回放
    ///
    /// - `target_queued = TX_RING_BYTES`: 填满 (待机/聆听; 只播静音也无所谓)
    /// - `target_queued` 较小 (如 20ms): TTS 播报期用低水位。只在队列快见底时补静音,
    ///   既不把静音插进 TTS 帧之间 (一卡一卡), 也不让 DMA 绕回重放旧数据 (嘟嘟嘟)。
    pub fn maintain_silence(&mut self, silence: &[u8], target_queued: usize) {
        self.ensure_tx();
        let target = target_queued.min(TX_BUF_SIZE);
        loop {
            let Some(t) = self.tx.as_mut() else {
                return;
            };
            let free = match t.available() {
                Ok(v) => v,
                Err(e) => {
                    defmt::warn!("I2S TX available 失败: {:?}", e);
                    return;
                },
            };
            let queued = TX_BUF_SIZE - free.min(TX_BUF_SIZE);
            // 队列空 = DMA 已绕环回放旧数据 (播报期出现即为"重复字/杂音"根因)
            if free >= TX_BUF_SIZE && target < TX_BUF_SIZE {
                let n = UNDERRUNS.fetch_add(1, Ordering::Relaxed) + 1;
                if !n.is_multiple_of(10) || n == 1 {
                    defmt::warn!("I2S TX 欠载 #{} — DMA 回放旧数据", n);
                }
            }
            if queued >= target {
                return;
            }
            let n = (target - queued).min(silence.len());
            Self::push_all(&mut self.tx, &silence[..n]);
        }
    }

    /// 播放提示音 (250Hz 正弦, 指定时长; 环形常流追加, 延迟 ≤ 目标队列)
    pub fn beep(&mut self, ms: u32) {
        self.ensure_tx();
        // 64B/ms @16kHz 立体声 16bit; 分块生成, 保持相位连续
        let total = ms as usize * 64;
        let mut done = 0;
        let mut phase = 0usize;
        while done < total {
            let n = (total - done).min(self.beep_buf.len());
            let Self { beep_buf, tx, .. } = self;
            Self::fill_sine(&mut beep_buf[..n], phase);
            phase = (phase + n / 4) % 64;
            Self::push_all(tx, &beep_buf[..n]);
            done += n;
        }
    }

    /// 播放一块原始立体声 i16-LE 数据 (推入环形常流)
    pub fn play_block(&mut self, data: &[u8]) {
        self.ensure_tx();
        Self::push_all(&mut self.tx, data);
    }

    /// 环形 TX 当前可写入字节数 (0 = 已满或未启动)
    ///
    /// 供上层「软件 PCM 队列 → DMA 环形」泵送使用: 每次只写空闲空间,
    /// 环形永远保持填满, DMA 不会追到写指针后回放旧音频。
    pub fn tx_free(&mut self) -> usize {
        self.ensure_tx();
        match self.tx.as_mut() {
            Some(t) => match t.available() {
                Ok(v) => v,
                Err(e) => {
                    defmt::warn!("I2S TX available 失败 (可能欠载): {:?}", e);
                    0
                },
            },
            None => 0,
        }
    }

    /// 非阻塞写入: 只写空闲空间放得下的部分, 返回实际写入字节数
    pub fn push_avail(&mut self, data: &[u8]) -> usize {
        let Some(t) = self.tx.as_mut() else {
            return 0;
        };
        let free = match t.available() {
            Ok(v) => v,
            Err(_) => return 0,
        };
        let n = data.len().min(free);
        if n == 0 {
            return 0;
        }
        match t.push(&data[..n]) {
            Ok(written) => written,
            Err(_) => 0,
        }
    }

    /// 填充正弦波 (250Hz @16kHz: 64 采样/周期, 立体声 16bit)
    ///
    /// `phase` 为起始采样相位 (0..64), 分块生成时保持连续。
    fn fill_sine(buf: &mut [u8], mut phase: usize) {
        const TAB: [i16; 64] = build_sine_table();
        let samples = buf.len() / 4;
        for i in 0..samples {
            let v = TAB[phase % 64].to_le_bytes();
            let o = i * 4;
            buf[o..o + 2].copy_from_slice(&v); // 左
            buf[o + 2..o + 4].copy_from_slice(&v); // 右
            phase += 1;
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

impl Capture {
    /// 启动连续采集 (循环 DMA)
    ///
    /// 返回的句柄存活期间 DMA 持续运行, 周期性 `available()` / `pop()`
    /// 取走数据; 句柄丢弃即停止采集。
    pub fn start(&mut self) -> Result<DmaTransferRxCircular<'_, I2sRx<'static, Blocking>>, Error> {
        let transfer = self.i2s_rx.read_dma_circular(&mut self.rx_buf)?;
        // esp-hal 1.1.2 把 `rx_eof_num` 写成整条环字节数-1 (32735), 但 S3 该字段
        // 只有 12 位 → 截断成 4063; 与描述符边界 (4092B) 错位, 真机表现为
        // 每 64B 仅 8B 有效 (其余全 0) → ASR 只能听到"嗯/。"。按 IDF 语义
        // 重写为单个描述符字节数。
        const I2S0_RXEOF_NUM: usize = 0x6000_f064;
        // SAFETY: I2S0 寄存器窗口固定映射; 采集刚启动, 写阈值无副作用
        unsafe {
            core::ptr::write_volatile(I2S0_RXEOF_NUM as *mut u32, DMA_CHUNK_SIZE as u32);
        }
        Ok(transfer)
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
