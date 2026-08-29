//! STA 网络栈 — smoltcp Interface + DHCP 客户端 + TCP 阻塞工具
//!
//! 全阻塞设计: 每轮循环调用 `poll()` 驱动协议栈, TCP 请求用
//! `tcp_get()` 内部状态机 + 超时完成, 无需异步运行时。

use heapless::String;
use smoltcp::{
    iface::{Config, Interface, SocketSet},
    socket::dhcpv4,
    socket::tcp,
    wire::{EthernetAddress, HardwareAddress, IpAddress, Ipv4Address},
};
use esp_wifi::wifi::WifiDevice;

/// STA 网络栈
pub struct StaNet {
    iface: Interface,
    device: WifiDevice<'static>,
    sockets: SocketSet<'static>,
    dhcp: smoltcp::socket::SocketHandle,
    tcp: smoltcp::socket::SocketHandle,
}

impl StaNet {
    pub fn new(device: WifiDevice<'static>) -> Self {
        let mac = device.mac_address();
        let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
        let mut iface = Interface::new(config, &mut device, smoltcp::time::Instant::from_millis(0));

        let mut sockets = SocketSet::new(vec![]);
        let dhcp = sockets.add(dhcpv4::Socket::new());
        let mut tcp_socket = tcp::Socket::new(
            tcp::SocketBuffer::new(alloc::vec![0u8; 2048].into_boxed_slice()),
            tcp::SocketBuffer::new(alloc::vec![0u8; 2048].into_boxed_slice()),
        );
        tcp_socket.set_ack_delay(None);
        let tcp = sockets.add(tcp_socket);

        Self {
            iface,
            device,
            sockets,
            dhcp,
            tcp,
        }
    }

    /// 驱动协议栈 (每轮主循环调用); 返回 DHCP 分配到的 IPv4 地址
    pub fn poll(&mut self) -> Option<Ipv4Address> {
        let now = smoltcp::time::Instant::from_millis(super::now_ms());
        self.iface
            .poll(now, &mut self.device, &mut self.sockets);

        let event = self
            .sockets
            .get_mut::<dhcpv4::Socket>(self.dhcp)
            .poll();
        if let Some(dhcpv4::Event::Configured(config)) = event {
            let addr = config.address.address();
            self.iface.update_ip_addrs(|addrs| {
                addrs.clear();
                let _ = addrs.push(config.address);
            });
            if let Some(gw) = config.router {
                self.iface
                    .routes_mut()
                    .add_default_ipv4_route(gw)
                    .ok();
            }
            Some(addr)
        } else {
            None
        }
    }

    /// 当前 IP
    pub fn ip(&self) -> Option<Ipv4Address> {
        self.iface.ipv4_addr()
    }

    /// 阻塞式 TCP GET: 连接 → 发送请求 → 收响应, 直到服务端关闭或超时
    pub fn tcp_get(
        &mut self,
        host: Ipv4Address,
        port: u16,
        request: &str,
        timeout_ms: i64,
    ) -> Result<String<1024>, &'static str> {
        let socket = self.sockets.get_mut::<tcp::Socket>(self.tcp);
        socket.abort();
        socket
            .connect(
                self.iface.context(),
                (IpAddress::Ipv4(host), port),
                49152 + (super::now_ms() as u16 % 16384),
            )
            .map_err(|_| "TCP 连接参数错误")?;

        let deadline = super::now_ms() + timeout_ms;
        let mut sent = false;
        let mut response = String::<1024>::new();

        loop {
            if super::now_ms() > deadline {
                return Err("TCP 请求超时");
            }
            self.poll();

            let socket = self.sockets.get_mut::<tcp::Socket>(self.tcp);
            match socket.state() {
                tcp::State::Closed | tcp::State::TimeWait if response.is_empty() => {
                    return Err("TCP 连接被关闭");
                },
                _ => {},
            }

            if socket.may_send() && !sent {
                let _ = socket.send_slice(request.as_bytes());
                sent = true;
            }
            if socket.can_recv() {
                let mut buf = [0u8; 512];
                let n = socket.recv_slice(&mut buf).unwrap_or(0);
                for b in &buf[..n] {
                    let _ = response.push(*b as char);
                }
                if response.len() >= response.capacity() - 1 {
                    break;
                }
            }
            if sent && !socket.can_recv() && socket.may_recv() {
                // 等待更多数据
            }
            if sent && socket.state() == tcp::State::CloseWait {
                // 服务端已关闭: 响应完整
                break;
            }
            super::busy_wait_ms(2);
        }

        let socket = self.sockets.get_mut::<tcp::Socket>(self.tcp);
        socket.close();
        Ok(response)
    }
}
