//! 配网模式 — SoftAP 热点 + DHCP + HTTP 配置页
//!
//! 自 wifi-test bin 抽出供主固件复用: 无凭据/连接失败时进入配网,
//! 手机连热点 `xiaozhi-gezipai`, 浏览器打开 http://192.168.4.1:8080/ 填表单。

use embassy_executor::Spawner;
use embassy_net::{Ipv4Address, Ipv4Cidr, Stack, StackResources, tcp, udp};
use esp_storage::FlashStorage;

use defmt::{error, info};

use alloc::format;

/// 配网 AP 的 IP
const AP_ADDR: [u8; 4] = [192, 168, 4, 1];
/// 表单未填服务器 IP 时的默认 xiaozhi 服务器 (局域网)
const DEFAULT_SERVER: &str = "192.168.123.14";

// 本模块不再重复注册堆: 调用方 (bin) 已注册 (主固件在 create_board 内注册)

/// 配网模式: SoftAP + DHCP + HTTP 表单 (HTTP 任务保存后直接重启设备)
pub async fn run(
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
    // 容量: DHCP UDP socket 1 + 并发 TCP 连接 7 (StackResources 按 socket 计数,
    // 超容量时 TcpSocket::new panic → 设备重启循环)
    let (stack, runner) = embassy_net::new(
        interface,
        static_cfg,
        crate::mk_static!(StackResources<8>, StackResources::<8>::new()),
        0x0FED_CBA9_8765_4321,
    );
    spawner.spawn(net_task(runner).unwrap());

    info!("配网模式: 热点 xiaozhi-gezipai, 配置页 http://192.168.4.1:8080/");
    spawner.spawn(dhcp_server(stack).unwrap());
    http_accept_loop(spawner, stack, flash).await
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

/// 嵌入资源: Vue3 运行时 + 配网页 (构建时打包进固件)
const VUE_JS: &str = include_str!("../../assets/wifi-config/vue.global.prod.js");
const INDEX_HTML: &str = include_str!("../../assets/wifi-config/index.html");

/// 发送完整 HTTP 响应 (头部 + 分块写 body + 官方验证过的关闭时序)
async fn serve_bytes(socket: &mut tcp::TcpSocket<'_>, ctype: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.0 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        ctype,
        body.len()
    );
    // 零拷贝分块写: 头部与正文直接从源切片写出 (大文件不进堆,
    // WiFi 协议栈已占用内部堆大部分, 158KB 级 Vec 会 OOM panic)
    let mut head_slice = head.as_str();
    while !head_slice.is_empty() {
        let n = socket
            .write_with(|buf| {
                let n = usize::min(buf.len(), head_slice.len());
                buf[..n].copy_from_slice(&head_slice.as_bytes()[..n]);
                (n, n)
            })
            .await
            .unwrap_or(0);
        if n == 0 {
            break;
        }
        head_slice = &head_slice[n..];
    }
    let mut body_slice = body;
    while !body_slice.is_empty() {
        let n = socket
            .write_with(|buf| {
                let n = usize::min(buf.len(), body_slice.len());
                buf[..n].copy_from_slice(&body_slice[..n]);
                (n, n)
            })
            .await
            .unwrap_or(0);
        if n == 0 {
            break;
        }
        body_slice = &body_slice[n..];
    }
    let _ = socket.flush().await;
    // 官方验证过的时序: flush 后延时, close 后延时, 再 abort
    embassy_time::Timer::after(embassy_time::Duration::from_millis(1000)).await;
    socket.close();
    embassy_time::Timer::after(embassy_time::Duration::from_millis(1000)).await;
    socket.abort();
}

/// HTTP 接受循环: 监听窗口最小化 — 连接一建立立即重建监听,
/// 页面/JS 请求放入池化任务并行处理 (浏览器紧随 HTML 的 /vue.js 请求不会被 RST)
async fn http_accept_loop(
    spawner: Spawner,
    stack: Stack<'static>,
    flash: &'static mut FlashStorage<'static>,
) -> ! {
    info!("HTTP 配置页就绪 (:8080)");
    loop {
        let rx = alloc::vec![0u8; 2048].leak();
        let tx = alloc::vec![0u8; 2048].leak();
        let mut socket = tcp::TcpSocket::new(stack, rx, tx);
        if socket.accept(8080).await.is_err() {
            embassy_time::Timer::after(embassy_time::Duration::from_millis(50)).await;
            continue;
        }
        info!("HTTP: 已接受连接");

        let mut request = [0u8; 1024];
        let n = socket.read(&mut request).await.unwrap_or(0);
        let req = core::str::from_utf8(&request[..n]).unwrap_or("");
        let first_line = req.lines().next().unwrap_or("");
        let path = first_line.split_whitespace().nth(1).unwrap_or("/");
        let (path_only, query) = path.split_once('?').unwrap_or((path, ""));
        info!("HTTP: 请求路径 {}", path_only);

        match path_only {
            "/save" => {
                // 保存需要 Flash (独占借用), 走同步路径 (毫秒级, 不影响后续监听)
                handle_save(&mut socket, query, flash).await;
            },
            _ => {
                // 页面与 Vue 运行时: 无 Flash 访问, 放入池化任务并行服务
                let path_owned = heapless::String::<32>::try_from(path_only).unwrap_or_default();
                spawner.spawn(http_asset_task(socket, path_owned).unwrap());
            },
        }
    }
}

/// 静态资源任务: 配网页 / Vue3 运行时 / 404
#[embassy_executor::task(pool_size = 4)]
async fn http_asset_task(mut socket: tcp::TcpSocket<'static>, path: heapless::String<32>) {
    match path.as_str() {
        "/vue.js" => {
            serve_bytes(
                &mut socket,
                "application/javascript; charset=utf-8",
                VUE_JS.as_bytes(),
            )
            .await;
        },
        "/" | "/index.html" => {
            serve_bytes(
                &mut socket,
                "text/html; charset=utf-8",
                INDEX_HTML.as_bytes(),
            )
            .await;
        },
        _ => {
            serve_bytes(&mut socket, "text/plain; charset=utf-8", b"404").await;
        },
    }
}

/// 处理 /save: 凭据写入 Flash 并优雅应答
async fn handle_save(socket: &mut tcp::TcpSocket<'_>, query: &str, flash: &mut FlashStorage<'_>) {
    let (ssid, pass, server) = parse_query(query);
    if ssid.is_empty() {
        serve_bytes(
            socket,
            "text/plain; charset=utf-8",
            "SSID 不能为空".as_bytes(),
        )
        .await;
        return;
    }
    let config = super::WifiConfig::new(&ssid, &pass, &server);
    match super::store::save(flash, &config) {
        Ok(()) => {
            info!(
                "配网成功: ssid={} 服务器={}",
                config.ssid.as_str(),
                config.server.as_str()
            );
            serve_bytes(
                socket,
                "text/plain; charset=utf-8",
                "已保存, 设备将重启并连接 WiFi".as_bytes(),
            )
            .await;
            // 响应已完整关闭, 重启进入 STA 模式
            esp_hal::system::software_reset();
        },
        Err(e) => {
            error!("保存失败: {}", e);
            serve_bytes(socket, "text/plain; charset=utf-8", "保存失败".as_bytes()).await;
        },
    }
}

/// 解析 query string (url-decode: %XX 与 +), 值去首尾空白
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
        server = heapless::String::try_from(DEFAULT_SERVER).unwrap_or_default();
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
