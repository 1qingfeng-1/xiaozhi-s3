//! SoftAP 配网 — 手机连热点 `xiaozhi-gezipai`, 浏览器打开
//! http://192.168.4.1/ 填写 WiFi 与服务器信息。
//!
//! 内含最小 DHCP 服务器 (固定租约 192.168.4.2) 与微型 HTTP 服务,
//! 纯手写协议, 无额外依赖。保存成功后返回配置, 由调用方重连 STA。

use esp_storage::FlashStorage;
use esp_wifi::wifi::{
    AccessPointConfiguration, AuthMethod, Configuration, WifiController, WifiDevice,
};
use heapless::Vec;
use smoltcp::{
    iface::{Config, Interface, SocketSet},
    socket::{tcp, udp},
    wire::{EthernetAddress, HardwareAddress, IpAddress, Ipv4Address, Ipv4Cidr},
};

use super::store;
use super::WifiConfig;

const AP_IP: [u8; 4] = [192, 168, 4, 1];
const CLIENT_IP: [u8; 4] = [192, 168, 4, 2];

const FORM: &str = "<html><head><meta charset='utf-8'><title>xiaozhi 配网</title>\
<meta name='viewport' content='width=device-width,initial-scale=1'></head><body>\
<h3>xiaozhi 音箱配网</h3>\
<form action='/save'>\
WiFi 名称:<br><input name='ssid'><br>\
WiFi 密码:<br><input name='pass' type='password'><br>\
服务器地址:<br><input name='server' value='192.168.123.14'><br><br>\
<input type='submit' value='保存并连接'>\
</form><p>保存后设备将自动重启连接。</p></body></html>";

/// 进入配网模式 (阻塞直到手机提交配置)
///
/// 返回 Some(config) 表示配网成功 (已存 Flash), 由调用方重连 STA。
pub fn run(
    controller: &mut WifiController<'static>,
    ap: WifiDevice<'static>,
    flash: &mut FlashStorage,
) -> Option<WifiConfig> {
    // 1. 启动 SoftAP
    let ap_config = AccessPointConfiguration {
        ssid: heapless::String::try_from("xiaozhi-gezipai").ok()?,
        auth_method: AuthMethod::None,
        ..Default::default()
    };
    controller
        .set_configuration(&Configuration::AccessPoint(ap_config))
        .ok()?;
    controller.start().ok()?;

    // 2. AP 网络栈: 静态 IP 192.168.4.1/24
    let mac = ap.mac_address();
    let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
    let mut iface = Interface::new(config, &mut ap, smoltcp::time::Instant::from_millis(0));
    iface.update_ip_addrs(|addrs| {
        let _ = addrs.push(Ipv4Cidr::new(
            IpAddress::v4(AP_IP[0], AP_IP[1], AP_IP[2], AP_IP[3]),
            24,
        ));
    });

    // 3. DHCP 服务器 (UDP :67) + HTTP 服务 (TCP :80)
    let mut sockets = SocketSet::new(vec![]);
    let mut dhcp_socket = udp::Socket::new(
        udp::PacketBuffer::new(vec![], alloc::vec![0u8; 600].into_boxed_slice()),
        udp::PacketBuffer::new(vec![], alloc::vec![0u8; 600].into_boxed_slice()),
    );
    dhcp_socket
        .bind((IpAddress::Ipv4(Ipv4Address::UNSPECIFIED), 67))
        .ok()?;
    let dhcp_handle = sockets.add(dhcp_socket);

    let mut http_socket = tcp::Socket::new(
        tcp::SocketBuffer::new(alloc::vec![0u8; 1024].into_boxed_slice()),
        tcp::SocketBuffer::new(alloc::vec![0u8; 1024].into_boxed_slice()),
    );
    http_socket.listen(80).ok()?;
    let http_handle = sockets.add(http_socket);

    defmt::info!("配网模式: 热点 xiaozhi-gezipai, 配置页 http://192.168.4.1/");

    // 4. 服务循环
    let mut saved: Option<WifiConfig> = None;
    loop {
        let now = smoltcp::time::Instant::from_millis(super::now_ms());
        iface.poll(now, &mut ap, &mut sockets);

        dhcp_serve(&mut sockets, dhcp_handle);

        if saved.is_none() {
            if let Some(cfg) = http_serve(&mut sockets, http_handle, flash) {
                saved = Some(cfg);
            }
        } else {
            // 响应已入队, 等 TCP 发送完毕后返回
            let http = sockets.get_mut::<tcp::Socket>(http_handle);
            if http.send_queue() == 0 {
                return saved;
            }
        }
        super::busy_wait_ms(2);
    }
}

/// 极简 DHCP 服务器: 固定租约 192.168.4.2
fn dhcp_serve(sockets: &mut SocketSet<'static>, handle: smoltcp::socket::SocketHandle) {
    let socket = sockets.get_mut::<udp::Socket>(handle);
    let (data, remote) = match socket.recv() {
        Ok(x) => x,
        Err(_) => return,
    };

    let reply = match dhcp_build_reply(data) {
        Some(r) => r,
        None => return,
    };
    let _ = socket.send_slice(
        &reply,
        (IpAddress::Ipv4(Ipv4Address::BROADCAST), 68).into(),
    );
    defmt::debug!("DHCP: {} ← {}B", remote.endpoint.addr, reply.len());
}

/// 解析 DHCP 请求并构造 OFFER/ACK 响应
fn dhcp_build_reply(req: &[u8]) -> Option<Vec<u8, 320>> {
    if req.len() < 240 || req[0] != 1 {
        return None; // 非 BOOTREQUEST
    }
    let msg_type = dhcp_option(req, 53)?;
    let reply_type: u8 = match msg_type {
        1 => 2, // DISCOVER → OFFER
        3 => 5, // REQUEST → ACK
        _ => return None,
    };

    let mut out: Vec<u8, 320> = Vec::new();
    // 固定头 236B
    out.extend_from_slice(&[2, 1, 6, 0]).ok()?; // op=BOOTREPLY, htype=eth, hlen=6
    out.extend_from_slice(&req[4..8]).ok()?; // xid
    out.extend_from_slice(&[0, 0]).ok()?; // secs/flags
    out.extend_from_slice(&req[12..16]).ok()?; // ciaddr
    out.extend_from_slice(&CLIENT_IP).ok()?; // yiaddr
    out.extend_from_slice(&AP_IP).ok()?; // siaddr
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
    opt(54, &AP_IP)?; // server identifier
    opt(51, &[0xFF, 0xFF, 0xFF, 0xFF])?; // lease: 无限
    opt(1, &[255, 255, 255, 0])?; // subnet mask
    opt(3, &AP_IP)?; // router
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

/// 微型 HTTP 服务: 表单 + 保存; 返回 Some(config) 表示已保存
fn http_serve(
    sockets: &mut SocketSet<'static>,
    handle: smoltcp::socket::SocketHandle,
    flash: &mut FlashStorage,
) -> Option<WifiConfig> {
    let socket = sockets.get_mut::<tcp::Socket>(handle);

    if !socket.is_active() && !socket.is_listening() {
        socket.listen(80).ok()?;
    }
    if !socket.can_recv() {
        return None;
    }

    let mut request = [0u8; 1024];
    let n = socket.recv_slice(&mut request).unwrap_or(0);
    let req = core::str::from_utf8(&request[..n]).unwrap_or("");
    let first_line = req.lines().next().unwrap_or("");
    let path = first_line.split_whitespace().nth(1).unwrap_or("/");

    if let Some(query) = path.strip_prefix("/save?") {
        let (ssid, pass, server) = parse_query(query);
        if ssid.is_empty() {
            let _ = socket.send_slice(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\nSSID 不能为空");
            return None;
        }
        let config = WifiConfig::new(&ssid, &pass, &server);
        match store::save(flash, &config) {
            Ok(()) => {
                let _ = socket.send_slice(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n已保存, 设备将重启并连接 WiFi");
                defmt::info!("配网成功: ssid={}", config.ssid.as_str());
                Some(config)
            },
            Err(e) => {
                let mut body = heapless::String::<128>::new();
                let _ = core::fmt::write(
                    &mut body,
                    format_args!("HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n保存失败: {}", e),
                );
                let _ = socket.send_slice(body.as_bytes());
                None
            },
        }
    } else {
        let mut page = heapless::String::<512>::new();
        let _ = core::fmt::write(
            &mut page,
            format_args!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                FORM.len(),
                FORM
            ),
        );
        let _ = socket.send_slice(page.as_bytes());
        None
    }
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
        let decoded = url_decode(v);
        match k {
            "ssid" => ssid = decoded,
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
