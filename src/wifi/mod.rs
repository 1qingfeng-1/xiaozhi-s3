//! WiFi 子系统 — esp-radio 1.0.0-beta.0 (配 esp-hal 1.1) + esp-rtos 调度
//!
//! 流程: 读 Flash 凭据 → STA 连接 (DHCP) → 网络;
//! 无凭据/连接失败 → SoftAP 配网 (热点 `xiaozhi-gezipai`,
//! 浏览器打开 http://192.168.4.1/ 填表单) → 存 Flash → 重启。

pub mod store;

use esp_radio::wifi::{Config, sta::StationConfig};

/// WiFi 配置 (配网表单内容)
#[derive(Debug, Clone)]
pub struct WifiConfig {
    /// WiFi 名称
    pub ssid: heapless::String<32>,
    /// WiFi 密码
    pub password: heapless::String<64>,
    /// 服务器地址 (主机 IP, 不含协议/端口; OTA 探测用 :8003)
    pub server: heapless::String<64>,
}

impl WifiConfig {
    pub fn new(ssid: &str, password: &str, server: &str) -> Self {
        Self {
            ssid: heapless::String::try_from(ssid).unwrap_or_default(),
            password: heapless::String::try_from(password).unwrap_or_default(),
            server: heapless::String::try_from(server).unwrap_or_default(),
        }
    }

    /// 构造 STA 配置 (esp-radio)
    pub fn station_config(&self) -> Config {
        Config::Station(
            StationConfig::default()
                .with_ssid(self.ssid.as_str())
                .with_password(self.password.as_str().into()),
        )
    }
}

/// WiFi 启动模式 (配网判断用)
pub enum Mode {
    /// 已有凭据, 走 STA
    Station(WifiConfig),
    /// 无凭据, 走 SoftAP 配网
    Provision,
}

/// 从 Flash 读配置决定启动模式
pub fn boot_mode(flash: &mut esp_storage::FlashStorage) -> Mode {
    match store::load(flash) {
        Some(config) => Mode::Station(config),
        None => Mode::Provision,
    }
}
