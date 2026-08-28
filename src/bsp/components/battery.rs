//! 电池组件 — ADC 电压检测 + xiaozhi-core 百分比换算

use esp_hal::{
    Blocking,
    analog::adc::{Adc, AdcCalCurve, AdcConfig, AdcPin, Attenuation},
    peripherals::{ADC1, GPIO3},
};

use xiaozhi_core::board::battery_percentage;

/// ADC 换算常数: 11dB 衰减下满量程约 3300mV, 12bit
const ADC_FULL_SCALE_MV: u32 = 3300;
const ADC_MAX_CODE: u32 = 4095;
/// 每次读取的采样数 (均值滤波, 对齐参考实现)
const SAMPLE_COUNT: u32 = 10;

/// 电池电压检测 (ADC1 + eFuse 曲线校准, 对齐 IDF curve fitting)
pub struct Battery {
    adc: Adc<'static, ADC1<'static>, Blocking>,
    pin: AdcPin<GPIO3<'static>, ADC1<'static>, AdcCalCurve<ADC1<'static>>>,
    /// 分压还原系数
    multiplier: u16,
    /// 电量 0% / 100% 对应端电压 (mV)
    min_mv: u16,
    max_mv: u16,
}

impl Battery {
    pub fn new(
        adc1: ADC1<'static>,
        pin: GPIO3<'static>,
        multiplier: u16,
        min_mv: u16,
        max_mv: u16,
    ) -> Self {
        let mut cfg = AdcConfig::default();
        let pin = cfg.enable_pin_with_cal::<GPIO3<'static>, AdcCalCurve<ADC1<'static>>>(
            pin,
            Attenuation::_11dB,
        );
        Self {
            adc: Adc::new(adc1, cfg),
            pin,
            multiplier,
            min_mv,
            max_mv,
        }
    }

    /// 单次采样 (阻塞至转换完成)
    fn read_raw(&mut self) -> u16 {
        loop {
            if let Ok(v) = self.adc.read_oneshot(&mut self.pin) {
                return v;
            }
        }
    }

    /// 读取电池端电压 (mV): 多次采样均值 × 分压还原系数
    pub fn read_voltage_mv(&mut self) -> u32 {
        let mut acc = 0u32;
        for _ in 0..SAMPLE_COUNT {
            acc += self.read_raw() as u32;
        }
        let raw = acc / SAMPLE_COUNT;
        let pin_mv = raw * ADC_FULL_SCALE_MV / ADC_MAX_CODE;
        pin_mv * self.multiplier as u32
    }

    /// 读取电量百分比 (0~100); 配置非法时 None
    pub fn percentage(&mut self) -> Option<u8> {
        let mv = self.read_voltage_mv() as i32;
        let p = battery_percentage(mv, self.min_mv as i32, self.max_mv as i32);
        (p >= 0).then_some(p as u8)
    }
}
