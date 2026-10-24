//! 电池组件 — ADC 电压检测 + xiaozhi-core 百分比换算

use esp_hal::{
    Blocking,
    analog::adc::{Adc, AdcCalCurve, AdcConfig, AdcPin, Attenuation},
    peripherals::{ADC1, GPIO3},
};

use xiaozhi_core::board::{PowerStatus, battery_percentage, reported_battery_percentage};

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

    /// 单次采样。AdcCalCurve (basic+line+curve) 校准后返回值已是引脚电压 mV,
    /// 勿再按 12bit raw code 换算 (参考 esp-hal calibration/line.rs 文档)。
    fn read_raw_mv(&mut self) -> Option<u16> {
        // 限次重试: ADC 持续错误时返回 None, 避免忙等卡死音频主循环
        for _ in 0..20 {
            if let Ok(v) = self.adc.read_oneshot(&mut self.pin) {
                return Some(v);
            }
            // 给 ADC/校准一点时间, 避免连续失败
            for _ in 0..20_000 {
                core::hint::spin_loop();
            }
        }
        None
    }

    /// 读取电池端电压 (mV): 多次采样均值 × 分压还原系数
    pub fn read_voltage_mv(&mut self) -> Option<u32> {
        let mut acc = 0u32;
        for _ in 0..SAMPLE_COUNT {
            acc += self.read_raw_mv()? as u32;
        }
        Some(acc / SAMPLE_COUNT * self.multiplier as u32)
    }

    /// 读取电量百分比 (0~100); 配置非法时 None
    pub fn percentage(&mut self) -> Option<u8> {
        let mv = self.read_voltage_mv()? as i32;
        let p = battery_percentage(mv, self.min_mv as i32, self.max_mv as i32);
        (p >= 0).then_some(p as u8)
    }

    /// 读取上报电量 (0~100): 充电完成 (DONE 有效) 钳位 100%,
    /// 其余状态按电压线性映射; 配置非法时 None
    pub fn reported_percentage(&mut self, status: PowerStatus) -> Option<u8> {
        let mv = self.read_voltage_mv()? as i32;
        reported_battery_percentage(status, mv, self.min_mv as i32, self.max_mv as i32)
            .map(|p| p as u8)
    }
}
