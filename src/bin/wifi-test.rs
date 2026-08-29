//! M4.0 调试: WiFi 配网 → 连接 → 服务器可达性验证
//!
//! 架构: esp-rtos 调度器 + esp-radio WiFi + embassy-net 0.8 (全异步)
//!
//! 流程:
//! 1. 读 Flash 凭据; 无 → 进入配网 (手机连热点 `xiaozhi-gezipai`,
//!    浏览器打开 http://192.168.4.1/ 填表单), 保存后自动重启
//! 2. STA 连接 (DHCP), 失败清凭据重回配网
//! 3. TCP 探测服务器 OTA 接口 (:8003), 打印响应
//!
//! ```bash
//! cargo run --release --bin wifi-test
//! ```

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![allow(clippy::too_many_arguments)]

use embassy_executor::Spawner;
use embassy_net::{Ipv4Address, StackResources, tcp};
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, Pull},
    interrupt::software::SoftwareInterruptControl,
    timer::timg::TimerGroup,
};
use esp_storage::FlashStorage;

use defmt::{error, info};
use esp_println as _;

use xiaozhi_s3::wifi::{self, Mode, store};

#[panic_handler]
fn panic(panic_info: &core::panic::PanicInfo) -> ! {
    error!("{}", panic_info);
    esp_hal::system::software_reset()
}

extern crate alloc;
// This creates a default app-descriptor required by the esp-idf bootloader.
esp_bootloader_esp_idf::esp_app_desc!();

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // 内部堆: WiFi 协议栈 + 任务栈 (本 bin 不含音频缓冲)
    esp_alloc::heap_allocator!(size: 160 * 1024);

    info!("=== WiFi 配网/连接调试 (M4.0) ===");

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    // 调度器必须先于 radio 启动
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);
    info!("esp-rtos 调度器已启动");

    let flash = mk_static!(FlashStorage<'static>, FlashStorage::new(peripherals.FLASH));

    if !store::self_test(flash) {
        error!("存储层自检失败!");
    }

    // BOOT 键 (GPIO1, 低电平有效): 开机按住 → 清除 WiFi 凭据, 强制进入配网
    let boot_btn = Input::new(
        peripherals.GPIO1,
        InputConfig::default().with_pull(Pull::Up),
    );
    if boot_btn.is_low() {
        info!("BOOT 按下: 清除 WiFi 配置...");
        let _ = store::clear(flash);
        embassy_time::Timer::after(embassy_time::Duration::from_millis(200)).await;
        info!("配置已清除, 进入配网模式");
    }

    match wifi::boot_mode(flash) {
        Mode::Station(config) => {
            info!(
                "启动模式: STA ssid={} 密码长度={} 服务器={}",
                config.ssid.as_str(),
                config.password.len(),
                config.server.as_str()
            );
            station_mode(spawner, config, peripherals.WIFI, flash).await;
        },
        Mode::Provision => {
            info!("启动模式: 配网 (Flash 无有效凭据)");
            wifi::provision::run(spawner, peripherals.WIFI, flash).await;
        },
    }
}

/// STA 模式: 连接 → DHCP → 探测服务器
async fn station_mode(
    spawner: Spawner,
    config: wifi::WifiConfig,
    wifi_peri: esp_hal::peripherals::WIFI<'static>,
    flash: &'static mut FlashStorage<'static>,
) -> ! {
    let interface = esp_radio::wifi::Interface::station();
    let mut controller = esp_radio::wifi::WifiController::new(
        wifi_peri,
        esp_radio::wifi::ControllerConfig::default().with_initial_config(config.station_config()),
    )
    .unwrap();

    let (stack, runner) = embassy_net::new(
        interface,
        embassy_net::Config::dhcpv4(Default::default()),
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        0x1234_5678_9ABC_DEF0,
    );
    spawner.spawn(wifi::net_task(runner).unwrap());

    // 连接 (重试 3 次, 失败清凭据重回配网)
    for attempt in 1..=3u8 {
        info!(
            "连接 WiFi: [{}] 长度 {} (第 {} 次)...",
            config.ssid.as_str(),
            config.ssid.len(),
            attempt
        );
        match controller.connect_async().await {
            Ok(_) => break,
            Err(_) => {
                error!("连接失败");
                embassy_time::Timer::after(embassy_time::Duration::from_secs(3)).await;
            },
        }
    }
    if !controller.is_connected() {
        error!("连接失败 3 次, 清除配置重回配网");
        let _ = store::clear(flash);
        esp_hal::system::software_reset();
    }
    info!("WiFi 已连接!");

    stack.wait_config_up().await;
    if let Some(ip_config) = stack.config_v4() {
        let o = ip_config.address.address().octets();
        info!("DHCP 分配: {}.{}.{}.{}", o[0], o[1], o[2], o[3]);
    }

    // 探测服务器 OTA 接口
    let server: Ipv4Address = match config.server.as_str().parse() {
        Ok(ip) => ip,
        Err(_) => {
            error!("服务器地址无效: {} (需为 IP)", config.server.as_str());
            esp_hal::system::software_reset();
        },
    };
    let so = server.octets();
    info!(
        "探测服务器 OTA 接口: {}.{}.{}.{}:8003",
        so[0], so[1], so[2], so[3]
    );

    let mut socket = tcp::TcpSocket::new(
        stack,
        mk_static!([u8; 2048], [0u8; 2048]),
        mk_static!([u8; 2048], [0u8; 2048]),
    );

    let mut request = heapless::String::<256>::new();
    let _ = core::fmt::write(
        &mut request,
        format_args!(
            "GET /xiaozhi/ota/ HTTP/1.1\r\nHost: {}.{}.{}.{}\r\nConnection: close\r\n\r\n",
            so[0], so[1], so[2], so[3]
        ),
    );

    loop {
        let remote = (server, 8003);
        match socket.connect(remote).await {
            Ok(()) => {},
            Err(_) => {
                error!("TCP 连接失败 — 5s 后重试");
                embassy_time::Timer::after(embassy_time::Duration::from_secs(5)).await;
                continue;
            },
        }

        let request_bytes = request.as_bytes();
        let _ = socket
            .write_with(|buf| {
                let n = request_bytes.len();
                buf[..n].copy_from_slice(request_bytes);
                (n, ())
            })
            .await;
        let _ = socket.flush().await;

        let mut response = heapless::String::<512>::new();
        loop {
            let mut buf = [0u8; 512];
            match socket.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    for b in &buf[..n] {
                        let _ = response.push(*b as char);
                    }
                    if response.len() >= response.capacity() {
                        break;
                    }
                },
                Err(_) => {
                    error!("接收失败");
                    break;
                },
            }
        }
        let head = response.lines().next().unwrap_or("");
        let body = response.split("\r\n\r\n").nth(1).unwrap_or("");
        info!("服务器响应: {} | {}", head, body);
        info!("=== 网络链路 OK (IP / 服务器均可达), M4.0 完成 ===");
        break;
    }

    socket.abort();
    // 保持运行
    loop {
        embassy_time::Timer::after(embassy_time::Duration::from_secs(10)).await;
    }
}
