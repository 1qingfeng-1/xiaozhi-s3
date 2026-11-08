//! WebSocket 会话传输层 (xiaozhi 协议版本 1/2/3)
//!
//! 版本 1 的二进制帧 = 裸 opus; 版本 2/3 帧头由
//! [`xiaozhi_core::protocol`] 编解码 (v2 携带服务端 AEC 时间戳)。
//! 只做传输: 帧编解码 / 握手 / hello。会话生命周期与状态机见 [`crate::app`]。

use embassy_net::{Ipv4Address, tcp::TcpSocket};
use esp_hal::rng::Rng;

use crate::audio::opus::MAX_PACKET_BYTES;

/// WebSocket 服务器端口与路径 (xiaozhi 协议)
pub const WS_PORT: u16 = 8000;
pub const WS_PATH: &str = "/xiaozhi/v1/";

/// WS Bearer token: 默认兼容官方测试值, 可用构建期
/// `XIAOZHI_WS_TOKEN=... cargo build` 覆盖 (不把部署 token 写进源码)
const WS_TOKEN: &str = match option_env!("XIAOZHI_WS_TOKEN") {
    Some(t) => t,
    None => "xiaozhi-test",
};

/// WebSocket 连接参数 (可由 OTA 下发配置覆盖默认地址)
///
/// 默认由 [`WsEndpoint::default_for`] 构造 (配置 `server` + :8000 + 默认路径);
/// OTA 清单 `websocket.url` 可用 [`parse_endpoint`] 解析后替换。
#[derive(Clone, PartialEq, Eq)]
pub struct WsEndpoint {
    pub host: Ipv4Address,
    pub port: u16,
    /// 请求路径 (含前导 `/`)
    pub path: heapless::String<64>,
    /// 原始 token (不含 `Bearer `; 握手时统一补前缀)
    pub token: heapless::String<96>,
    /// xiaozhi 协议版本 (1/2/3)
    pub version: u8,
}

impl WsEndpoint {
    /// 默认端点: 配置服务器 + 默认端口/路径/token
    pub fn default_for(host: Ipv4Address, version: u16) -> Self {
        let version = version.clamp(VERSION_MIN, VERSION_MAX) as u8;
        Self {
            host,
            port: WS_PORT,
            path: heapless::String::try_from(WS_PATH).unwrap_or_default(),
            token: heapless::String::try_from(WS_TOKEN).unwrap_or_default(),
            version,
        }
    }
}

/// 解析 OTA 下发的 `websocket.url` (当前仅支持 `ws://<ipv4>[:port][/path]`)
///
/// - 端口缺省 8000, 路径缺省 [`WS_PATH`];
/// - `token` 若已带 `Bearer ` 前缀则去掉 (握手时统一补), 空 token 用默认值;
/// - `version` 只接受 1/2/3, 非法返回 None (调用方回退默认端点)。
///
/// 域名/`wss://` 暂不支持: 自建服务端用 IPv4 即可; 需要时再走 DNS/加密。
pub fn parse_endpoint(url: &str, token: &str, version: u8) -> Option<WsEndpoint> {
    if !(VERSION_MIN..=VERSION_MAX).contains(&u16::from(version)) {
        return None;
    }
    // 只接受 ws://, 不做隐式 scheme 推断
    let rest = url.trim().strip_prefix("ws://")?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, WS_PATH),
    };
    if hostport.is_empty() {
        return None;
    }
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => (h, p.parse::<u16>().ok()?),
        Some(_) => return None,
        None => (hostport, WS_PORT),
    };
    let host: Ipv4Address = host.parse().ok()?;
    // 路径来自网络, 拒绝控制字符/空白 (防请求行注入)
    if path.bytes().any(|b| b <= 0x20 || b == 0x7F) {
        return None;
    }
    let path = heapless::String::<64>::try_from(path).ok()?;

    let token = token.trim();
    let token = token
        .strip_prefix("Bearer ")
        .or_else(|| token.strip_prefix("bearer "))
        .unwrap_or(token)
        .trim_start();
    let token = if token.is_empty() { WS_TOKEN } else { token };

    Some(WsEndpoint {
        host,
        port,
        path,
        token: heapless::String::try_from(token).ok()?,
        version,
    })
}

/// WS 帧载荷上限: opus 最大包 1276B, 但 MCP/tools 文本可达数 KB
const RECV_BUF: usize = 4096;
/// 单帧读/写上限: 防止一条慢帧把会话任务卡住 10s (上行通道只有 ~1.4s)
const FRAME_TIMEOUT: embassy_time::Duration = embassy_time::Duration::from_millis(1_500);

/// listen/tts 消息模板 (xiaozhi 协议; mode 语义对齐 C++ protocol.cc)
pub const LISTEN_START_MANUAL: &str =
    "{\"type\":\"listen\",\"state\":\"start\",\"mode\":\"manual\"}";
pub const LISTEN_START_AUTO: &str = "{\"type\":\"listen\",\"state\":\"start\",\"mode\":\"auto\"}";
pub const LISTEN_STOP: &str = "{\"type\":\"listen\",\"state\":\"stop\"}";
/// 打断播报: 协议里是独立的 `abort` 类型 (对齐 C++ protocol.cc / 服务端
/// textMessageHandlerRegistry), **不是** `tts` 的 state=abort —— 后者服务端
/// 会当成未知消息忽略, TTS 不会真正停止。
pub const TTS_ABORT: &str = "{\"type\":\"abort\"}";
/// 设备主动结束会话 (MQTT 网关据此关闭 bridge; WS 服务端忽略)
pub const GOODBYE: &str = "{\"type\":\"goodbye\"}";

/// 允许的协议版本范围 (v1 裸 opus / v2 16B 头含时间戳 / v3 4B 头)
pub use xiaozhi_core::protocol::{VERSION_MAX, VERSION_MIN};

/// 构造 hello 消息
///
/// `features.mcp` 宣告 MCP 设备控制 (服务器随后下发 initialize/tools_list 握手);
/// `features.aec` 仅在版本 ≥2 时置 true —— 此时上行帧带时间戳, 服务器可据此
/// 做回声消除 (服务端 AEC)。audio_params 与上行编码器一致。
pub fn hello_message(version: u16) -> heapless::String<320> {
    xiaozhi_core::protocol::hello_message(version)
}

/// 极简 WebSocket 客户端 (客户端帧强制掩码, 服务端帧不掩码)
///
/// 借用外部 TcpSocket: 会话任务持有 socket 并跨重连复用, 避免每次
/// connect 泄漏缓冲耗尽内部堆。
pub struct WsClient<'a> {
    pub socket: &'a mut TcpSocket<'static>,
}

impl WsClient<'_> {
    /// 发送文本帧
    pub async fn send_text(&mut self, payload: &[u8]) -> Result<(), &'static str> {
        self.send_frame(0x1, payload).await
    }

    /// 回复 ping (pong = opcode 0xA, 载荷原样返回)
    pub async fn send_pong(&mut self, payload: &[u8]) -> Result<(), &'static str> {
        self.send_frame(0xA, payload).await
    }

    /// 发送上行音频帧 (按协议版本加帧头)
    ///
    /// v1: 裸 opus; v2: 16B 头 (含时间戳); v3: 4B 头。
    pub async fn send_audio(
        &mut self,
        version: u16,
        timestamp: u32,
        opus: &[u8],
    ) -> Result<(), &'static str> {
        match version {
            2 => {
                let mut buf = [0u8; MAX_PACKET_BYTES + xiaozhi_core::protocol::V2_HEADER_LEN];
                let n = xiaozhi_core::protocol::encode_v2(timestamp, opus, &mut buf)
                    .map_err(|_| "v2 组帧失败")?;
                self.send_frame(2, &buf[..n]).await
            },
            3 => {
                let mut buf = [0u8; MAX_PACKET_BYTES + xiaozhi_core::protocol::V3_HEADER_LEN];
                let n = xiaozhi_core::protocol::encode_v3(
                    xiaozhi_core::protocol::MSG_TYPE_OPUS as u8,
                    opus,
                    &mut buf,
                )
                .map_err(|_| "v3 组帧失败")?;
                self.send_frame(2, &buf[..n]).await
            },
            _ => self.send_frame(2, opus).await,
        }
    }

    /// 发送帧 (FIN + opcode, 掩码); 载荷上限 = opus 单包最大值
    pub async fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), &'static str> {
        if payload.len() > MAX_PACKET_BYTES {
            return Err("帧过长");
        }
        let rng = Rng::new();
        let key = [
            rng.random() as u8,
            (rng.random() >> 8) as u8,
            (rng.random() >> 16) as u8,
            (rng.random() >> 24) as u8,
        ];
        // 栈上定长: 不把 ~2KB 的 heapless::Vec 跨 await 放进 future
        let mut frame = [0u8; MAX_PACKET_BYTES + xiaozhi_core::ws::MAX_HEADER];
        let n = xiaozhi_core::ws::encode_client_frame(opcode, payload, key, &mut frame)
            .map_err(|e| e.describe())?;
        write_all(self.socket, &frame[..n], FRAME_TIMEOUT).await
    }

    /// 接收一帧 (服务端帧不掩码), 返回 (opcode, payload)
    pub async fn recv_frame(&mut self) -> Result<(u8, heapless::Vec<u8, RECV_BUF>), &'static str> {
        self.recv_frame_timeout(FRAME_TIMEOUT).await
    }

    /// 带自定义超时的接收 (握手/hello 用较长超时)
    async fn recv_frame_timeout(
        &mut self,
        timeout: embassy_time::Duration,
    ) -> Result<(u8, heapless::Vec<u8, RECV_BUF>), &'static str> {
        let mut first = [0u8; 2];
        read_exact(self.socket, &mut first, timeout)
            .await
            .map_err(|_| "读取帧头失败")?;
        // 先算完整头长度, 再一次性读扩展长度/掩码
        let hlen = xiaozhi_core::ws::header_len(&first).map_err(|e| e.describe())?;
        let mut head = [0u8; xiaozhi_core::ws::MAX_HEADER];
        head[..2].copy_from_slice(&first);
        if hlen > 2 {
            read_exact(self.socket, &mut head[2..hlen], timeout)
                .await
                .map_err(|_| "读取帧头失败")?;
        }
        let fh =
            xiaozhi_core::ws::parse_header(&head[..hlen], RECV_BUF).map_err(|e| e.describe())?;
        let mut payload = heapless::Vec::<u8, RECV_BUF>::new();
        payload.resize(fh.payload_len, 0).map_err(|_| "帧过长")?;
        if fh.payload_len > 0 {
            read_exact(self.socket, &mut payload, timeout)
                .await
                .map_err(|_| "读取负载失败")?;
            if fh.masked {
                xiaozhi_core::ws::unmask(&mut payload, fh.mask);
            }
        }
        Ok((fh.opcode, payload))
    }
}

impl crate::control::ControlLink for WsClient<'_> {
    async fn send_control(&mut self, text: &str) -> Result<(), &'static str> {
        WsClient::send_text(self, text.as_bytes()).await
    }
}

/// 解析下行音频帧, 返回 (时间戳, opus 载荷)
///
/// 版本 1 无帧头 (时间戳为 0); 版本 2/3 按帧头切分并校验长度。
pub fn decode_audio(version: u16, payload: &[u8]) -> Result<(u32, &[u8]), &'static str> {
    use xiaozhi_core::protocol as p;
    match version {
        2 => {
            let f = p::decode_v2(payload).map_err(|_| "v2 帧解析失败")?;
            if f.msg_type != p::MSG_TYPE_OPUS {
                return Err("v2 帧非音频");
            }
            Ok((f.timestamp, f.payload))
        },
        3 => {
            let f = p::decode_v3(payload).map_err(|_| "v3 帧解析失败")?;
            if f.msg_type != p::MSG_TYPE_OPUS as u8 {
                return Err("v3 帧非音频");
            }
            Ok((0, f.payload))
        },
        _ => Ok((0, payload)),
    }
}

/// 单次网络读/写等待上限: 半开连接时让会话任务能退出重连
const IO_TIMEOUT: embassy_time::Duration = embassy_time::Duration::from_secs(10);

/// TCP 精确读取 N 字节 (每次 read 的等待上限由调用方指定)
async fn read_exact(
    socket: &mut TcpSocket<'_>,
    buf: &mut [u8],
    timeout: embassy_time::Duration,
) -> Result<(), ()> {
    let mut done = 0;
    while done < buf.len() {
        let n = embassy_time::with_timeout(timeout, socket.read(&mut buf[done..]))
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        done += n;
    }
    Ok(())
}

/// 循环写入全部字节 (每次 write 的等待上限由调用方指定)
async fn write_all(
    socket: &mut TcpSocket<'_>,
    mut data: &[u8],
    timeout: embassy_time::Duration,
) -> Result<(), &'static str> {
    while !data.is_empty() {
        let n = embassy_time::with_timeout(
            timeout,
            socket.write_with(|buf| {
                let n = usize::min(buf.len(), data.len());
                buf[..n].copy_from_slice(&data[..n]);
                (n, n)
            }),
        )
        .await
        .map_err(|_| "写入超时")?
        .map_err(|_| "写入失败")?;
        if n == 0 {
            return Err("写入零字节");
        }
        data = &data[n..];
    }
    Ok(())
}

/// 在既有 socket 上建立会话: TCP 连接 → WS 握手 → 发送 hello → 等待服务器 hello
///
/// socket 由调用方持有并跨重连复用 (每次 connect 都 leak 缓冲会耗尽内部堆)。
/// 返回服务器音频参数: 采样率 / 帧长 ms。
pub async fn connect(
    socket: &mut TcpSocket<'static>,
    endpoint: &WsEndpoint,
    mac: &str,
) -> Result<(u32, u32), &'static str> {
    let version = u16::from(endpoint.version).clamp(VERSION_MIN, VERSION_MAX);
    socket.abort();
    socket
        .connect((endpoint.host, endpoint.port))
        .await
        .map_err(|_| "WS TCP 连接失败")?;

    handshake(socket, mac, endpoint).await?;

    let mut ws = WsClient { socket };
    let hello = hello_message(version);
    ws.send_text(hello.as_bytes()).await?;
    let (opcode, payload) = ws
        .recv_frame_timeout(IO_TIMEOUT)
        .await
        .map_err(|_| "服务器 hello 接收失败")?;
    if opcode != 1 {
        return Err("服务器首帧非文本");
    }
    let text = core::str::from_utf8(&payload).map_err(|_| "hello 非 UTF-8")?;
    Ok((
        json_num_field(text, "sample_rate").unwrap_or(24_000),
        json_num_field(text, "frame_duration").unwrap_or(60),
    ))
}

/// WebSocket 握手 (HTTP Upgrade), 校验 101
async fn handshake(
    socket: &mut TcpSocket<'static>,
    mac: &str,
    endpoint: &WsEndpoint,
) -> Result<(), &'static str> {
    // Sec-WebSocket-Key: 16 随机字节的 base64
    let mut key_bytes = [0u8; 16];
    let rng = Rng::new();
    for chunk in key_bytes.chunks_mut(4) {
        let v = rng.random().to_le_bytes();
        chunk.copy_from_slice(&v[..chunk.len()]);
    }
    let mut key_b64 = heapless::String::<32>::new();
    xiaozhi_core::http::base64_encode(&key_bytes, &mut key_b64);

    let mut req = heapless::String::<512>::new();
    let _ = core::fmt::write(
        &mut req,
        format_args!(
            "GET {} HTTP/1.1\r\n\
             Host: {}:{}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             Authorization: Bearer {}\r\n\
             Protocol-Version: {}\r\n\
             Device-Id: {}\r\n\
             Client-Id: {}\r\n\
             \r\n",
            endpoint.path.as_str(),
            endpoint.host,
            endpoint.port,
            key_b64.as_str(),
            endpoint.token.as_str(),
            endpoint.version,
            mac,
            mac
        ),
    );

    write_all(socket, req.as_bytes(), IO_TIMEOUT)
        .await
        .map_err(|_| "握手请求发送失败")?;

    // 读响应头 (直到空行)
    let mut resp = heapless::Vec::<u8, 1024>::new();
    loop {
        let mut buf = [0u8; 256];
        let n = embassy_time::with_timeout(IO_TIMEOUT, socket.read(&mut buf))
            .await
            .map_err(|_| "握手响应超时")?
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

    // 严格校验状态行 (不能只 contains("101"))
    let status = head.lines().next().unwrap_or("");
    let mut parts = status.split_whitespace();
    let proto = parts.next().unwrap_or("");
    let code = parts.next().unwrap_or("");
    if !proto.starts_with("HTTP/1.") || code != "101" {
        return Err("WS 升级失败 (非 101)");
    }

    // Sec-WebSocket-Accept = base64(SHA1(key || GUID)), 防中间人/错误端点
    let expected = xiaozhi_core::http::websocket_accept(key_b64.as_str());
    match xiaozhi_core::http::header_value(head, "Sec-WebSocket-Accept") {
        Some(v) if v == expected.as_str() => {},
        _ => return Err("WS Accept 校验失败"),
    }
    Ok(())
}

// JSON 字段扫描已下沉到 core (host 可测); 这里保持原路径兼容调用方
pub use xiaozhi_core::json_scan::{num_field as json_num_field, str_field as json_str_field};
