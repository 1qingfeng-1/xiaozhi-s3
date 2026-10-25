//! 配网模式 — SoftAP 热点 + DHCP + HTTP 配置页
//!
//! 自 wifi-test bin 抽出供主固件复用: 无凭据/连接失败时进入配网,
//! 手机连热点 `xiaozhi-gezipai`, 浏览器打开 http://192.168.4.1:8080/ 填表单。

use embassy_executor::Spawner;
use embassy_net::{Ipv4Address, Ipv4Cidr, Stack, StackResources, tcp, udp};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Channel, Sender};
use embassy_time::{Duration, with_timeout};
use esp_storage::FlashStorage;

use defmt::{error, info, warn};

use alloc::format;

/// 配网 HTTP 单次读写上限: 防止恶意/异常客户端永久占住 socket
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// accept 等待上限: 超时后回收 socket, 避免半死监听
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);
/// 并发 HTTP 资源任务数与 socket 池大小 (必须一致)
const POOL_SIZE: usize = 4;
type SocketPool = Channel<CriticalSectionRawMutex, tcp::TcpSocket<'static>, POOL_SIZE>;

/// 从 PSRAM 分配 socket 缓冲 (失败回退内部堆)
///
/// 配网模式在音频启动前运行, 内部堆余量充足; 仍优先 PSRAM 以免占用
/// 后续会话所需的内部 RAM。
fn alloc_socket_buf(len: usize) -> &'static mut [u8] {
    use core::alloc::Layout;
    let layout = Layout::from_size_align(len, 4).expect("bad layout");
    // SAFETY: 非零布局, 分配块常驻
    let p =
        unsafe { esp_alloc::HEAP.alloc_caps(esp_alloc::MemoryCapability::External.into(), layout) };
    if p.is_null() {
        alloc::vec![0u8; len].leak()
    } else {
        // SAFETY: 可写 len 字节; 清零避免脏数据
        unsafe {
            core::ptr::write_bytes(p, 0, len);
            core::slice::from_raw_parts_mut(p, len)
        }
    }
}

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
    // 配网热点不再开放: 旁观者可被动抓包拿到家庭 WiFi 密码。
    // 密码为固定出厂值, 文档/启动日志会打印; 需要更强隔离时改为一次性 PIN。
    const AP_PASSWORD: &str = "xiaozhi-setup";
    let ap_config = esp_radio::wifi::Config::AccessPoint(
        esp_radio::wifi::ap::AccessPointConfig::default()
            .with_ssid("xiaozhi-gezipai")
            .with_password(alloc::string::String::from(AP_PASSWORD))
            .with_auth_method(esp_radio::wifi::AuthenticationMethod::Wpa2Personal),
    );
    let interface = esp_radio::wifi::Interface::access_point();
    let _controller = match esp_radio::wifi::WifiController::new(
        wifi_peri,
        esp_radio::wifi::ControllerConfig::default().with_initial_config(ap_config),
    ) {
        Ok(c) => c,
        Err(e) => {
            error!("配网: WiFi 控制器初始化失败 {:?} — 重启重试", e);
            esp_hal::system::software_reset();
        },
    };

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

    info!(
        "配网模式: 热点 xiaozhi-gezipai (密码 {}), 配置页 http://192.168.4.1:8080/",
        AP_PASSWORD
    );
    spawner.spawn(dhcp_server(stack).unwrap());

    // socket 池: 一次分配、跨连接复用; 每个连接不再 leak 缓冲
    let pool = crate::mk_static!(SocketPool, SocketPool::new());
    for _ in 0..POOL_SIZE {
        let rx = alloc_socket_buf(2048);
        let tx = alloc_socket_buf(2048);
        let _ = pool.try_send(tcp::TcpSocket::new(stack, rx, tx));
    }
    http_accept_loop(spawner, stack, flash, pool).await
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

        let reply = match xiaozhi_core::dhcp::build_reply(&buf[..n]) {
            Some(r) => r,
            None => continue,
        };
        let port = remote.endpoint.port;
        let _ = socket.send_to(&reply, (Ipv4Address::BROADCAST, port)).await;
        info!("DHCP 应答: {}B (类型 {})", reply.len(), reply[242]);
    }
}

/// 嵌入资源: Vue3 运行时 + 配网页 (构建时打包进固件)
const VUE_JS: &str = include_str!("../../assets/wifi-config/vue.global.prod.js");
const INDEX_HTML: &str = include_str!("../../assets/wifi-config/index.html");

/// 带超时的分块写: 客户端不读/半开时最多阻塞 `IO_TIMEOUT`
async fn write_all_bounded(socket: &mut tcp::TcpSocket<'_>, mut data: &[u8]) -> bool {
    while !data.is_empty() {
        let n = match with_timeout(
            IO_TIMEOUT,
            socket.write_with(|buf| {
                let n = usize::min(buf.len(), data.len());
                buf[..n].copy_from_slice(&data[..n]);
                (n, n)
            }),
        )
        .await
        {
            Ok(Ok(n)) if n > 0 => n,
            _ => return false,
        };
        data = &data[n..];
    }
    true
}

/// 发送完整 HTTP 响应 (头部 + 分块写 body + 官方验证过的关闭时序)
async fn serve_bytes(socket: &mut tcp::TcpSocket<'_>, ctype: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.0 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        ctype,
        body.len()
    );
    // 零拷贝分块写: 头部与正文直接从源切片写出 (大文件不进堆)
    if !write_all_bounded(socket, head.as_str().as_bytes()).await
        || !write_all_bounded(socket, body).await
    {
        socket.abort();
        return;
    }
    if !matches!(with_timeout(IO_TIMEOUT, socket.flush()).await, Ok(Ok(()))) {
        socket.abort();
        return;
    }
    // 官方验证过的时序: flush 后延时, close 后延时, 再 abort
    embassy_time::Timer::after(Duration::from_millis(1000)).await;
    socket.close();
    embassy_time::Timer::after(Duration::from_millis(1000)).await;
    socket.abort();
}

/// HTTP 接受循环: socket 从池中取用, 处理完归还, 不再每连接泄漏 4KB。
///
/// 并发上限 = `POOL_SIZE`; 池空时 accept 循环等待, 由 TCP 栈对超额连接
/// 反压/拒绝。任务池大小与 socket 池一致, 因此 `spawn` 不会 `Busy`。
async fn http_accept_loop(
    spawner: Spawner,
    _stack: Stack<'static>,
    flash: &'static mut FlashStorage<'static>,
    pool: &'static SocketPool,
) -> ! {
    info!("HTTP 配置页就绪 (:8080)");
    loop {
        let mut socket = pool.receive().await;
        match with_timeout(ACCEPT_TIMEOUT, socket.accept(8080)).await {
            Ok(Ok(())) => {},
            _ => {
                socket.abort();
                let _ = pool.try_send(socket);
                continue;
            },
        }
        info!("HTTP: 已接受连接");

        let mut request = [0u8; 1024];
        let n = match with_timeout(IO_TIMEOUT, socket.read(&mut request)).await {
            Ok(Ok(n)) => n,
            _ => {
                socket.abort();
                let _ = pool.try_send(socket);
                continue;
            },
        };
        let req = core::str::from_utf8(&request[..n]).unwrap_or("");
        let first_line = req.lines().next().unwrap_or("");
        let path = first_line.split_whitespace().nth(1).unwrap_or("/");
        let (path_only, query) = path.split_once('?').unwrap_or((path, ""));
        info!("HTTP: 请求路径 {}", path_only);

        match path_only {
            "/save" => {
                // 保存需要 Flash (独占借用), 走同步路径 (毫秒级, 不影响后续监听)
                handle_save(&mut socket, query, flash).await;
                socket.abort();
                let _ = pool.try_send(socket);
            },
            _ => {
                // 页面与 Vue 运行时: 无 Flash 访问, 放入池化任务并行服务
                let path_owned = heapless::String::<32>::try_from(path_only).unwrap_or_default();
                match http_asset_task(socket, path_owned, pool.sender()) {
                    Ok(token) => spawner.spawn(token),
                    // 正常不可达 (socket 池大小 == 任务池大小); 不 panic
                    Err(_) => warn!("配网: 资源任务池满, 丢弃连接"),
                }
            },
        }
    }
}

/// 静态资源任务: 配网页 / Vue3 运行时 / 404
#[embassy_executor::task(pool_size = 4)]
async fn http_asset_task(
    mut socket: tcp::TcpSocket<'static>,
    path: heapless::String<32>,
    pool: Sender<'static, CriticalSectionRawMutex, tcp::TcpSocket<'static>, POOL_SIZE>,
) {
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
    // 归还 socket (serve_bytes 已 abort), 供后续连接复用
    let _ = pool.send(socket).await;
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
    // 启动路径只接受 IPv4 字面量: 非法值会让 main 在 WiFi 连上后复位,
    // 形成「连上→复位」的无限循环 (只能 BOOT 长按恢复)。
    if server.parse::<Ipv4Address>().is_err() {
        serve_bytes(
            socket,
            "text/plain; charset=utf-8",
            "服务器地址必须是 IPv4, 例如 192.168.1.10".as_bytes(),
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
    let ssid = xiaozhi_core::http::form_field::<32>(query, "ssid").unwrap_or_default();
    let pass = xiaozhi_core::http::form_field::<64>(query, "pass").unwrap_or_default();
    let mut server = xiaozhi_core::http::form_field::<64>(query, "server").unwrap_or_default();
    if server.is_empty() {
        server = heapless::String::try_from(DEFAULT_SERVER).unwrap_or_default();
    }
    (ssid, pass, server)
}
