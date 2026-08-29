//! WiFi 子系统 — esp-radio 1.0.0-beta.0 (配 esp-hal 1.1) + esp-rtos 调度
//!
//! 流程: 读 Flash 凭据 → STA 连接 (DHCP) → 网络;
//! 无凭据/连接失败 → SoftAP 配网 (热点 `xiaozhi-gezipai`,
//! 浏览器打开 http://192.168.4.1/ 填表单) → 存 Flash → 重启。

pub mod provision;
pub mod store;

use embassy_executor::Spawner;
use embassy_net::{Stack, StackResources};
use esp_hal::peripherals::WIFI;

use esp_radio::wifi::{Config, sta::StationConfig};

use defmt::{error, info};

use crate::mk_static;

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

/// STA 连接: 建网络栈 → 重试连接 (3 次) → 等 DHCP
///
/// 返回网络栈供上层建会话; 失败 (3 次未连上 / DHCP 超时) 返回 Err,
/// 由调用方决定降级策略 (清凭据重回配网或周期重试)。
pub async fn connect_station(
    spawner: &Spawner,
    config: &WifiConfig,
    wifi_peri: WIFI<'static>,
) -> Result<
    (
        Stack<'static>,
        esp_radio::wifi::WifiController<'static>,
        heapless::String<32>,
    ),
    (),
> {
    let interface = esp_radio::wifi::Interface::station();
    let mac_bytes = interface.mac_address();
    let mac = {
        let mut s = heapless::String::<32>::new();
        for (i, b) in mac_bytes.iter().enumerate() {
            if i > 0 {
                let _ = s.push(':');
            }
            use core::fmt::Write as _;
            let _ = write!(&mut s, "{:02X}", b);
        }
        s
    };
    let mut controller = esp_radio::wifi::WifiController::new(
        wifi_peri,
        esp_radio::wifi::ControllerConfig::default().with_initial_config(config.station_config()),
    )
    .map_err(|_| {
        error!("WiFi 控制器初始化失败");
    })?;

    let (stack, runner) = embassy_net::new(
        interface,
        embassy_net::Config::dhcpv4(Default::default()),
        mk_static!(StackResources<4>, StackResources::<4>::new()),
        0x1234_5678_9ABC_DEF0,
    );
    spawner.spawn(net_task(runner).unwrap());

    for attempt in 1..=3u8 {
        info!(
            "连接 WiFi: [{}] (第 {} 次)...",
            config.ssid.as_str(),
            attempt
        );
        if controller.connect_async().await.is_ok() {
            info!("WiFi 已连接!");
            stack.wait_config_up().await;
            // 控制器一并交还调用方保活 (丢弃可能影响射频状态; 断线重连策略后续接入)
            return Ok((stack, controller, mac));
        }
        error!("连接失败");
        embassy_time::Timer::after(embassy_time::Duration::from_secs(3)).await;
    }
    Err(())
}

#[embassy_executor::task]
pub async fn net_task(mut runner: embassy_net::Runner<'static, esp_radio::wifi::Interface>) {
    runner.run().await
}
