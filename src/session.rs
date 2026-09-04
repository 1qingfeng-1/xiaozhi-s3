//! WebSocket 会话传输层 (xiaozhi 协议, version 1: 二进制帧 = 裸 opus 载荷)
//!
//! 只做传输: 帧编解码 / 握手 / hello。会话生命周期与状态机见 [`crate::app`]。

use embassy_net::{Ipv4Address, tcp::TcpSocket};
use esp_hal::rng::Rng;

use alloc::format;

use crate::audio::opus::MAX_PACKET_BYTES;

/// WebSocket 服务器端口与路径 (xiaozhi 协议)
pub const WS_PORT: u16 = 8000;
pub const WS_PATH: &str = "/xiaozhi/v1/";

/// WS 帧载荷上限 (服务器 TTS 单帧可达 opus 最大包 1276B)
const RECV_BUF: usize = 2048;

/// listen/tts 消息模板 (xiaozhi 协议; mode 语义对齐 C++ protocol.cc)
pub const LISTEN_START_MANUAL: &str =
    "{\"type\":\"listen\",\"state\":\"start\",\"mode\":\"manual\"}";
pub const LISTEN_START_AUTO: &str = "{\"type\":\"listen\",\"state\":\"start\",\"mode\":\"auto\"}";
pub const LISTEN_STOP: &str = "{\"type\":\"listen\",\"state\":\"stop\"}";
pub const TTS_ABORT: &str = "{\"type\":\"tts\",\"state\":\"abort\"}";

/// hello 消息 (version 1; audio_params 与上行编码器一致)
pub const HELLO: &str = "{\"type\":\"hello\",\"version\":1,\"features\":{},\
  \"transport\":\"websocket\",\"audio_params\":{\"format\":\"opus\",\
  \"sample_rate\":16000,\"channels\":1,\"frame_duration\":60}}";

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
        let mut frame: heapless::Vec<u8, 2048> = heapless::Vec::new();
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
        write_all(self.socket, &frame).await
    }

    /// 接收一帧 (服务端帧不掩码), 返回 (opcode, payload)
    pub async fn recv_frame(&mut self) -> Result<(u8, heapless::Vec<u8, RECV_BUF>), &'static str> {
        let mut hdr = [0u8; 2];
        read_exact(self.socket, &mut hdr)
            .await
            .map_err(|_| "读取帧头失败")?;
        let opcode = hdr[0] & 0x0F;
        let masked = hdr[1] & 0x80 != 0;
        let len0 = (hdr[1] & 0x7F) as usize;
        let len = match len0 {
            126 => {
                let mut ext = [0u8; 2];
                read_exact(self.socket, &mut ext)
                    .await
                    .map_err(|_| "读取长度失败")?;
                u16::from_be_bytes(ext) as usize
            },
            127 => return Err("64 位长度不支持"),
            n => n,
        };
        if len > RECV_BUF {
            return Err("帧过长");
        }
        let mut mask = [0u8; 4];
        if masked {
            read_exact(self.socket, &mut mask)
                .await
                .map_err(|_| "读取掩码失败")?;
        }
        let mut payload = heapless::Vec::<u8, RECV_BUF>::new();
        payload.resize(len, 0).map_err(|_| "帧过长")?;
        if len > 0 {
            read_exact(self.socket, &mut payload)
                .await
                .map_err(|_| "读取负载失败")?;
            if masked {
                unmask_in_place(&mut payload, &mask);
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
async fn write_all(socket: &mut TcpSocket<'_>, mut data: &[u8]) -> Result<(), &'static str> {
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

/// 就地 XOR 掩码 (WebSocket 客户端帧解码)
fn unmask_in_place(payload: &mut [u8], mask: &[u8; 4]) {
    for (i, b) in payload.iter_mut().enumerate() {
        *b ^= mask[i % 4];
    }
}

/// 在既有 socket 上建立会话: TCP 连接 → WS 握手 → 发送 hello → 等待服务器 hello
///
/// socket 由调用方持有并跨重连复用 (每次 connect 都 leak 缓冲会耗尽内部堆)。
/// 返回服务器音频参数: 采样率 / 帧长 ms。
pub async fn connect(
    socket: &mut TcpSocket<'static>,
    server: Ipv4Address,
    mac: &str,
) -> Result<(u32, u32), &'static str> {
    socket.abort();
    socket
        .connect((server, WS_PORT))
        .await
        .map_err(|_| "WS TCP 连接失败")?;

    handshake(socket, mac, server).await?;

    let mut ws = WsClient { socket };
    ws.send_text(HELLO.as_bytes()).await?;
    let (opcode, payload) = ws.recv_frame().await.map_err(|_| "服务器 hello 接收失败")?;
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
    server: Ipv4Address,
) -> Result<(), &'static str> {
    // Sec-WebSocket-Key: 16 随机字节的 base64
    let mut key_bytes = [0u8; 16];
    let rng = Rng::new();
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
             Host: {}:{}\r\n\
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
            WS_PORT,
            key_b64.as_str(),
            mac,
            mac
        ),
    );

    write_all(socket, req.as_bytes())
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
        return Err("WS 升级失败");
    }
    Ok(())
}

/// 从 JSON 文本提取 `"key":<数字>` 字段值 (服务器 hello 的 audio_params 用)
pub fn json_num_field(json: &str, key: &str) -> Option<u32> {
    let pat = format!("\"{key}\":");
    let mut rest = json;
    while let Some(i) = rest.find(&pat) {
        let after = rest[i + pat.len()..].trim_start();
        let digits = after.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 {
            return after[..digits].parse().ok();
        }
        rest = &rest[i + pat.len()..];
    }
    None
}

/// 从 JSON 文本提取 `"key":"字符串"` 字段值 (服务器消息的 type/state 用)
pub fn json_str_field<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\":");
    let mut rest = json;
    while let Some(i) = rest.find(&pat) {
        let after = rest[i + pat.len()..].trim_start();
        if let Some(v) = after
            .strip_prefix('"')
            .and_then(|v| v.split_once('"').map(|(s, _)| s))
        {
            return Some(v);
        }
        rest = &rest[i + pat.len()..];
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_num_field_finds_audio_params() {
        let hello = r#"{"type": "hello", "audio_params": {"format": "opus", "sample_rate": 16000, "frame_duration": 60}}"#;
        assert_eq!(json_num_field(hello, "sample_rate"), Some(16000));
        assert_eq!(json_num_field(hello, "frame_duration"), Some(60));
        assert_eq!(json_num_field(hello, "missing"), None);
    }

    #[test]
    fn json_num_field_takes_first_match() {
        let s = r#"{"a":1,"b":{"a":2}}"#;
        assert_eq!(json_num_field(s, "a"), Some(1));
    }

    #[test]
    fn json_str_field_extracts_value() {
        let msg = r#"{"type":"tts","state":"start","session_id":"abc"}"#;
        assert_eq!(json_str_field(msg, "type"), Some("tts"));
        assert_eq!(json_str_field(msg, "state"), Some("start"));
        assert_eq!(json_str_field(msg, "session_id"), Some("abc"));
        assert_eq!(json_str_field(msg, "nope"), None);
    }
}
