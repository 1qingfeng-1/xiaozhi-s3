//! 显示组件 — ST7789 240x240 SPI (状态文本, 3x5 点阵字库来自 xiaozhi-core)
//!
//! 无帧缓冲设计: 按字符窗口直接写面板 (CASET/RASET + RAMWR), 内部 RAM
//! 占用为零。状态文本属低频更新 (状态切换时), 逐字符写完全够用。

use esp_hal::{
    Blocking,
    gpio::Output,
    spi::master::{Config as SpiConfig, Spi},
    time::{Duration, Instant, Rate},
};

/// 同步延时 (显示初始化在工厂内执行, 无异步上下文)
fn delay_ms(ms: u64) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(ms) {}
}

use xiaozhi_core::display::{GLYPH_GAP, GLYPH_H, GLYPH_W, glyph_rows, row_pixels, text_size};

/// RGB565 颜色
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb565(pub u16);

impl Rgb565 {
    pub const BLACK: Rgb565 = Rgb565(0x0000);
    pub const WHITE: Rgb565 = Rgb565(0xFFFF);
    pub const GREEN: Rgb565 = Rgb565(0x07E0);
    pub const CYAN: Rgb565 = Rgb565(0x07FF);
    pub const YELLOW: Rgb565 = Rgb565(0xFFE0);

    pub fn bytes(self) -> [u8; 2] {
        self.0.to_be_bytes()
    }
}

/// ST7789V 命令 (常用子集)
const CMD_SWRESET: u8 = 0x01;
const CMD_SLPOUT: u8 = 0x11;
const CMD_MADCTL: u8 = 0x36;
const CMD_COLMOD: u8 = 0x3A;
const CMD_CASET: u8 = 0x2A;
const CMD_RASET: u8 = 0x2B;
const CMD_RAMWR: u8 = 0x2C;
const CMD_INVON: u8 = 0x21;
const CMD_NORON: u8 = 0x13;
const CMD_DISPON: u8 = 0x29;

/// 面板尺寸 (gezipai: 240x240, 无行偏移)
const PANEL_W: u16 = 240;
const PANEL_H: u16 = 240;

/// ST7789 显示器
pub struct Display {
    spi: Spi<'static, Blocking>,
    dc: Output<'static>,
    _rst: Output<'static>,
    _backlight: Output<'static>,
}

impl Display {
    /// 组装并初始化 (工厂调用; 引脚接线见 BoardConfig.display)
    pub fn new(
        spi: Spi<'static, Blocking>,
        mut dc: Output<'static>,
        mut rst: Output<'static>,
        mut backlight: Output<'static>,
    ) -> Self {
        backlight.set_high();
        // 硬复位
        rst.set_low();
        delay_ms(10);
        rst.set_high();
        delay_ms(120);

        let mut disp = Self {
            spi,
            dc,
            _rst: rst,
            _backlight: backlight,
        };
        // 初始化序列 (ST7789 无 GRAM 初始化区的通用最小集)
        disp.cmd(CMD_SWRESET);
        delay_ms(150);
        disp.cmd(CMD_SLPOUT);
        delay_ms(120);
        disp.cmd_data(CMD_COLMOD, &[0x55]); // 16bit RGB565
        disp.cmd_data(CMD_MADCTL, &[0x00]); // 顶部左起, RGB
        disp.cmd(CMD_INVON); // IPS 面板需要反显
        disp.cmd(CMD_NORON);
        disp.cmd(CMD_DISPON);
        disp.cmd_data(
            CMD_CASET,
            &[0, 0, ((PANEL_W - 1) >> 8) as u8, (PANEL_W - 1) as u8],
        );
        disp.cmd_data(
            CMD_RASET,
            &[0, 0, ((PANEL_H - 1) >> 8) as u8, (PANEL_H - 1) as u8],
        );
        disp
    }

    /// 写命令 (DC 拉低)
    fn cmd(&mut self, c: u8) {
        self.dc.set_low();
        let _ = self.spi.write(&[c]);
    }

    /// 命令 + 参数
    fn cmd_data(&mut self, c: u8, data: &[u8]) {
        self.cmd(c);
        self.dc.set_high();
        let _ = self.spi.write(data);
    }

    /// 设置写窗口 (含端点)
    fn set_window(&mut self, x: u16, y: u16, w: u16, h: u16) {
        let (x0, y0) = (x, y);
        let (x1, y1) = (x + w - 1, y + h - 1);
        self.cmd_data(
            CMD_CASET,
            &[
                (x0 >> 8) as u8,
                (x0 & 0xFF) as u8,
                (x1 >> 8) as u8,
                (x1 & 0xFF) as u8,
            ],
        );
        self.cmd_data(
            CMD_RASET,
            &[
                (y0 >> 8) as u8,
                (y0 & 0xFF) as u8,
                (y1 >> 8) as u8,
                (y1 & 0xFF) as u8,
            ],
        );
        self.cmd(CMD_RAMWR);
        self.dc.set_high();
    }

    /// 填充矩形
    pub fn fill_rect(&mut self, x: u16, y: u16, w: u16, h: u16, color: Rgb565) {
        self.set_window(x, y, w, h);
        let [hi, lo] = color.bytes();
        // 一次窗口连续写 (SPI 按像素流填充)
        let mut rem = (w as usize) * (h as usize);
        let mut buf = [0u8; 512];
        while rem > 0 {
            let chunk = rem.min(256);
            for px in buf[..chunk * 2].chunks_mut(2) {
                px[0] = hi;
                px[1] = lo;
            }
            let _ = self.spi.write(&buf[..chunk * 2]);
            rem -= chunk;
        }
    }

    /// 清屏
    pub fn clear(&mut self, color: Rgb565) {
        self.fill_rect(0, 0, PANEL_W, PANEL_H, color);
    }

    /// 绘制文本 (3x5 点阵; 越界部分静默裁剪; 不支持的字形渲染为空白)
    ///
    /// `scale` 为整数放大 (1/2/3...), 便于状态行在小屏可读。
    pub fn draw_text(&mut self, x: u16, y: u16, text: &str, fg: Rgb565, bg: Rgb565, scale: u16) {
        let scale = scale.max(1);
        let mut cx = x;
        for ch in text.chars() {
            let rows = glyph_rows(ch as u8);
            for row in 0..GLYPH_H {
                let px: [bool; GLYPH_W] = match &rows {
                    Some(r) => row_pixels(r[row]),
                    None => [false; GLYPH_W],
                };
                for col in 0..GLYPH_W {
                    let on = px[col];
                    let color = if on { fg } else { bg };
                    // 放大: scale×scale 实心块
                    self.fill_rect(
                        cx + (col as u16) * scale,
                        y + (row as u16) * scale,
                        scale,
                        scale,
                        color,
                    );
                }
            }
            cx += (GLYPH_W + GLYPH_GAP) as u16 * scale;
        }
    }

    /// 状态行: 清区 + 写文本 (水平居中, 2 倍字高)
    pub fn draw_status(&mut self, text: &str, fg: Rgb565) {
        const BG: Rgb565 = Rgb565::BLACK;
        let (w, h) = text_size(text);
        let scale = 2u16;
        let px_w = (w as u16) * scale;
        let x = px_w.min(PANEL_W.saturating_sub(1));
        let x = if x == px_w { (PANEL_W - px_w) / 2 } else { 0 };
        let y = 12;
        // 清除文本区 (上一次文本残留)
        self.fill_rect(0, y - 2, PANEL_W, h as u16 * scale + 4, BG);
        self.draw_text(x, y, text, fg, BG, scale);
    }
}

/// 工厂辅助: 构造 SPI 总线 (gezipai: SPI2, 面板 20MHz, Mode0)
pub fn make_spi(
    spi: esp_hal::peripherals::SPI2<'static>,
    sck: esp_hal::peripherals::GPIO7<'static>,
    mosi: esp_hal::peripherals::GPIO6<'static>,
    cs: esp_hal::peripherals::GPIO15<'static>,
) -> Spi<'static, Blocking> {
    Spi::new(
        spi,
        SpiConfig::default()
            .with_frequency(Rate::from_mhz(20))
            .with_mode(esp_hal::spi::Mode::_0),
    )
    .expect("SPI2 init failed")
    .with_sck(sck)
    .with_mosi(mosi)
    .with_cs(cs)
}
