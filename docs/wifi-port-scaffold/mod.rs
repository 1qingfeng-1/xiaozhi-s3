//! WiFi 子系统 — esp-wifi 0.13 (阻塞式) + smoltcp 网络栈
//!
//! 流程: 读 Flash 凭据 → STA 连接 (DHCP) → 返回网络栈;
//! 无凭据/连接失败 → SoftAP 配网 (手机连热点 `xiaozhi-gezipai`,
//! 浏览器打开 http://192.168.4.1/ 填表单) → 存 Flash → 重连。

pub mod net;
pub mod provision;
pub mod store;

use esp_wifi::wifi::{
    AccessPointConfiguration, AuthMethod, ClientConfiguration, Configuration, WifiController,
    WifiDevice, WifiMode,
};

/// WiFi 配置 (配网表单内容)
#[derive(Debug, Clone)]
pub struct WifiConfig {
    /// WiFi 名称
    pub ssid: heapless::String<32>,
    /// WiFi 密码
    pub password: heapless::String<64>,
    /// 服务器地址 (主机, 不含协议/端口; OTA 探测用 :8003)
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
}

/// 初始化 esp-wifi (内部堆注册由 create_board/bin 负责)
///
/// 返回 (EspWifiController, STA 设备, AP 设备)。
/// controller 与设备须全程持有; STA/AP 设备分别驱动独立 smoltcp Interface。
pub fn init(
    timg0: esp_hal::timer::timg::TimerGroup<'static, esp_hal::peripherals::TIMG0<'static>>,
    radio_clk: esp_hal::peripherals::RADIO_CLK<'static>,
    wifi: esp_hal::peripherals::WIFI<'static>,
) -> (
    esp_wifi::EspWifiController<'static>,
    WifiController<'static>,
    WifiDevice<'static>,
    WifiDevice<'static>,
) {
    // WiFi 驱动内部任务栈来自全局堆, 且 CPU 需 ≥80MHz (main 已设 max)
    timg0.wdt.disable();
    let init = esp_wifi::init(timg0.timer0, esp_hal::rng::Rng::new(), radio_clk)
        .expect("esp-wifi 初始化失败");
    let (controller, interfaces) =
        esp_wifi::wifi::new(&init, wifi).expect("wifi 控制器创建失败");
    (init, controller, interfaces.sta, interfaces.ap)
}

/// STA 模式连接 AP (阻塞, 20s 超时)
pub fn connect_sta(
    controller: &mut WifiController<'static>,
    config: &WifiConfig,
) -> Result<(), &'static str> {
    let client = ClientConfiguration {
        ssid: config.ssid.clone(),
        password: config.password.clone(),
        auth_method: AuthMethod::WPA2Personal,
        ..Default::default()
    };
    controller
        .set_configuration(&Configuration::Client(client))
        .map_err(|_| "配置 WiFi 失败")?;
    controller.start().map_err(|_| "WiFi 启动失败")?;

    let start = esp_hal::time::Instant::now();
    while !controller.is_started().unwrap_or(false) {
        if start.elapsed() > esp_hal::time::Duration::from_secs(5) {
            return Err("WiFi 启动超时");
        }
        busy_wait_ms(10);
    }

    controller.connect().map_err(|_| "发起连接失败")?;
    let start = esp_hal::time::Instant::now();
    while !controller.is_connected().unwrap_or(false) {
        if start.elapsed() > esp_hal::time::Duration::from_secs(20) {
            return Err("WiFi 连接超时");
        }
        busy_wait_ms(100);
    }
    Ok(())
}

pub(crate) fn busy_wait_ms(ms: u32) {
    let t0 = esp_hal::time::Instant::now();
    while t0.elapsed() < esp_hal::time::Duration::from_millis(ms) {}
}

/// 单调毫秒时间戳 (自启动起), 供 smoltcp 使用
pub fn now_ms() -> i64 {
    esp_hal::time::Instant::now()
        .duration_since_epoch()
        .to_millis() as i64
}
