//! MQTT + UDP 会话任务 (xiaozhi MQTT 网关)
//!
//! 与 [`crate::app::session_task`] 二选一: main 检测到 NVS 里有 MQTT 网关配置
//! (由 OTA 清单下发) 时启动本任务。分工:
//!
//! - **MQTT (TCP)**: 控制通道 — hello / listen start/stop / tts abort / MCP,
//!   以及服务器 hello (含 UDP 密钥) 与 tts start/stop 事件;
//! - **UDP**: 音频通道 — 每包 = 16B 头 + AES-128-CTR 密文, 头里的 timestamp
//!   供服务端 AEC 对齐 (回声消除)。
//!
//! 通道契约与 WS 会话完全一致 (`Channels`), 音频主循环无需区分传输方式。

use embassy_net::{
    Ipv4Address, Stack,
    tcp::TcpSocket,
    udp::{PacketMetadata, UdpSocket},
};
use embassy_time::{Duration, with_timeout};

use defmt::{error, info, warn};

use xiaozhi_core::mqtt::{self, ConnectParams, Packet};
use xiaozhi_core::udp as udp_proto;

use crate::app::{
    Channels, DROP_DOWNLINK, DownlinkPacket, OpusPacket, SessionCommand, SessionEvent, VOLUME_PCT,
    mcp_handle,
};
use crate::session::{LISTEN_START_AUTO, LISTEN_START_MANUAL, LISTEN_STOP, TTS_ABORT};
use crate::wifi::store::MqttSettings;

/// MQTT keepalive (秒); 网关按此判断心跳超时
const KEEPALIVE_SECS: u16 = 240;
/// 断线重连间隔
const RECONNECT_MS: u64 = 3_000;
/// MQTT 读缓冲 (MCP 工具列表 ~1.2KB, 服务器工具列表可达数 KB)
const MQTT_BUF: usize = 4096;
/// UDP 收包缓冲 (单包上限 1276B + 16B 头)
const UDP_BUF: usize = 2048;

/// 会话任务依赖
pub struct MqttSessionArgs {
    pub stack: Stack<'static>,
    pub config: MqttSettings,
    pub channels: &'static Channels,
}

/// 从 PSRAM 分配字节缓冲 (失败回退内部堆)
///
/// 内部堆只有 128KB: MQTT/WS 两个会话任务不会同时存在, 但 20KB 网络缓冲
/// 留在内部会把水位推到 95%+ (真机实测), 触发分配失败/panic。
fn psram_bytes(len: usize) -> &'static mut [u8] {
    use core::alloc::Layout;
    let layout = Layout::from_size_align(len, 4).expect("bad layout");
    // SAFETY: 非零布局; 分配块常驻 (泄漏至重启)
    let p =
        unsafe { esp_alloc::HEAP.alloc_caps(esp_alloc::MemoryCapability::External.into(), layout) };
    if p.is_null() {
        alloc::vec![0u8; len].leak()
    } else {
        // SAFETY: alloc_caps 返回 len 字节可写内存; 清零避免脏数据
        unsafe {
            core::ptr::write_bytes(p, 0, len);
            core::slice::from_raw_parts_mut(p, len)
        }
    }
}

fn now_ms() -> u64 {
    esp_hal::time::Instant::now()
        .duration_since_epoch()
        .as_millis()
}

/// 解析 `host:port` (OTA 下发的 MQTT endpoint)
fn parse_endpoint(s: &str) -> Option<(Ipv4Address, u16)> {
    let (host, port) = s.rsplit_once(':')?;
    Some((host.parse().ok()?, port.parse().ok()?))
}

/// 单次网络读/写等待上限: 半开连接时让会话任务能退出重连
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// TCP 精确读取 N 字节
async fn read_exact(socket: &mut TcpSocket<'_>, buf: &mut [u8]) -> Result<(), &'static str> {
    let mut done = 0;
    while done < buf.len() {
        let n = with_timeout(IO_TIMEOUT, socket.read(&mut buf[done..]))
            .await
            .map_err(|_| "读取超时")?
            .map_err(|_| "读取失败")?;
        if n == 0 {
            return Err("连接关闭");
        }
        done += n;
    }
    Ok(())
}

/// 循环写入全部字节
async fn write_all(socket: &mut TcpSocket<'_>, mut data: &[u8]) -> Result<(), &'static str> {
    while !data.is_empty() {
        let n = with_timeout(
            IO_TIMEOUT,
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

/// 读一个完整 MQTT 报文到 `buf`, 返回字节数
///
/// 剩余长度超过缓冲时把超长报文整体读掉丢弃 (保持流同步), 返回错误由调用方
/// 决定是否断开。`Ok(None)` 表示报文超长已被丢弃。
async fn read_packet(
    socket: &mut TcpSocket<'_>,
    buf: &mut [u8],
) -> Result<Option<usize>, &'static str> {
    let mut hdr = [0u8; 5];
    read_exact(socket, &mut hdr[..1]).await?;
    let mut remaining = 0usize;
    let mut mult = 1usize;
    let mut n = 1usize;
    loop {
        if n >= hdr.len() {
            return Err("剩余长度非法");
        }
        read_exact(socket, &mut hdr[n..n + 1]).await?;
        let b = hdr[n];
        n += 1;
        remaining += (b & 0x7F) as usize * mult;
        mult *= 128;
        if b & 0x80 == 0 {
            break;
        }
    }
    if n + remaining > buf.len() {
        // 超长报文: 丢弃剩余字节
        let mut left = remaining;
        let mut scratch = [0u8; 256];
        while left > 0 {
            let take = left.min(scratch.len());
            read_exact(socket, &mut scratch[..take]).await?;
            left -= take;
        }
        warn!("MQTT 报文过长 ({}B), 已丢弃", remaining);
        return Ok(None);
    }
    buf[..n].copy_from_slice(&hdr[..n]);
    read_exact(socket, &mut buf[n..n + remaining]).await?;
    Ok(Some(n + remaining))
}

/// 发送一帧 MQTT PUBLISH (QoS0)
async fn publish(
    socket: &mut TcpSocket<'_>,
    out: &mut [u8],
    topic: &str,
    payload: &[u8],
) -> Result<(), &'static str> {
    let n = mqtt::encode_publish(topic, payload, out).map_err(|_| "MQTT 组包失败")?;
    write_all(socket, &out[..n]).await
}

/// 建立 MQTT 连接并订阅回复主题
///
/// 握手期间收到 PUBLISH (网关的 MCP initialize) 时即时处理, 避免网关侧
/// 工具初始化超时。
async fn connect_mqtt(
    socket: &mut TcpSocket<'static>,
    cfg: &MqttSettings,
    host: Ipv4Address,
    port: u16,
    buf: &mut [u8],
    out: &mut [u8],
    channels: &Channels,
) -> Result<(), &'static str> {
    socket.abort();
    socket
        .connect((host, port))
        .await
        .map_err(|_| "MQTT TCP 连接失败")?;

    let params = ConnectParams {
        client_id: cfg.client_id.as_str(),
        username: cfg.username.as_str(),
        password: cfg.password.as_str(),
        keep_alive_secs: KEEPALIVE_SECS,
    };
    let n = mqtt::encode_connect(&params, out).map_err(|_| "CONNECT 组包失败")?;
    write_all(socket, &out[..n]).await?;

    // 等 CONNACK
    loop {
        let Some(n) = read_packet(socket, buf).await? else {
            continue;
        };
        match mqtt::decode(&buf[..n]).map_err(|_| "MQTT 解析失败")? {
            Some((Packet::ConnAck { code: 0, .. }, _)) => break,
            Some((Packet::ConnAck { code, .. }, _)) => {
                error!("MQTT CONNACK 拒绝: code={}", code);
                return Err("MQTT 认证失败");
            },
            _ => continue,
        }
    }

    // 订阅回复主题 (设备专属)
    let n = mqtt::encode_subscribe(1, cfg.subscribe_topic.as_str(), out)
        .map_err(|_| "SUBSCRIBE 组包失败")?;
    write_all(socket, &out[..n]).await?;
    loop {
        let Some(n) = read_packet(socket, buf).await? else {
            continue;
        };
        match mqtt::decode(&buf[..n]).map_err(|_| "MQTT 解析失败")? {
            Some((Packet::SubAck { granted, .. }, _)) => {
                if granted > 2 {
                    return Err("SUBACK 拒绝订阅");
                }
                break;
            },
            Some((Packet::Publish(p), _)) => {
                let text = core::str::from_utf8(p.payload).unwrap_or("");
                if crate::session::json_str_field(text, "type") == Some("mcp") {
                    let volume = VOLUME_PCT.load(core::sync::atomic::Ordering::Relaxed);
                    if let Some(resp) = mcp_handle(text, volume, channels) {
                        let _ =
                            publish(socket, out, cfg.publish_topic.as_str(), resp.as_bytes()).await;
                    }
                }
            },
            _ => continue,
        }
    }
    info!(
        "MQTT 已连接: {} (订阅 {})",
        cfg.endpoint.as_str(),
        cfg.subscribe_topic.as_str()
    );
    Ok(())
}

/// MQTT + UDP 会话任务
#[embassy_executor::task]
pub async fn mqtt_session_task(args: MqttSessionArgs) {
    let MqttSessionArgs {
        stack,
        config,
        channels,
    } = args;

    let (host, port) = match parse_endpoint(config.endpoint.as_str()) {
        Some(v) => v,
        None => {
            error!("MQTT endpoint 无效: {}", config.endpoint.as_str());
            // 配置不可用时保持任务存活, 避免 main 侧通道无人消费
            loop {
                embassy_time::Timer::after(Duration::from_secs(3600)).await;
            }
        },
    };

    // 大缓冲走 PSRAM (泄漏至重启), 元数据留内部堆: 20KB 留在内部会把
    // 堆水位推到 95%+ (真机实测), TCP/音频分配随之失败
    let rx = psram_bytes(4096);
    let tx = psram_bytes(4096);
    let mut socket = TcpSocket::new(stack, rx, tx);
    let read_buf = psram_bytes(MQTT_BUF);
    let out_buf = psram_bytes(2048);
    let udp_buf = psram_bytes(UDP_BUF);
    let udp_rx_meta: &'static mut [PacketMetadata] = alloc::vec![PacketMetadata::EMPTY; 4].leak();
    let udp_tx_meta: &'static mut [PacketMetadata] = alloc::vec![PacketMetadata::EMPTY; 4].leak();
    let udp_rx = psram_bytes(UDP_BUF);
    let udp_tx = psram_bytes(2048);
    let mut udp = UdpSocket::new(stack, udp_rx_meta, udp_rx, udp_tx_meta, udp_tx);
    if udp.bind(0).is_err() {
        error!("UDP 本地端口绑定失败");
    }

    let mut connected = false;
    let mut want_connect = false;
    let mut udp_ready = false;
    let mut udp_addr: Option<(Ipv4Address, u16)> = None;
    let mut aes_key: Option<xiaozhi_core::aes::Aes128> = None;
    let mut udp_nonce = [0u8; udp_proto::UDP_HEADER_LEN];
    let mut udp_seq: u32 = 0;
    let mut last_ping = now_ms();
    let mut last_try = 0u64;
    // 当前会话 id: 网关关闭旧 bridge 时会补发一条带**旧 session_id** 的
    // goodbye, 设备必须按 id 过滤, 否则新会话刚建立就被判为关闭。
    let mut session_id: heapless::String<48> = heapless::String::new();
    // 下行统计 (tts stop 时打印)
    let mut down_pkts: u32 = 0;
    let mut down_bytes: u64 = 0;

    loop {
        // ---- WiFi 掉线: 关闭连接, 等链路恢复后重连 ----
        if connected && !stack.is_link_up() {
            warn!("WiFi 链路断开 — 关闭 MQTT 会话");
            socket.abort();
            connected = false;
            udp_ready = false;
            channels.event.send(SessionEvent::Closed).await;
        }

        // ---- 命令 ----
        loop {
            match channels.command.try_receive() {
                Ok(SessionCommand::Connect) => {
                    if connected {
                        let hello = xiaozhi_core::protocol::udp_hello_message();
                        if publish(
                            &mut socket,
                            out_buf,
                            config.publish_topic.as_str(),
                            hello.as_bytes(),
                        )
                        .await
                        .is_ok()
                        {
                            info!("会话: 已发送 hello (等待 UDP 配置)");
                        }
                    } else {
                        want_connect = true;
                    }
                },
                Ok(SessionCommand::SendListenStart(mode)) => {
                    if connected {
                        let msg = if mode == xiaozhi_core::dialog::ListenMode::Auto {
                            LISTEN_START_AUTO
                        } else {
                            LISTEN_START_MANUAL
                        };
                        let _ = publish(
                            &mut socket,
                            out_buf,
                            config.publish_topic.as_str(),
                            msg.as_bytes(),
                        )
                        .await;
                    }
                },
                Ok(SessionCommand::SendListenStop) => {
                    if connected {
                        let _ = publish(
                            &mut socket,
                            out_buf,
                            config.publish_topic.as_str(),
                            LISTEN_STOP.as_bytes(),
                        )
                        .await;
                    }
                },
                Ok(SessionCommand::SendTtsAbort) => {
                    if connected {
                        let _ = publish(
                            &mut socket,
                            out_buf,
                            config.publish_topic.as_str(),
                            TTS_ABORT.as_bytes(),
                        )
                        .await;
                    }
                },
                Ok(SessionCommand::CloseSession) => {
                    if connected {
                        let _ = publish(
                            &mut socket,
                            out_buf,
                            config.publish_topic.as_str(),
                            crate::session::GOODBYE.as_bytes(),
                        )
                        .await;
                    }
                },
                Ok(SessionCommand::SetVolume(_)) => { /* 音频主循环执行 */ },
                Err(_) => break,
            }
        }

        // ---- 未连接: 定时重连 ----
        if !connected {
            let now = now_ms();
            if now.saturating_sub(last_try) >= RECONNECT_MS {
                last_try = now;
                match with_timeout(
                    Duration::from_secs(15),
                    connect_mqtt(
                        &mut socket,
                        &config,
                        host,
                        port,
                        read_buf,
                        out_buf,
                        channels,
                    ),
                )
                .await
                {
                    Ok(Ok(())) => {
                        connected = true;
                        last_ping = now_ms();
                        if want_connect {
                            want_connect = false;
                            let hello = xiaozhi_core::protocol::udp_hello_message();
                            let _ = publish(
                                &mut socket,
                                out_buf,
                                config.publish_topic.as_str(),
                                hello.as_bytes(),
                            )
                            .await;
                        }
                    },
                    Ok(Err(e)) => info!("MQTT 连接失败: {} ({}ms 后重试)", e, RECONNECT_MS),
                    Err(_) => {
                        socket.abort();
                        info!("MQTT 握手超时 (15s), {}ms 后重试", RECONNECT_MS)
                    },
                }
            }
        }

        if !connected {
            embassy_time::Timer::after(Duration::from_millis(100)).await;
            continue;
        }

        // ---- TCP 半开/对端关闭检测 ----
        // can_recv() 在缓冲为空时恒为 false (连接已关闭也返回 false),
        // 只看 can_recv 会永远发现不了对端断开; 用连接状态兜底。
        if socket.state() != embassy_net::tcp::State::Established {
            warn!("MQTT 链路已断开 (TCP 状态异常), 准备重连");
            socket.abort();
            connected = false;
            udp_ready = false;
            channels.event.send(SessionEvent::Closed).await;
            embassy_time::Timer::after(Duration::from_millis(50)).await;
            continue;
        }

        // ---- MQTT 收包 (每轮最多 4 个) ----
        for _ in 0..4 {
            if !socket.can_recv() {
                break;
            }
            let n = match read_packet(&mut socket, read_buf).await {
                Ok(Some(n)) => n,
                Ok(None) => continue, // 超长报文已丢弃
                Err(e) => {
                    warn!("MQTT 读取错误: {} — 会话关闭", e);
                    connected = false;
                    udp_ready = false;
                    channels.event.send(SessionEvent::Closed).await;
                    break;
                },
            };
            let Ok(Some((packet, _))) = mqtt::decode(&read_buf[..n]) else {
                continue;
            };
            match packet {
                Packet::Publish(p) => {
                    let text = core::str::from_utf8(p.payload).unwrap_or("");
                    let mtype = crate::session::json_str_field(text, "type");
                    let state = crate::session::json_str_field(text, "state");
                    match (mtype, state) {
                        (Some("hello"), _) => {
                            session_id.clear();
                            let _ = session_id.push_str(
                                crate::session::json_str_field(text, "session_id").unwrap_or(""),
                            );
                            if apply_server_hello(
                                text,
                                &mut aes_key,
                                &mut udp_nonce,
                                &mut udp_addr,
                                &mut udp_seq,
                            ) {
                                udp_ready = true;
                                channels.event.send(SessionEvent::Ready).await;
                            }
                        },
                        (Some("tts"), Some("start")) => {
                            DROP_DOWNLINK.store(false, core::sync::atomic::Ordering::Relaxed);
                            down_pkts = 0;
                            down_bytes = 0;
                            info!("服务器: tts start");
                            channels.event.send(SessionEvent::TtsStart).await;
                        },
                        (Some("tts"), Some("stop")) => {
                            info!("服务器: tts stop (下行 {} 包 {}B)", down_pkts, down_bytes);
                            channels.event.send(SessionEvent::TtsStop).await;
                        },
                        (Some("goodbye"), _) => {
                            let sid =
                                crate::session::json_str_field(text, "session_id").unwrap_or("");
                            if !session_id.is_empty() && sid != session_id.as_str() {
                                // 旧 bridge 关闭产生的过期通知: 忽略
                                info!(
                                    "忽略过期 goodbye (会话 {}), 当前 {}",
                                    sid,
                                    session_id.as_str()
                                );
                            } else {
                                info!("服务器: goodbye");
                                udp_ready = false;
                                channels.event.send(SessionEvent::Closed).await;
                            }
                        },
                        (Some("error"), _) => {
                            warn!("服务器错误: {}", text);
                            channels.event.send(SessionEvent::Closed).await;
                        },
                        (Some("mcp"), _) => {
                            let volume = VOLUME_PCT.load(core::sync::atomic::Ordering::Relaxed);
                            if let Some(resp) = mcp_handle(text, volume, channels) {
                                info!("MCP 应答: {}", resp.as_str());
                                if publish(
                                    &mut socket,
                                    out_buf,
                                    config.publish_topic.as_str(),
                                    resp.as_bytes(),
                                )
                                .await
                                .is_err()
                                {
                                    connected = false;
                                    channels.event.send(SessionEvent::Closed).await;
                                }
                            }
                        },
                        _ => {
                            crate::diag!("服务器: {}", text);
                        },
                    }
                },
                Packet::PingResp => {},
                Packet::Other { kind } => {
                    crate::diag!("MQTT 报文 type={}", kind);
                    let _ = kind;
                },
                Packet::ConnAck { .. } | Packet::SubAck { .. } | Packet::PubAck { .. } => {},
            }
        }

        // ---- 上行: 编码帧 → UDP (AES-128-CTR) ----
        while let Ok(pkt) = channels.uplink.try_receive() {
            if !udp_ready {
                continue;
            }
            let (Some(aes), Some(addr)) = (aes_key.as_ref(), udp_addr) else {
                continue;
            };
            udp_seq = udp_seq.wrapping_add(1);
            let Some(header) =
                udp_proto::build_header(&udp_nonce, pkt.data.len(), pkt.timestamp, udp_seq)
            else {
                continue;
            };
            let total = udp_proto::UDP_HEADER_LEN + pkt.data.len();
            let frame = &mut out_buf[..total];
            frame[..udp_proto::UDP_HEADER_LEN].copy_from_slice(&header);
            frame[udp_proto::UDP_HEADER_LEN..].copy_from_slice(&pkt.data);
            aes.ctr(&header, &mut frame[udp_proto::UDP_HEADER_LEN..]);
            if let Err(e) = udp.send_to(frame, addr).await {
                warn!("UDP 上行失败: {:?}", defmt::Debug2Format(&e));
            }
        }

        // ---- 下行: UDP → 解密 → 通道 ----
        if udp_ready {
            let Some(aes) = aes_key.as_ref() else {
                continue;
            };
            for _ in 0..4 {
                let recv = with_timeout(Duration::from_millis(1), udp.recv_from(udp_buf)).await;
                let Ok(Ok((n, _meta))) = recv else {
                    break;
                };
                let Some(hdr) = udp_proto::parse_header(&udp_buf[..n]) else {
                    continue;
                };
                if hdr.msg_type != 1 || hdr.payload_len as usize != n - udp_proto::UDP_HEADER_LEN {
                    continue;
                }
                if DROP_DOWNLINK.load(core::sync::atomic::Ordering::Relaxed) {
                    continue;
                }
                let mut header = [0u8; udp_proto::UDP_HEADER_LEN];
                header.copy_from_slice(&udp_buf[..udp_proto::UDP_HEADER_LEN]);
                aes.ctr(&header, &mut udp_buf[udp_proto::UDP_HEADER_LEN..n]);
                let mut data: OpusPacket = heapless::Vec::new();
                if data
                    .extend_from_slice(&udp_buf[udp_proto::UDP_HEADER_LEN..n])
                    .is_ok()
                {
                    down_pkts += 1;
                    down_bytes += data.len() as u64;
                    channels
                        .downlink
                        .send(DownlinkPacket {
                            timestamp: hdr.timestamp,
                            data,
                        })
                        .await;
                }
            }
        }

        // ---- 心跳 ----
        if now_ms().saturating_sub(last_ping) >= KEEPALIVE_SECS as u64 * 500 {
            last_ping = now_ms();
            if write_all(&mut socket, &mqtt::PINGREQ_FRAME).await.is_err() {
                connected = false;
                udp_ready = false;
                channels.event.send(SessionEvent::Closed).await;
            }
        }

        embassy_time::Timer::after(Duration::from_millis(5)).await;
    }
}

/// 解析服务器 hello 中的 UDP 参数
fn apply_server_hello(
    text: &str,
    aes_key: &mut Option<xiaozhi_core::aes::Aes128>,
    nonce: &mut [u8; udp_proto::UDP_HEADER_LEN],
    addr: &mut Option<(Ipv4Address, u16)>,
    seq: &mut u32,
) -> bool {
    let parse_str = |k: &str| crate::session::json_str_field(text, k);
    let (Some(server), Some(port), Some(key_hex), Some(nonce_hex)) = (
        parse_str("server"),
        crate::session::json_num_field(text, "port"),
        parse_str("key"),
        parse_str("nonce"),
    ) else {
        warn!("服务器 hello 缺少 udp 字段: {}", text);
        return false;
    };
    let Ok(server_ip) = server.parse::<Ipv4Address>() else {
        warn!("UDP 服务器地址无效: {}", server);
        return false;
    };
    let Some(key) = udp_proto::decode_hex::<16>(key_hex) else {
        warn!("UDP 密钥非法");
        return false;
    };
    let Some(n) = udp_proto::decode_hex::<{ udp_proto::UDP_HEADER_LEN }>(nonce_hex) else {
        warn!("UDP nonce 非法");
        return false;
    };
    *aes_key = Some(xiaozhi_core::aes::Aes128::new(&key));
    *nonce = n;
    *addr = Some((server_ip, port as u16));
    *seq = 0;
    info!("UDP 通道就绪: {}:{}", server, port);
    true
}
