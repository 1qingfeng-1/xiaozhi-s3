//! M4.1 调试: WebSocket 协议层 — 握手 + hello + BOOT 按键 listen 消息
//!
//! 流程: 读 Flash 凭据 → STA 连接 → WebSocket 连接 xiaozhi 服务器 →
//! hello 握手 → 按住 BOOT 发送 listen start (对讲框架, opus 音频后续接入)
//!
//! ```bash
//! cargo run --release --bin xiaozhi-cloud --features wifi
//! ```

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

use embassy_executor::Spawner;
use embassy_net::{Ipv4Address, Stack, StackResources, tcp::TcpSocket};
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, Pull},
    interrupt::software::SoftwareInterruptControl,
    timer::timg::TimerGroup,
};
use esp_storage::FlashStorage;

use defmt::{error, info, warn};
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

/// WebSocket 服务器端口与路径 (xiaozhi 协议)
const WS_PORT: u16 = 8000;
const WS_PATH: &str = "/xiaozhi/v1/";

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(size: 160 * 1024);

    info!("=== xiaozhi 云连接调试 (M4.1 协议层) ===");

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    let flash = mk_static!(FlashStorage<'static>, FlashStorage::new(peripherals.FLASH));

    // BOOT 键: 清除配置强制重回配网
    let boot_btn = Input::new(
        peripherals.GPIO1,
        InputConfig::default().with_pull(Pull::Up),
    );
    if boot_btn.is_low() {
        info!("BOOT 按下: 清除 WiFi 配置...");
        let _ = store::clear(flash);
        embassy_time::Timer::after(embassy_time::Duration::from_millis(200)).await;
    }

    let config = match wifi::boot_mode(flash) {
        Mode::Station(config) => config,
        Mode::Provision => {
            error!("无 WiFi 凭据 — 请先运行 wifi-test 完成配网");
            loop {
                embassy_time::Timer::after(embassy_time::Duration::from_secs(10)).await;
            }
        },
    };

    // STA 连接 + 网络栈
    // Interface::station() 是单例 (二次调用 panic): 只取一次, MAC 先读后移交
    let interface = esp_radio::wifi::Interface::station();
    let mac_bytes = interface.mac_address();
    let mut controller = esp_radio::wifi::WifiController::new(
        peripherals.WIFI,
        esp_radio::wifi::ControllerConfig::default().with_initial_config(config.station_config()),
    )
    .unwrap();
    let (stack, runner) = embassy_net::new(
        interface,
        embassy_net::Config::dhcpv4(Default::default()),
        mk_static!(StackResources<4>, StackResources::<4>::new()),
        0x1234_5678_9ABC_DEF0,
    );
    spawner.spawn(net_task(runner).unwrap());

    let mut connected = false;
    for attempt in 1..=3u8 {
        info!(
            "连接 WiFi: [{}] (第 {} 次)...",
            config.ssid.as_str(),
            attempt
        );
        match controller.connect_async().await {
            Ok(_) => {
                connected = true;
                break;
            },
            Err(e) => error!("连接失败: {:?}", e),
        }
        embassy_time::Timer::after(embassy_time::Duration::from_secs(3)).await;
    }
    if !connected {
        error!("WiFi 连接失败 3 次, 清除配置重回配网");
        let _ = store::clear(flash);
        esp_hal::system::software_reset();
    }
    info!("WiFi 已连接!");
    stack.wait_config_up().await;
    let ip = stack
        .config_v4()
        .map(|c| c.address.address())
        .unwrap_or(Ipv4Address::new(0, 0, 0, 0));
    let o = ip.octets();
    info!("DHCP 分配: {}.{}.{}.{}", o[0], o[1], o[2], o[3]);

    let server: Ipv4Address = match config.server.as_str().parse() {
        Ok(ip) => ip,
        Err(_) => {
            error!("服务器地址无效: {}", config.server.as_str());
            esp_hal::system::software_reset();
        },
    };

    let mac = {
        let mut s = heapless::String::<32>::new();
        for (i, b) in mac_bytes.iter().enumerate() {
            if i > 0 {
                let _ = s.push_str(":");
            }
            let _ = core::fmt::write(&mut s, format_args!("{:02X}", b));
        }
        s
    };

    // WebSocket 重连循环
    loop {
        info!(
            "连接 WebSocket: {}:{}{}",
            config.server.as_str(),
            WS_PORT,
            WS_PATH
        );
        let mut socket = TcpSocket::new(
            stack,
            mk_static!([u8; 4096], [0u8; 4096]),
            mk_static!([u8; 4096], [0u8; 4096]),
        );
        match socket.connect((server, WS_PORT)).await {
            Ok(()) => {},
            Err(_) => {
                error!("WS TCP 连接失败 — 5s 后重试");
                embassy_time::Timer::after(embassy_time::Duration::from_secs(5)).await;
                continue;
            },
        }

        if let Err(e) = ws_handshake(&mut socket, &mac, config.server.as_str()).await {
            error!("WS 握手失败: {} — 5s 后重试", e);
            embassy_time::Timer::after(embassy_time::Duration::from_secs(5)).await;
            continue;
        }
        info!("WS 握手完成, 发送 hello...");

        let mut ws = WsClient { socket };
        if let Err(e) = ws_send_hello(&mut ws).await {
            error!("hello 发送失败: {}", e);
            continue;
        }

        // 等待服务器 hello
        match ws.recv_frame().await {
            Ok((opcode, payload)) => {
                if opcode == 1 {
                    info!(
                        "服务器 hello: {}",
                        core::str::from_utf8(&payload).unwrap_or("?")
                    );
                } else {
                    warn!("服务器首帧非文本 (opcode {})", opcode);
                }
            },
            Err(e) => {
                error!("服务器 hello 接收失败: {} — 重连", e);
                continue;
            },
        }

        info!("=== 协议层 OK — 按住 BOOT 说话 (listen start/stop, opus 后续接入) ===");
        session_loop(&mut ws, &boot_btn).await;
        // session_loop 返回 = 连接断开, 重连
        embassy_time::Timer::after(embassy_time::Duration::from_secs(2)).await;
    }
}

/// WebSocket 会话循环: BOOT 按键 → listen 消息, 服务器消息 → 日志
async fn session_loop(ws: &mut WsClient<'_>, boot_btn: &Input<'_>) {
    let mut held = false;
    loop {
        // 收服务器消息 (非阻塞轮询)
        if ws.socket.can_recv() {
            match ws.recv_frame().await {
                Ok((opcode, payload)) => match opcode {
                    1 => {
                        info!("服务器: {}", core::str::from_utf8(&payload).unwrap_or("?"));
                    },
                    2 => {
                        info!("服务器二进制帧: {}B (音频, opus 接入后处理)", payload.len());
                    },
                    8 => {
                        info!("服务器关闭连接");
                        return;
                    },
                    _ => {},
                },
                Err(e) => {
                    warn!("WS 接收错误: {}", e);
                    return;
                },
            }
        }

        // BOOT 按键 → listen 消息
        let pressed = boot_btn.is_low();
        if pressed != held {
            held = pressed;
            let msg = if pressed {
                "{\"type\":\"listen\",\"state\":\"start\"}"
            } else {
                "{\"type\":\"listen\",\"state\":\"stop\"}"
            };
            info!("发送: {}", msg);
            if let Err(e) = ws.send_text(msg.as_bytes()).await {
                error!("listen 发送失败: {}", e);
                return;
            }
        }
        embassy_time::Timer::after(embassy_time::Duration::from_millis(50)).await;
    }
}

/// 极简 WebSocket 客户端 (客户端帧强制掩码, 服务端帧不掩码)
struct WsClient<'a> {
    socket: TcpSocket<'a>,
}

impl WsClient<'_> {
    /// 发送文本帧
    async fn send_text(&mut self, payload: &[u8]) -> Result<(), &'static str> {
        self.send_frame(0x1, payload).await
    }

    /// 发送帧 (FIN + opcode, 掩码)
    async fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), &'static str> {
        if payload.len() > 1000 {
            return Err("帧过长");
        }
        let rng = esp_hal::rng::Rng::new();
        let key = [
            rng.random() as u8,
            (rng.random() >> 8) as u8,
            (rng.random() >> 16) as u8,
            (rng.random() >> 24) as u8,
        ];
        let mut frame: heapless::Vec<u8, 1024> = heapless::Vec::new();
        frame.push(0x80 | opcode).ok();
        let len = payload.len();
        if len < 126 {
            frame.push(0x80 | len as u8).ok();
        } else {
            frame.push(0x80 | 126).ok();
            frame.extend_from_slice(&(len as u16).to_be_bytes()).ok();
        }
        frame.extend_from_slice(&key).ok();
        for (i, b) in payload.iter().enumerate() {
            frame.push(b ^ key[i % 4]).ok();
        }
        ws_write_all(&mut self.socket, &frame).await
    }

    /// 接收一帧 (服务端帧不掩码), 返回 (opcode, payload)
    async fn recv_frame(&mut self) -> Result<(u8, heapless::Vec<u8, 1024>), &'static str> {
        let mut hdr = [0u8; 2];
        read_exact(&mut self.socket, &mut hdr)
            .await
            .map_err(|_| "读取帧头失败")?;
        let opcode = hdr[0] & 0x0F;
        let masked = hdr[1] & 0x80 != 0;
        let len0 = (hdr[1] & 0x7F) as usize;
        let len = match len0 {
            126 => {
                let mut ext = [0u8; 2];
                read_exact(&mut self.socket, &mut ext)
                    .await
                    .map_err(|_| "读取长度失败")?;
                u16::from_be_bytes(ext) as usize
            },
            127 => return Err("64 位长度不支持"),
            n => n,
        };
        if len > 1024 {
            return Err("帧过长");
        }
        let mut mask = [0u8; 4];
        if masked {
            read_exact(&mut self.socket, &mut mask)
                .await
                .map_err(|_| "读取掩码失败")?;
        }
        let mut payload = heapless::Vec::<u8, 1024>::new();
        payload.resize(len, 0).map_err(|_| "帧过长")?;
        if len > 0 {
            read_exact(&mut self.socket, &mut payload)
                .await
                .map_err(|_| "读取负载失败")?;
            if masked {
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= mask[i % 4];
                }
            }
        }
        Ok((opcode, payload))
    }
}

/// TCP 精确读取 N 字节
async fn read_exact(socket: &mut TcpSocket<'_>, buf: &mut [u8]) -> Result<(), ()> {
    let mut done = 0;
    while done < buf.len() {
        let n = socket.read(&mut buf[done..]).await.map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        done += n;
    }
    Ok(())
}

/// 循环写入全部字节
async fn ws_write_all(socket: &mut TcpSocket<'_>, mut data: &[u8]) -> Result<(), &'static str> {
    while !data.is_empty() {
        let n = socket
            .write_with(|buf| {
                let n = usize::min(buf.len(), data.len());
                buf[..n].copy_from_slice(&data[..n]);
                (n, n)
            })
            .await
            .map_err(|_| "写入失败")?;
        if n == 0 {
            return Err("写入零字节");
        }
        data = &data[n..];
    }
    Ok(())
}

/// WebSocket 握手 (HTTP Upgrade), 校验 101
async fn ws_handshake(
    socket: &mut TcpSocket<'static>,
    mac: &str,
    server: &str,
) -> Result<(), &'static str> {
    // Sec-WebSocket-Key: 16 随机字节的 base64
    let mut key_bytes = [0u8; 16];
    let rng = esp_hal::rng::Rng::new();
    for chunk in key_bytes.chunks_mut(4) {
        let v = rng.random().to_le_bytes();
        chunk.copy_from_slice(&v[..chunk.len()]);
    }
    let mut key_b64 = heapless::String::<32>::new();
    base64_encode(&key_bytes, &mut key_b64);

    let mut req = heapless::String::<512>::new();
    let _ = core::fmt::write(
        &mut req,
        format_args!(
            "GET {} HTTP/1.1\r\n\
             Host: {}:8000\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             Authorization: Bearer xiaozhi-test\r\n\
             Protocol-Version: 1\r\n\
             Device-Id: {}\r\n\
             Client-Id: {}\r\n\
             \r\n",
            WS_PATH,
            server,
            key_b64.as_str(),
            mac,
            mac
        ),
    );

    ws_write_all(socket, req.as_bytes())
        .await
        .map_err(|_| "握手请求发送失败")?;

    // 读响应头 (直到空行)
    let mut resp = heapless::Vec::<u8, 1024>::new();
    loop {
        let mut buf = [0u8; 256];
        let n = socket
            .read(&mut buf)
            .await
            .map_err(|_| "握手响应读取失败")?;
        if n == 0 {
            return Err("握手连接关闭");
        }
        resp.extend_from_slice(&buf[..n]).ok();
        if core::str::from_utf8(&resp)
            .unwrap_or("")
            .contains("\r\n\r\n")
        {
            break;
        }
        if resp.len() >= resp.capacity() - 1 {
            return Err("握手响应过长");
        }
    }
    let head = core::str::from_utf8(&resp).unwrap_or("");
    if !head.contains("101") {
        let status = head.lines().next().unwrap_or("");
        error!("WS 升级被拒: {}", status);
        return Err("WS 升级失败");
    }
    Ok(())
}

/// 发送 hello 消息 (xiaozhi 协议)
async fn ws_send_hello(ws: &mut WsClient<'_>) -> Result<(), &'static str> {
    ws.send_text(
        b"{\"type\":\"hello\",\"version\":1,\"features\":{\"mcp\":true},\
          \"transport\":\"websocket\",\"audio_params\":{\"format\":\"opus\",\
          \"sample_rate\":16000,\"channels\":1,\"frame_duration\":60}}",
    )
    .await
}

/// base64 编码 (标准字母表, 带填充)
fn base64_encode(input: &[u8], out: &mut heapless::String<32>) {
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let _ = out.push(TBL[(n >> 18) as usize & 63] as char);
        let _ = out.push(TBL[(n >> 12) as usize & 63] as char);
        let _ = out.push(if chunk.len() > 1 {
            TBL[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        let _ = out.push(if chunk.len() > 2 {
            TBL[n as usize & 63] as char
        } else {
            '='
        });
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, esp_radio::wifi::Interface>) {
    runner.run().await
}
