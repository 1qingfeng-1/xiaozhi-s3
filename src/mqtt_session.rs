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
use xiaozhi_core::server_msg::{self, ServerMessage};
use xiaozhi_core::udp as udp_proto;

use crate::app::{
    Channels, DROP_DOWNLINK, DownlinkPacket, OpusPacket, SessionCommand, SessionEvent, UiMessage,
    VOLUME_PCT, mcp_handle, ui_send, ui_text,
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

use crate::mem::psram_bytes;

fn now_ms() -> u64 {
    esp_hal::time::Instant::now()
        .duration_since_epoch()
        .as_millis()
}

/// 解析 `host:port` (OTA 下发的 MQTT endpoint)
pub fn parse_endpoint(s: &str) -> Option<(Ipv4Address, u16)> {
    let (host, port) = xiaozhi_core::http::parse_host_port(s)?;
    Some((host.parse().ok()?, port))
}

/// 单次网络读/写等待上限: 半开连接时让会话任务能退出重连
const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// 会话主循环的单帧读上限: 慢帧不能把上行通道 (≈1.4s) 拖爆
const FRAME_TIMEOUT: Duration = Duration::from_secs(2);

/// TCP 精确读取 N 字节 (自定义单次读超时)
async fn read_exact_timeout(
    socket: &mut TcpSocket<'_>,
    buf: &mut [u8],
    timeout: Duration,
) -> Result<(), &'static str> {
    let mut done = 0;
    while done < buf.len() {
        let n = with_timeout(timeout, socket.read(&mut buf[done..]))
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
    read_packet_timeout(socket, buf, IO_TIMEOUT).await
}

/// 读一个完整 MQTT 报文, 单次读超时可配置 (主循环用 FRAME_TIMEOUT)
async fn read_packet_timeout(
    socket: &mut TcpSocket<'_>,
    buf: &mut [u8],
    timeout: Duration,
) -> Result<Option<usize>, &'static str> {
    let mut hdr = [0u8; 5];
    read_exact_timeout(socket, &mut hdr[..1], timeout).await?;
    let mut remaining = 0usize;
    let mut mult = 1usize;
    let mut n = 1usize;
    loop {
        if n >= hdr.len() {
            return Err("剩余长度非法");
        }
        read_exact_timeout(socket, &mut hdr[n..n + 1], timeout).await?;
        let b = hdr[n];
        n += 1;
        remaining += (b & 0x7F) as usize * mult;
        mult *= 128;
        if b & 0x80 == 0 {
            break;
        }
    }
    if n + remaining > buf.len() {
        // 超长报文: 设硬上限并限时丢弃。剩余长度字段最大可声明 268MB,
        // 逐 256B 同步丢弃会被慢速对端拖住数分钟 (看门狗也监控不到)。
        if remaining > 64 * 1024 {
            return Err("MQTT 报文过大");
        }
        let drained = with_timeout(Duration::from_secs(10), async {
            let mut left = remaining;
            let mut scratch = [0u8; 256];
            while left > 0 {
                let take = left.min(scratch.len());
                read_exact_timeout(socket, &mut scratch[..take], timeout).await?;
                left -= take;
            }
            Ok::<(), &'static str>(())
        })
        .await;
        if !matches!(drained, Ok(Ok(()))) {
            return Err("丢弃超长报文超时");
        }
        warn!("MQTT 报文过长 ({}B), 已丢弃", remaining);
        return Ok(None);
    }
    buf[..n].copy_from_slice(&hdr[..n]);
    read_exact_timeout(socket, &mut buf[n..n + remaining], timeout).await?;
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
                // QoS1 必须回 PUBACK, 否则 broker 会反复重发
                // (MCP 的 set_volume/reboot 会被重复执行)
                if let Some(id) = p.packet_id {
                    if let Ok(n) = mqtt::encode_puback(id, out) {
                        let _ = write_all(socket, &out[..n]).await;
                    }
                }
                if p.topic != cfg.subscribe_topic.as_str() {
                    continue;
                }
                let text = core::str::from_utf8(p.payload).unwrap_or("");
                if crate::session::json_str_field(text, "type") == Some("mcp") {
                    let volume = VOLUME_PCT.load(core::sync::atomic::Ordering::Relaxed);
                    if let Some(resp) = mcp_handle(text, volume) {
                        if publish(socket, out, cfg.publish_topic.as_str(), resp.as_bytes())
                            .await
                            .is_err()
                        {
                            crate::app::MCP_REBOOT
                                .store(false, core::sync::atomic::Ordering::Relaxed);
                        } else {
                            crate::app::mcp_maybe_reboot().await;
                        }
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
    // UDP 收缓冲要能吸收 TTS 突发: 服务器/网关按 WS 到达节奏转发, 短缓冲
    // (2KB≈2 包) 会静默丢包 → opus 解码断点 → 长句卡顿/杂音 (真机复现)
    let udp_rx_meta: &'static mut [PacketMetadata] = alloc::vec![PacketMetadata::EMPTY; 128].leak();
    let udp_tx_meta: &'static mut [PacketMetadata] = alloc::vec![PacketMetadata::EMPTY; 8].leak();
    // 64KB ≈ 50 包: TTS 合成快于实时, 服务器会成串下发, 缓冲不足会静默丢包
    let udp_rx = psram_bytes(64 * 1024);
    let udp_tx = psram_bytes(4096);
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
    // 等 PINGRESP 的起点; Some 表示已发 PINGREQ 未收到响应
    let mut awaiting_pong: Option<u64> = None;
    // 指数退避重连间隔 (3s → 6s → … → 30s)
    let mut retry_ms: u64 = RECONNECT_MS;
    // 当前会话 id: 网关关闭旧 bridge 时会补发一条带**旧 session_id** 的
    // goodbye, 设备必须按 id 过滤, 否则新会话刚建立就被判为关闭。
    let mut session_id: heapless::String<48> = heapless::String::new();
    // 已发出 hello、尚未收到回复: 期间到达的 goodbye 一定是旧 bridge 的
    // (网关先关旧 bridge 再建新会话), 必须忽略。
    let mut awaiting_hello = false;
    // 下行统计 (tts stop 时打印)
    let mut down_pkts: u32 = 0;
    let mut down_bytes: u64 = 0;
    // 下行序列连续性: 丢包会让 opus 解码出现断点 (长句卡顿/杂音)
    let mut have_seq = false;
    let mut max_seq: u32 = 0;
    let mut seq_gaps: u32 = 0;
    // 迟到/重复包: 序号不新于已见最大值的包
    let mut late_pkts: u32 = 0;
    // 来源不匹配的 UDP 包 (任意主机注入)
    let mut bad_src: u32 = 0;

    loop {
        crate::app::SESSION_HEARTBEAT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        // ---- WiFi 掉线: 关闭连接, 等链路恢复后重连 ----
        if connected && !stack.is_link_up() {
            warn!("WiFi 链路断开 — 关闭 MQTT 会话");
            socket.abort();
            connected = false;
            udp_ready = false;
            awaiting_hello = false;
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
                            awaiting_hello = true;
                            session_id.clear();
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
                Err(_) => break,
            }
        }

        // ---- 未连接: 指数退避重连 ----
        if !connected {
            let now = now_ms();
            if now.saturating_sub(last_try) >= retry_ms {
                last_try = now;
                let delay = retry_ms;
                match with_timeout(
                    Duration::from_secs(15),
                    connect_mqtt(&mut socket, &config, host, port, read_buf, out_buf),
                )
                .await
                {
                    Ok(Ok(())) => {
                        connected = true;
                        retry_ms = RECONNECT_MS;
                        awaiting_pong = None;
                        last_ping = now_ms();
                        if want_connect {
                            want_connect = false;
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
                                awaiting_hello = true;
                                session_id.clear();
                            }
                        }
                    },
                    Ok(Err(e)) => {
                        info!("MQTT 连接失败: {} ({}ms 后重试)", e, delay);
                        retry_ms = (retry_ms * 2).min(30_000);
                    },
                    Err(_) => {
                        socket.abort();
                        info!("MQTT 握手超时 (15s), {}ms 后重试", delay);
                        retry_ms = (retry_ms * 2).min(30_000);
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
            awaiting_hello = false;
            channels.event.send(SessionEvent::Closed).await;
            embassy_time::Timer::after(Duration::from_millis(50)).await;
            continue;
        }

        // ---- MQTT 收包 (每轮最多 4 个) ----
        for _ in 0..4 {
            if !socket.can_recv() {
                break;
            }
            let n = match read_packet_timeout(&mut socket, read_buf, FRAME_TIMEOUT).await {
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
                    // QoS1: 立即回 PUBACK, 防止 broker 重发导致 MCP 重复执行
                    if let Some(id) = p.packet_id {
                        if let Ok(n) = mqtt::encode_puback(id, out_buf) {
                            if write_all(&mut socket, &out_buf[..n]).await.is_err() {
                                connected = false;
                                channels.event.send(SessionEvent::Closed).await;
                                break;
                            }
                        }
                    }
                    if p.topic != config.subscribe_topic.as_str() {
                        crate::diag!("MQTT: 忽略非订阅 topic {}", p.topic);
                        continue;
                    }
                    let text = core::str::from_utf8(p.payload).unwrap_or("");
                    match server_msg::parse(text) {
                        ServerMessage::Hello => {
                            awaiting_hello = false;
                            have_seq = false;
                            max_seq = 0;
                            seq_gaps = 0;
                            late_pkts = 0;
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
                                // 长句自测: 直接发文本请求, 触发服务端长 TTS,
                                // 用于无人值守验证下行 UDP 音质/断序
                                #[cfg(feature = "tts-selftest")]
                                {
                                    embassy_time::Timer::after(Duration::from_millis(3000)).await;
                                    // mock LLM 对含「长/测试/讲」的文本返回长回复
                                    let req = "{\"type\":\"listen\",\"state\":\"detect\",\
                                        \"text\":\"请讲一段长文本测试播放稳定性。\"}";
                                    if publish(
                                        &mut socket,
                                        out_buf,
                                        config.publish_topic.as_str(),
                                        req.as_bytes(),
                                    )
                                    .await
                                    .is_ok()
                                    {
                                        info!("长句自测: 已请求长 TTS");
                                    }
                                }
                            }
                        },
                        ServerMessage::TtsStart => {
                            DROP_DOWNLINK.store(false, core::sync::atomic::Ordering::Relaxed);
                            down_pkts = 0;
                            down_bytes = 0;
                            info!("服务器: tts start");
                            channels.event.send(SessionEvent::TtsStart).await;
                        },
                        ServerMessage::TtsStop => {
                            info!(
                                "服务器: tts stop (下行 {} 包 {}B, 断序 {}, 迟到 {}, 非法源 {}, max_seq {})",
                                down_pkts, down_bytes, seq_gaps, late_pkts, bad_src, max_seq
                            );
                            channels.event.send(SessionEvent::TtsStop).await;
                        },
                        ServerMessage::TtsSentenceStart(t) => {
                            info!("服务器: 朗读 \"{}\"", t);
                            ui_send(channels, UiMessage::AssistantText(ui_text::<160>(t)));
                        },
                        ServerMessage::Stt(t) => {
                            info!("服务器: 识别 \"{}\"", t);
                            ui_send(channels, UiMessage::UserText(ui_text::<160>(t)));
                        },
                        ServerMessage::Llm { emotion } => {
                            info!("服务器: 情绪 {}", emotion);
                            ui_send(channels, UiMessage::Emotion(ui_text::<24>(emotion)));
                        },
                        ServerMessage::Alert {
                            status,
                            message,
                            emotion,
                        } => {
                            warn!("服务器 alert: {} / {}", status, message);
                            ui_send(
                                channels,
                                UiMessage::Alert {
                                    status: ui_text::<32>(status),
                                    message: ui_text::<96>(message),
                                    emotion: ui_text::<24>(emotion),
                                },
                            );
                        },
                        ServerMessage::System { command } => {
                            if command == "reboot" {
                                info!("服务器: 远程重启");
                                crate::app::MCP_REBOOT
                                    .store(true, core::sync::atomic::Ordering::Relaxed);
                                crate::app::mcp_maybe_reboot().await;
                            } else {
                                warn!("服务器: 未知 system 命令 {}", command);
                            }
                        },
                        ServerMessage::Goodbye => {
                            let sid =
                                crate::session::json_str_field(text, "session_id").unwrap_or("");
                            if awaiting_hello {
                                // 网关关闭旧 bridge 的通知, 新会话正在建立
                                info!("忽略建连期间的 goodbye (旧会话 {})", sid);
                            } else if !session_id.is_empty() && sid != session_id.as_str() {
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
                        ServerMessage::Error { message } => {
                            warn!("服务器错误: {}", message);
                            udp_ready = false;
                            channels.event.send(SessionEvent::Error).await;
                        },
                        ServerMessage::Mcp => {
                            let volume = VOLUME_PCT.load(core::sync::atomic::Ordering::Relaxed);
                            if let Some(resp) = mcp_handle(text, volume) {
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
                                    crate::app::MCP_REBOOT
                                        .store(false, core::sync::atomic::Ordering::Relaxed);
                                    connected = false;
                                    channels.event.send(SessionEvent::Closed).await;
                                } else {
                                    crate::app::mcp_maybe_reboot().await;
                                }
                            }
                        },
                        ServerMessage::Custom | ServerMessage::Unknown(_) => {
                            crate::diag!("服务器: {}", text);
                        },
                    }
                },
                Packet::PingResp => {
                    awaiting_pong = None;
                },
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
            for _ in 0..64 {
                let recv = with_timeout(Duration::from_millis(1), udp.recv_from(udp_buf)).await;
                let Ok(Ok((n, meta))) = recv else {
                    break;
                };
                // 只接受 hello 下发的服务器地址/端口: 同 LAN 任意主机都能
                // 向本机 UDP 端口发包, 不校验会把随机数据送进 opus 解码。
                let Some((addr, port)) = udp_addr else {
                    continue;
                };
                if !matches!(meta.endpoint.addr, embassy_net::IpAddress::Ipv4(ip) if ip == addr)
                    || meta.endpoint.port != port
                {
                    bad_src = bad_src.saturating_add(1);
                    continue;
                }
                let Some(hdr) = udp_proto::parse_header(&udp_buf[..n]) else {
                    continue;
                };
                if hdr.msg_type != 1 || hdr.payload_len as usize != n - udp_proto::UDP_HEADER_LEN {
                    continue;
                }
                // ssrc 必须等于 hello nonce 里的连接 id: 重连/新会话期间旧连接的
                // 在途包序号可能比新会话大, 不校验会把新会话所有包判成「迟到」。
                let hello_ssrc =
                    u32::from_be_bytes([udp_nonce[4], udp_nonce[5], udp_nonce[6], udp_nonce[7]]);
                if hdr.ssrc != hello_ssrc {
                    bad_src = bad_src.saturating_add(1);
                    continue;
                }
                // 序号: 用序号回绕比较, 先丢弃迟到/重复包, 再算缺口。
                // 旧实现先按 last_seq 算缺口, 一个迟到包会污染后续判断并
                // 凭空插入 PLC 帧 (杂音)。
                let seq = hdr.sequence;
                if have_seq {
                    let diff = seq.wrapping_sub(max_seq);
                    if diff == 0 || diff >= 0x8000_0000 {
                        late_pkts = late_pkts.saturating_add(1);
                        continue;
                    }
                    if diff > 1 {
                        seq_gaps += 1;
                        let missing = diff - 1;
                        if seq_gaps <= 5 || seq_gaps.is_multiple_of(50) {
                            warn!(
                                "UDP 下行不连续: seq {} → {} (缺 {}) 累计 {}",
                                max_seq, seq, missing, seq_gaps
                            );
                        }
                        // 丢包隐藏: 通知音频侧补 PLC 帧 (最多 4 帧, 防通道拥塞)
                        if missing < 64 {
                            let send = channels.downlink.send(DownlinkPacket {
                                timestamp: 0,
                                data: OpusPacket::new(),
                                lost: missing.min(4) as u8,
                            });
                            if with_timeout(Duration::from_secs(2), send).await.is_err() {
                                warn!("下行通道拥塞, 丢弃 PLC");
                            }
                        }
                    }
                } else {
                    have_seq = true;
                }
                max_seq = seq;
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
                    let send = channels.downlink.send(DownlinkPacket {
                        timestamp: hdr.timestamp,
                        data,
                        lost: 0,
                    });
                    if with_timeout(Duration::from_secs(2), send).await.is_err() {
                        warn!("下行通道拥塞, 丢弃一帧");
                    }
                }
            }
        }

        // ---- 心跳 ----
        let now = now_ms();
        if now.saturating_sub(last_ping) >= KEEPALIVE_SECS as u64 * 500 {
            last_ping = now;
            if write_all(&mut socket, &mqtt::PINGREQ_FRAME).await.is_err() {
                connected = false;
                udp_ready = false;
                awaiting_pong = None;
                channels.event.send(SessionEvent::Closed).await;
            } else {
                awaiting_pong = Some(now);
            }
        }
        // 半开连接: broker 已死但 TCP 仍 Established 时, PINGRESP 不会回来
        if let Some(sent) = awaiting_pong
            && now.saturating_sub(sent) > 10_000
        {
            warn!("MQTT PINGRESP 超时 (10s), 重连");
            socket.abort();
            connected = false;
            udp_ready = false;
            awaiting_pong = None;
            channels.event.send(SessionEvent::Closed).await;
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
