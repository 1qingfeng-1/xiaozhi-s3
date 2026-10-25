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

/// STA 连接失败原因 (决定调用方是否清除凭据)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectError {
    /// 关联/认证失败 (密码错误、AP 不存在): 清凭据重回配网
    Association,
    /// 关联成功但 DHCP 超时: 保留凭据, 稍后重启重试 (路由器 DHCP 池满等)
    DhcpTimeout,
}

/// STA 连接: 建网络栈 → 重试连接 (5 次) → 等 DHCP (30s 超时)
///
/// 返回网络栈供上层建会话; 失败原因区分「关联失败」与「DHCP 超时」,
/// 由调用方决定清凭据还是保留凭据重试。
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
    ConnectError,
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
        esp_radio::wifi::ControllerConfig::default()
            // 驱动 RX 队列默认仅 5 帧: TTS 下行/音频解码期间成串到达会丢包
            // (长句 UDP 断序的根因之一), 提到 16 帧给协议栈留余量
            .with_rx_queue_size(16)
            .with_initial_config(config.station_config()),
    )
    .map_err(|_| {
        error!("WiFi 控制器初始化失败");
        ConnectError::Association
    })?;

    let (stack, runner) = embassy_net::new(
        interface,
        embassy_net::Config::dhcpv4(Default::default()),
        // 6 = DHCP + OTA TCP + 会话 TCP (WS 或 MQTT) + UDP 音频 + 2 余量
        // (MQTT+UDP 模式同时占 TCP 与 UDP 两个 socket; 超容 new 会 panic)
        mk_static!(StackResources<6>, StackResources::<6>::new()),
        0x1234_5678_9ABC_DEF0,
    );
    spawner.spawn(net_task(runner).unwrap());

    // 5 次 × 5s: 覆盖路由器重启/DFS 切换等瞬断, 避免过早清凭据
    for attempt in 1..=5u8 {
        info!(
            "连接 WiFi: [{}] (第 {} 次)...",
            config.ssid.as_str(),
            attempt
        );
        let connected = matches!(
            embassy_time::with_timeout(
                embassy_time::Duration::from_secs(20),
                controller.connect_async(),
            )
            .await,
            Ok(Ok(_))
        );
        if connected {
            info!("WiFi 已连接!");
            // DHCP 无超时会让启动永久卡住 (此时看门狗尚未启用)
            match embassy_time::with_timeout(
                embassy_time::Duration::from_secs(30),
                stack.wait_config_up(),
            )
            .await
            {
                Ok(()) => {
                    // 控制器一并交还调用方保活 (丢弃可能影响射频状态)
                    return Ok((stack, controller, mac));
                },
                Err(_) => {
                    error!("WiFi 关联成功但 DHCP 获取超时 (30s)");
                    return Err(ConnectError::DhcpTimeout);
                },
            }
        }
        error!("连接失败");
        embassy_time::Timer::after(embassy_time::Duration::from_secs(5)).await;
    }
    Err(ConnectError::Association)
}

/// 运行期断线重连任务
///
/// 持有 [`esp_radio::wifi::WifiController`] 保活射频 (原先由 main 持有)。
/// 已连接时阻塞等待断线事件; 断线后每 3s 重试 `connect_async` 直到重新关联。
/// DHCP/链路状态由 embassy-net runner 自动处理; 会话任务通过
/// [`embassy_net::Stack::is_link_up`] 感知掉线并关闭当前 WS 会话。
pub static WIFI_AUDIO_ACTIVE: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
/// 省电模式已切到请求值 (音频会话前用它做“预切换完成”门控)
pub static WIFI_PS_READY: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

#[embassy_executor::task]
pub async fn reconnect_task(mut controller: esp_radio::wifi::WifiController<'static>) {
    use embassy_futures::select::{Either, select};
    use esp_radio::wifi::PowerSaveMode;

    let mut applied: Option<bool> = None; // Some(true)=省电关闭 (会话中)
    loop {
        if !controller.is_connected() {
            match controller.connect_async().await {
                Ok(_) => info!("WiFi 重连成功"),
                Err(e) => {
                    error!("WiFi 重连失败: {:?} — 3s 后重试", e);
                    embassy_time::Timer::after(embassy_time::Duration::from_secs(3)).await;
                    continue;
                },
            }
            applied = None;
            WIFI_PS_READY.store(false, core::sync::atomic::Ordering::Relaxed);
        }

        // 已连接: 等断线 + 每 100ms 检查省电请求
        loop {
            let want_none = WIFI_AUDIO_ACTIVE.load(core::sync::atomic::Ordering::Relaxed);
            if applied != Some(want_none) {
                let mode = if want_none {
                    PowerSaveMode::None
                } else {
                    PowerSaveMode::Minimum
                };
                match controller.set_power_saving(mode) {
                    Ok(()) => {
                        applied = Some(want_none);
                        WIFI_PS_READY.store(want_none, core::sync::atomic::Ordering::Relaxed);
                        info!(
                            "WiFi 省电: {}",
                            if want_none {
                                "关闭 (会话中)"
                            } else {
                                "Minimum (空闲)"
                            }
                        );
                    },
                    Err(e) => {
                        applied = Some(want_none);
                        WIFI_PS_READY.store(false, core::sync::atomic::Ordering::Relaxed);
                        error!("WiFi 省电模式设置失败: {:?}", e);
                    },
                }
            } else {
                WIFI_PS_READY.store(want_none, core::sync::atomic::Ordering::Relaxed);
            }

            match select(
                controller.wait_for_disconnect_async(),
                embassy_time::Timer::after(embassy_time::Duration::from_millis(500)),
            )
            .await
            {
                Either::First(Ok(info)) => {
                    info!("WiFi 已断开: reason={:?} rssi={}", info.reason, info.rssi);
                    WIFI_PS_READY.store(false, core::sync::atomic::Ordering::Relaxed);
                    applied = None;
                    break;
                },
                Either::First(Err(_)) => {
                    embassy_time::Timer::after(embassy_time::Duration::from_millis(500)).await;
                    applied = None;
                    WIFI_PS_READY.store(false, core::sync::atomic::Ordering::Relaxed);
                    break;
                },
                Either::Second(_) => {},
            }
        }
        embassy_time::Timer::after(embassy_time::Duration::from_millis(500)).await;
    }
}

#[embassy_executor::task]
pub async fn net_task(mut runner: embassy_net::Runner<'static, esp_radio::wifi::Interface>) {
    runner.run().await
}
