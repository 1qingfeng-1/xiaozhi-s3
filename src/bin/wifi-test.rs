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
use embassy_net::{Ipv4Address, Ipv4Cidr, Stack, StackResources, tcp, udp};
use esp_hal::{
    clock::CpuClock, interrupt::software::SoftwareInterruptControl, timer::timg::TimerGroup,
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
use alloc::format;

// This creates a default app-descriptor required by the esp-idf bootloader.
esp_bootloader_esp_idf::esp_app_desc!();

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

/// 配网 AP 的 IP
const AP_ADDR: [u8; 4] = [192, 168, 4, 1];

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
            provision_mode(spawner, peripherals.WIFI, flash).await;
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
    spawner.spawn(net_task(runner).unwrap());

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

/// 配网模式: SoftAP + DHCP + HTTP 表单 (HTTP 任务保存后直接重启设备)
async fn provision_mode(
    spawner: Spawner,
    wifi_peri: esp_hal::peripherals::WIFI<'static>,
    flash: &'static mut FlashStorage<'static>,
) -> ! {
    let ap_config = esp_radio::wifi::Config::AccessPoint(
        esp_radio::wifi::ap::AccessPointConfig::default().with_ssid("xiaozhi-gezipai"),
    );
    let interface = esp_radio::wifi::Interface::access_point();
    let _controller = esp_radio::wifi::WifiController::new(
        wifi_peri,
        esp_radio::wifi::ControllerConfig::default().with_initial_config(ap_config),
    )
    .unwrap();

    let static_cfg = embassy_net::Config::ipv4_static(embassy_net::StaticConfigV4 {
        address: Ipv4Cidr::new(Ipv4Address::from(AP_ADDR), 24),
        gateway: Some(Ipv4Address::from(AP_ADDR)),
        dns_servers: heapless::Vec::<Ipv4Address, 3>::new(),
    });
    let (stack, runner) = embassy_net::new(
        interface,
        static_cfg,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        0x0FED_CBA9_8765_4321,
    );
    spawner.spawn(net_task(runner).unwrap());

    info!("配网模式: 热点 xiaozhi-gezipai, 配置页 http://192.168.4.1/");
    spawner.spawn(dhcp_server(stack).unwrap());
    spawner.spawn(http_server(stack, flash).unwrap());

    loop {
        embassy_time::Timer::after(embassy_time::Duration::from_secs(3600)).await;
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, esp_radio::wifi::Interface>) {
    runner.run().await
}

/// 极简 DHCP 服务器: 固定租约 192.168.4.2 (手机连热点自动获取 IP)
#[embassy_executor::task]
async fn dhcp_server(stack: Stack<'static>) {
    let mut rx_meta = [embassy_net::udp::PacketMetadata::EMPTY; 4];
    let mut tx_meta = [embassy_net::udp::PacketMetadata::EMPTY; 4];
    let mut rx_buf = [0u8; 600];
    let mut tx_buf = [0u8; 600];
    let mut socket =
        udp::UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);
    socket.bind(67).unwrap();
    info!("DHCP 服务器就绪 (:67)");

    loop {
        let mut buf = [0u8; 600];
        let (n, remote) = match socket.recv_from(&mut buf).await {
            Ok(x) => x,
            Err(_) => {
                error!("DHCP recv 错误");
                continue;
            },
        };
        match remote.endpoint.addr {
            embassy_net::IpAddress::Ipv4(a) => {
                let o = a.octets();
                info!(
                    "DHCP 收包: {}B from {}.{}.{}.{}:{}",
                    n, o[0], o[1], o[2], o[3], remote.endpoint.port
                );
            },
        }

        let reply = match dhcp_build_reply(&buf[..n]) {
            Some(r) => r,
            None => continue,
        };
        let port = remote.endpoint.port;
        let _ = socket.send_to(&reply, (Ipv4Address::BROADCAST, port)).await;
        info!("DHCP 应答: {}B (类型 {})", reply.len(), reply[242]);
    }
}

/// 解析 DHCP 请求并构造 OFFER/ACK 响应
fn dhcp_build_reply(req: &[u8]) -> Option<heapless::Vec<u8, 320>> {
    if req.len() < 240 || req[0] != 1 {
        return None; // 非 BOOTREQUEST
    }
    let msg_type = dhcp_option(req, 53)?;
    let reply_type: u8 = match msg_type {
        1 => 2, // DISCOVER → OFFER
        3 => 5, // REQUEST → ACK
        _ => return None,
    };

    let client_ip = [192, 168, 4, 2];
    let ap_ip = [192, 168, 4, 1];

    let mut out: heapless::Vec<u8, 320> = heapless::Vec::new();
    out.extend_from_slice(&[2, 1, 6, 0]).ok()?; // op=BOOTREPLY, htype, hlen, hops
    out.extend_from_slice(&req[4..8]).ok()?; // xid
    out.extend_from_slice(&[0, 0, 0, 0]).ok()?; // secs(2) + flags(2)
    out.extend_from_slice(&req[12..16]).ok()?; // ciaddr
    out.extend_from_slice(&client_ip).ok()?; // yiaddr
    out.extend_from_slice(&ap_ip).ok()?; // siaddr
    out.extend_from_slice(&[0; 4]).ok()?; // giaddr
    out.extend_from_slice(&req[28..44]).ok()?; // chaddr
    out.extend_from_slice(&[0; 64]).ok()?; // sname
    out.extend_from_slice(&[0; 128]).ok()?; // file
    out.extend_from_slice(&[0x63, 0x82, 0x53, 0x63]).ok()?; // magic cookie

    let mut opt = |code: u8, data: &[u8]| -> Option<()> {
        out.extend_from_slice(&[code, data.len() as u8]).ok()?;
        out.extend_from_slice(data).ok()
    };
    opt(53, &[reply_type])?;
    opt(54, &ap_ip)?; // server identifier
    opt(51, &[0xFF, 0xFF, 0xFF, 0xFF])?; // lease: 无限
    opt(1, &[255, 255, 255, 0])?; // subnet mask
    opt(3, &ap_ip)?; // router
    // BOOTP 规定最小包长 300B: 不足则用 padding option (0) 填充, 客户端会丢弃短包
    while out.len() < 299 {
        out.extend_from_slice(&[0]).ok()?;
    }
    out.extend_from_slice(&[255]).ok()?; // end
    Some(out)
}

/// 在 DHCP options 区查找指定 option 的值
fn dhcp_option(req: &[u8], code: u8) -> Option<u8> {
    let mut i = 240;
    while i + 2 <= req.len() {
        let c = req[i];
        if c == 255 {
            return None;
        }
        let len = req[i + 1] as usize;
        if c == code {
            return req.get(i + 2).copied();
        }
        i += 2 + len;
    }
    None
}

/// 配网表单页
const FORM: &str = "<html><head><meta charset='utf-8'><title>xiaozhi 配网</title>\
<meta name='viewport' content='width=device-width,initial-scale=1'></head><body>\
<h3>xiaozhi 音箱配网</h3>\
<form action='/save'>\
WiFi 名称:<br><input name='ssid'><br>\
WiFi 密码:<br><input name='pass' type='password'><br>\
服务器地址:<br><input name='server' value='192.168.123.14'><br><br>\
<input type='submit' value='保存并连接'>\
</form><p>保存后设备将自动重启连接。</p></body></html>";

/// HTTP 配网服务: 表单 + 保存 (保存成功即软件重启)
#[embassy_executor::task]
async fn http_server(stack: Stack<'static>, flash: &'static mut FlashStorage<'static>) {
    let mut socket = tcp::TcpSocket::new(
        stack,
        mk_static!([u8; 1024], [0u8; 1024]),
        mk_static!([u8; 1024], [0u8; 1024]),
    );
    info!("HTTP 配置页就绪 (:8080)");

    loop {
        if socket.state() != tcp::State::Closed {
            socket.abort(); // 仅异常残留时复位
        }
        info!("HTTP: 等待连接 (:8080)...");
        if let Err(e) = socket.accept(8080).await {
            error!("HTTP: accept 失败");
            embassy_time::Timer::after(embassy_time::Duration::from_millis(100)).await;
            continue;
        }
        info!("HTTP: 已接受连接");

        let mut request = [0u8; 1024];
        let n = socket.read(&mut request).await.unwrap_or(0);
        info!("HTTP: 收到 {}B", n);
        let req = core::str::from_utf8(&request[..n]).unwrap_or("");
        // 打印完整请求头 (区分浏览器请求 vs 系统探针)
        for line in req.split("\r\n").take(6) {
            info!("HTTP> {}", line);
        }
        let first_line = req.lines().next().unwrap_or("");
        let path = first_line.split_whitespace().nth(1).unwrap_or("/");
        info!("HTTP: 请求路径 {}", path);

        if let Some(query) = path.strip_prefix("/save?") {
            let (ssid, pass, server) = parse_query(query);
            if ssid.is_empty() {
                let _ = socket
                    .write_with(|buf| {
                        let resp = "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\nSSID 不能为空";
                        buf[..resp.len()].copy_from_slice(resp.as_bytes());
                        (resp.len(), ())
                    })
                    .await;
                continue;
            }
            let config = wifi::WifiConfig::new(&ssid, &pass, &server);
            match store::save(flash, &config) {
                Ok(()) => {
                    info!(
                        "配网成功: ssid={} 服务器={}",
                        config.ssid.as_str(),
                        config.server.as_str()
                    );
                    let _ = socket
                        .write_with(|buf| {
                            let resp = "HTTP/1.0 200 OK\r\n\r\n已保存, 设备将重启并连接 WiFi";
                            buf[..resp.len()].copy_from_slice(resp.as_bytes());
                            (resp.len(), ())
                        })
                        .await;
                    let _ = socket.flush().await;
                    embassy_time::Timer::after(embassy_time::Duration::from_millis(1000)).await;
                    socket.close();
                    embassy_time::Timer::after(embassy_time::Duration::from_millis(1000)).await;
                    socket.abort();
                    // 响应已送达, 重启进入 STA 模式
                    esp_hal::system::software_reset();
                },
                Err(e) => {
                    error!("保存失败: {}", e);
                    let _ = socket
                        .write_with(|buf| {
                            let resp = "HTTP/1.0 200 OK\r\n\r\n保存失败";
                            buf[..resp.len()].copy_from_slice(resp.as_bytes());
                            (resp.len(), ())
                        })
                        .await;
                    let _ = socket.flush().await;
                    embassy_time::Timer::after(embassy_time::Duration::from_millis(1000)).await;
                    socket.close();
                    embassy_time::Timer::after(embassy_time::Duration::from_millis(1000)).await;
                    socket.abort();
                },
            }
        } else {
            info!("HTTP: 回送配网表单");
            // 官方验证过的格式: HTTP/1.0 + EOF 定界 + flush→延时→close→延时→abort
            let _ = socket
                .write_with(|buf| {
                    let resp = "HTTP/1.0 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\"
            <html><head><meta charset='utf-8'><title>xiaozhi 配网</title>\
<meta name='viewport' content='width=device-width,initial-scale=1'></head><body>\
<h3>xiaozhi 音箱配网</h3>\
<form action='/save'>\
WiFi 名称:<br><input name='ssid'><br>\
WiFi 密码:<br><input name='pass' type='password'><br>\
服务器地址:<br><input name='server' value='192.168.123.14'><br><br>\
<input type='submit' value='保存并连接'>\
</form><p>保存后设备将自动重启连接。</p></body></html>\r\n\
            ";
                    buf[..resp.len()].copy_from_slice(resp.as_bytes());
                    (resp.len(), ())
                })
                .await;
            let _ = socket.flush().await;
            embassy_time::Timer::after(embassy_time::Duration::from_millis(1000)).await;
            socket.close();
            embassy_time::Timer::after(embassy_time::Duration::from_millis(1000)).await;
            socket.abort();
        }
    }
}

/// 优雅关闭: flush → FIN → 等待连接完全关闭。
/// 任何 abort/RST 都会丢弃对端内核未读的响应数据 (表现为"连接被重置"),
/// 因此成功路径全程不使用 abort。
async fn graceful_close(socket: &mut tcp::TcpSocket<'_>) -> bool {
    let _ = socket.flush().await;
    socket.close();
    for _ in 0..200 {
        if socket.state() == tcp::State::Closed {
            return true;
        }
        embassy_time::Timer::after(embassy_time::Duration::from_millis(10)).await;
    }
    false // 对端未正常关闭 (2s 超时)
}

/// 解析 query string (url-decode: %XX 与 +)
fn parse_query(
    query: &str,
) -> (
    heapless::String<32>,
    heapless::String<64>,
    heapless::String<64>,
) {
    let mut ssid = heapless::String::<32>::new();
    let mut pass = heapless::String::<64>::new();
    let mut server = heapless::String::<64>::new();

    for pair in query.split('&') {
        let (k, v) = match pair.split_once('=') {
            Some(x) => x,
            None => continue,
        };
        let full = url_decode(v);
        let decoded = heapless::String::<64>::try_from(full.trim()).unwrap_or_default();
        match k {
            "ssid" => ssid = heapless::String::try_from(decoded.as_str()).unwrap_or_default(),
            "pass" => pass = decoded,
            "server" => server = decoded,
            _ => {},
        }
    }
    if server.is_empty() {
        server = heapless::String::try_from("192.168.123.14").unwrap_or_default();
    }
    (ssid, pass, server)
}

fn url_decode(s: &str) -> heapless::String<64> {
    let mut out = heapless::String::<64>::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                let _ = out.push(' ');
                i += 1;
            },
            b'%' if i + 2 < bytes.len() => {
                let hex = core::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz");
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    let _ = out.push(v as char);
                }
                i += 3;
            },
            c => {
                let _ = out.push(c as char);
                i += 1;
            },
        }
    }
    out
}
