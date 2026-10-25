//! 智能音箱主应用 — 对话状态机驱动的编排层 (双任务架构)
//!
//! - 音频主循环 ([`run`]): 采集排空/唤醒 VAD/编码/解码/播放/灯效/显示,
//!   每轮 ≤64ms; 所有网络交互经 embassy-sync 通道非阻塞交换
//!   (单循环长阻塞 await 会让 I2S RX 饱和触发 Late, 见真机回归记录)
//! - 会话任务 ([`session_task`]): WS 连接/握手/收发, 长阻塞 await 集中于此
//!
//! 状态转移委托 [`xiaozhi_core::dialog::DialogSm`] (host 单测覆盖)。

use core::fmt::Write as _;

use embassy_net::{Ipv4Address, Stack};
use embassy_sync::channel::Channel;

use esp_hal::{Blocking, dma::DmaTransferRxCircular, i2s::master::I2sRx};

use defmt::{error, info, warn};

use xiaozhi_core::audio::capture::{
    apply_gain_percent, extract_slot, is_voice_active, level_stats, limit_peak,
};
use xiaozhi_core::audio::listen::{ListenTurn, ListenTurnConfig};
use xiaozhi_core::audio::wake::{
    VadWakeDetector, WakeEngine, WakeEvent, WakePolicy, WakePolicyConfig, adaptive_threshold,
    update_noise_floor,
};
use xiaozhi_core::dialog::{Action, DialogEvent, DialogSm, DialogState, ListenMode};
use xiaozhi_core::idle::IdlePromptPolicy;
use xiaozhi_core::server_msg::{self, ServerMessage};

use crate::audio::opus::{MAX_PACKET_BYTES, OpusDecoder, OpusEncoder};

impl defmt::Format for SessionCommand {
    fn format(&self, f: defmt::Formatter) {
        match self {
            SessionCommand::Connect => defmt::write!(f, "Connect"),
            SessionCommand::SendListenStart(_) => defmt::write!(f, "ListenStart"),
            SessionCommand::SendListenStop => defmt::write!(f, "ListenStop"),
            SessionCommand::SendTtsAbort => defmt::write!(f, "TtsAbort"),
            SessionCommand::CloseSession => defmt::write!(f, "CloseSession"),
        }
    }
}
use crate::audio::playback::Playback;
use crate::bsp::components::{
    Audio, Battery, BreathSpeed, Buttons, Display, LedMode, Leds, Power, Rgb565,
};
use crate::prompt::{Clip, PromptPlayer};
use crate::session;

/// 上行音频参数 (16kHz 单声道 60ms, 对齐 C++ 参考与 hello.audio_params;
/// 主任务栈已扩至 107KB, 可容纳 60ms 帧的 ~87KB 编码栈需求)
const SAMPLE_RATE: u32 = 16_000;
const FRAME_MS: u32 = 60;
const FRAME_SAMPLES: usize = SAMPLE_RATE as usize / 1000 * FRAME_MS as usize; // 960
/// 软件增益百分比: 麦克风数字电平偏低, 上行前放大 (原始域 ×4 ≈ +12dB)
///
/// 真机实测 (2026-09-26): 采集时钟修复 (环形 TX 常流) 后原始人声峰值达
/// 2000~5000, 旧的 800% 会把样本打到 ±32767 (削顶) → ASR 失效。
/// 400% 后上行 rms 约 2000~4000, 与参考样本 (test_query.raw) 电平一致。
const GAIN_PERCENT: u16 = 600;
/// 上行限幅门限: 留 ~1.4dB 余量, 防 i16 饱和削顶 (削顶会毁掉 ASR)
const UPLINK_CEILING: i16 = 28_000;
/// 唤醒 VAD 阈值 (均方幅度, 原始采集电平域)
///
/// 真机实测 (2026-10-23): 安静时 rms 150~300, 正常说话 rms 300~1300。
/// 固定阈值 300 会误唤醒, 700 又唤不醒; 现以 400 为下限, 按噪声底
/// `1.5x + 100` 动态上调 (上限 650), 配合 4 帧去抖 (~256ms)。
pub const VAD_THRESHOLD: i16 = 400;
/// 回合结束用 VAD 阈值: 比唤醒阈值低 (原始人声多数帧 rms 200~400);
/// 误判 "说过话" 只影响收尾时机, 不会误唤醒, 故取低值保证不丢话。
const LISTEN_VAD_THRESHOLD: i16 = 180;
/// 回合结束: 累计有声至少 250ms 才算"说过话"
///
/// 真机 2026-10-23: 环境噪声/唤醒提示音让单帧即置 heard, 随后 500ms
/// 安静就判停 (总时长仅 1.5s), 用户还没开口回合已结束 → 服务器只收到
/// 噪声/静音。改为累计有声时长门槛, 滤掉瞬态噪声。
const LISTEN_MIN_VOICED_MS: u64 = 250;
/// 回合结束: 说完后持续静音 1500ms 才收尾 (原 500ms/800ms 都太短)
const LISTEN_SILENCE_MS: u64 = 1_500;
/// 聆听前 600ms 不统计有声 (codec/DMA 上电瞬态)
const LISTEN_WARMUP_MS: u64 = 600;
/// 从未检测到人声时, 12s 后收尾 (给多轮对话留思考时间)
const LISTEN_NO_VOICE_MS: u64 = 12_000;
/// 唤醒预录: 保存最近 1s 原始 PCM, 覆盖建连窗口 (~0.7s) 内丢失的语音
const PREROLL_SAMPLES: usize = 16_000;

/// 乒乓窗口 = 单个 DMA 描述符 (4092B ≈ 64ms): available() 按完整描述符计数
const BLOCK_BYTES: usize = 4092;

/// opus 包通道项 (单包上限 1276B)
pub type OpusPacket = heapless::Vec<u8, MAX_PACKET_BYTES>;

/// 上行音频包: opus + 服务端 AEC 时间戳 (版本 1 恒为 0)
#[derive(Clone)]
pub struct UplinkPacket {
    pub timestamp: u32,
    pub data: OpusPacket,
}

/// 下行音频包: opus + 服务器时间戳 (版本 1 恒为 0)
///
/// `lost > 0` 表示前面丢了 `lost` 个包, 由解码器做 PLC (丢包隐藏) 补帧。
#[derive(Clone)]
pub struct DownlinkPacket {
    pub timestamp: u32,
    pub data: OpusPacket,
    pub lost: u8,
}

/// 跨任务共享的设备状态 (MCP get_device_status / set_volume 用)
pub static VOLUME_PCT: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(60);

/// 打断后丢弃服务器剩余下行音频: 把会话任务带宽留给上行, 防 uplink 饿死
pub static DROP_DOWNLINK: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
/// 当前会话轮次 (日志关联用)
/// MCP self.reboot 待执行 (会话任务发完应答后复位)
pub static MCP_REBOOT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// 当前会话轮次 (日志关联用)
pub static TURN_ID: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// 会话任务心跳: 每轮循环 +1; 音频循环据此发现「任务卡死但主循环还活着」
pub static SESSION_HEARTBEAT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
pub static BATTERY_PCT: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
pub static CHARGING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// MCP set_volume 待办 (valid 标记 + 目标值; 音频循环轮询消费)
pub static PENDING_VOLUME: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
pub static PENDING_VOLUME_VALID: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
type RawMutex = embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

/// 音频主循环返回原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// RX 饱和/故障: esp-hal 的 available 计数已损坏, 需重建采集句柄
    CaptureFault,
}

/// 音频主循环 → 会话任务 命令
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCommand {
    /// 建立会话 (连接 + hello)
    Connect,
    /// 发送 listen start
    SendListenStart(ListenMode),
    /// 发送 listen stop
    SendListenStop,
    /// 打断 TTS
    SendTtsAbort,
    /// 本地会话结束: 通知服务器关闭通道 (MQTT 网关据此释放 bridge)
    CloseSession,
}

/// 会话任务 → 音频主循环 事件
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEvent {
    /// 会话就绪 (hello 完成)
    Ready,
    /// 服务器开始播报
    TtsStart,
    /// 服务器播报结束
    TtsStop,
    /// 会话异常关闭 (网络/建连失败) — 与正常收尾区分, 用于提示音/显示
    Error,
    /// 会话关闭
    Closed,
}

/// 服务器文本/情绪 (显示层; 音频主循环按「最新值」覆盖, 不做历史)
#[derive(Debug, Clone)]
pub enum UiMessage {
    /// 用户语音识别文本 (stt)
    UserText(heapless::String<160>),
    /// 助手朗读文本 (tts sentence_start)
    AssistantText(heapless::String<160>),
    /// 情绪名 (llm.emotion, 用于表情)
    Emotion(heapless::String<24>),
    /// 服务器 alert: 状态/文案/情绪
    Alert {
        status: heapless::String<32>,
        message: heapless::String<96>,
        emotion: heapless::String<24>,
    },
}

/// 截断到固定容量 (按字符边界, 超长丢弃尾部)
pub(crate) fn ui_text<const N: usize>(text: &str) -> heapless::String<N> {
    let mut out = heapless::String::new();
    for ch in text.chars() {
        if out.push(ch).is_err() {
            break;
        }
    }
    out
}

/// 会话任务 → 音频主循环: 显示消息 (尽力发送, 满则丢弃旧语义)
pub(crate) fn ui_send(channels: &Channels, msg: UiMessage) {
    let _ = channels.ui.try_send(msg);
}

/// 任务间通道 (main 装配一次, 静态常驻)
pub struct Channels {
    /// 音频 → 会话: 编码后的 opus 帧 (+ AEC 时间戳)
    /// 40 槽 ≈2.4s, 覆盖一次慢帧/半开读 (WS 1.5s / MQTT 2s) 造成的阻塞
    pub uplink: Channel<RawMutex, UplinkPacket, 40>,
    /// 会话 → 音频: 下行 opus 帧 (TTS, + 服务器时间戳)
    pub downlink: Channel<RawMutex, DownlinkPacket, 16>,
    /// 音频 → 会话: 命令 (与 pending 容量对齐, 避免静默丢弃)
    pub command: Channel<RawMutex, SessionCommand, 8>,
    /// 会话 → 音频: 事件
    pub event: Channel<RawMutex, SessionEvent, 4>,
    /// 会话 → 音频: 显示文本/情绪 (stt/llm/tts/alert)
    pub ui: Channel<RawMutex, UiMessage, 4>,
}

/// 会话任务依赖 ('static: embassy 任务要求参数永久存活)
pub struct SessionArgs {
    pub stack: Stack<'static>,
    pub server: Ipv4Address,
    pub mac: heapless::String<32>,
    pub channels: &'static Channels,
    /// xiaozhi 协议版本 (1 = 裸 opus, 2/3 = 带帧头)
    pub ws_version: u16,
}

/// 会话任务: 连接/握手/收发 (所有长阻塞 await 集中在此, 不影响音频主循环)
#[embassy_executor::task]
pub async fn session_task(args: SessionArgs) {
    let SessionArgs {
        stack,
        server,
        mac,
        channels,
        ws_version,
    } = args;

    // socket 持有于任务内, 跨重连复用 (每次 connect 泄漏缓冲会耗尽内部堆)
    let rx = alloc::vec![0u8; 4096].leak();
    let tx = alloc::vec![0u8; 4096].leak();
    let mut socket = embassy_net::tcp::TcpSocket::new(stack, rx, tx);
    let mut connected = false;
    let mut want_connect = false;
    // 下行统计 (诊断重复播放: 每次 tts stop 输出包数/字节数)
    let mut down_pkts: u32 = 0;
    let mut down_bytes: u64 = 0;

    loop {
        SESSION_HEARTBEAT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        // ---- WiFi 掉线: 当前 WS 会话不可信, 立即收尾等重连 ----
        // (重连任务负责重新关联; 这里只关闭并 abort socket, 下次唤醒重建)
        if connected && !stack.is_link_up() {
            warn!("WiFi 链路断开 — 关闭当前会话");
            socket.abort();
            connected = false;
            channels.event.send(SessionEvent::Closed).await;
        }
        // TCP 对端关闭/半开: can_recv() 在缓冲空时为 false, 必须查状态
        if connected && socket.state() != embassy_net::tcp::State::Established {
            warn!("WS 链路已断开 (TCP 状态异常) — 会话关闭");
            socket.abort();
            connected = false;
            channels.event.send(SessionEvent::Closed).await;
        }

        // ---- 命令 ----
        loop {
            match channels.command.try_receive() {
                Ok(SessionCommand::Connect) => want_connect = true,
                Ok(SessionCommand::SendListenStart(mode)) => {
                    if connected {
                        let msg = if mode == ListenMode::Auto {
                            session::LISTEN_START_AUTO
                        } else {
                            session::LISTEN_START_MANUAL
                        };
                        let mut ws = session::WsClient {
                            socket: &mut socket,
                        };
                        if ws.send_text(msg.as_bytes()).await.is_err() {
                            connected = false;
                            channels.event.send(SessionEvent::Closed).await;
                        }
                    }
                },
                Ok(SessionCommand::SendListenStop) => {
                    if connected {
                        let mut ws = session::WsClient {
                            socket: &mut socket,
                        };
                        if ws.send_text(session::LISTEN_STOP.as_bytes()).await.is_err() {
                            connected = false;
                            channels.event.send(SessionEvent::Closed).await;
                        }
                    }
                },
                Ok(SessionCommand::CloseSession) => {
                    // 与 MQTT 对齐: 显式 goodbye, 让服务端尽快释放会话/通道
                    // (WS 服务端即使忽略也无害)
                    if connected {
                        let mut ws = session::WsClient {
                            socket: &mut socket,
                        };
                        let _ = ws.send_text(session::GOODBYE.as_bytes()).await;
                    }
                },
                Ok(SessionCommand::SendTtsAbort) => {
                    if connected {
                        let mut ws = session::WsClient {
                            socket: &mut socket,
                        };
                        if ws.send_text(session::TTS_ABORT.as_bytes()).await.is_err() {
                            connected = false;
                            channels.event.send(SessionEvent::Closed).await;
                        }
                    }
                },
                Err(_) => break,
            }
        }

        // ---- 建连 (阻塞仅影响本任务; 音频主循环照常排空采集) ----
        if want_connect {
            want_connect = false;
            if connected {
                // 已连接: 补发就绪。上一轮"按下 BOOT→建连中松开→回 Idle"时,
                // 迟到的 Ready 被状态机忽略, 但会话仍活着; 若不补发,
                // 下次按键会一直停在 Connecting (永远等不到就绪) — 真机复现。
                channels.event.send(SessionEvent::Ready).await;
            } else {
                info!("会话任务: 开始连接...");
                let started = esp_hal::time::Instant::now().duration_since_epoch();
                match embassy_time::with_timeout(
                    embassy_time::Duration::from_secs(8),
                    session::connect(&mut socket, server, &mac, ws_version),
                )
                .await
                {
                    Ok(Ok((_rate, _frame_ms))) => {
                        connected = true;
                        let elapsed = esp_hal::time::Instant::now()
                            .duration_since_epoch()
                            .saturating_sub(started)
                            .as_millis();
                        info!("会话连接完成: {}ms", elapsed);
                        channels.event.send(SessionEvent::Ready).await;
                    },
                    Ok(Err(e)) => {
                        warn!("会话连接失败: {}", e);
                        channels.event.send(SessionEvent::Closed).await;
                    },
                    Err(_) => {
                        warn!("会话连接超时 (8s) — 中止");
                        socket.abort();
                        channels.event.send(SessionEvent::Closed).await;
                    },
                }
            }
        }

        // ---- 上行: 编码帧 → WS ----
        while let Ok(pkt) = channels.uplink.try_receive() {
            if connected {
                let mut ws = session::WsClient {
                    socket: &mut socket,
                };
                if ws
                    .send_audio(ws_version, pkt.timestamp, &pkt.data)
                    .await
                    .is_err()
                {
                    connected = false;
                    channels.event.send(SessionEvent::Closed).await;
                }
            }
        }

        // ---- 服务器帧: 文本 → 事件/MCP; 二进制 → 下行通道 ----
        if connected {
            let mut ws = session::WsClient {
                socket: &mut socket,
            };
            if ws.socket.can_recv() {
                match ws.recv_frame().await {
                    Ok((opcode, payload)) => match opcode {
                        1 => {
                            let text = core::str::from_utf8(&payload).unwrap_or("");
                            match server_msg::parse(text) {
                                ServerMessage::TtsStart => {
                                    // 新回复开始: 解除打断后的下行丢弃
                                    DROP_DOWNLINK
                                        .store(false, core::sync::atomic::Ordering::Relaxed);
                                    info!("服务器: tts start");
                                    channels.event.send(SessionEvent::TtsStart).await;
                                },
                                ServerMessage::TtsStop => {
                                    info!(
                                        "服务器: tts stop (下行 {} 包 {}B)",
                                        down_pkts, down_bytes
                                    );
                                    down_pkts = 0;
                                    down_bytes = 0;
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
                                        MCP_REBOOT
                                            .store(true, core::sync::atomic::Ordering::Relaxed);
                                        mcp_maybe_reboot().await;
                                    } else {
                                        warn!("服务器: 未知 system 命令 {}", command);
                                    }
                                },
                                ServerMessage::Error { message } => {
                                    warn!("服务器错误: {}", message);
                                    channels.event.send(SessionEvent::Error).await;
                                    channels.event.send(SessionEvent::Closed).await;
                                },
                                ServerMessage::Mcp => {
                                    let volume =
                                        VOLUME_PCT.load(core::sync::atomic::Ordering::Relaxed);
                                    if let Some(resp) = mcp_handle(text, volume) {
                                        info!("MCP 应答: {}", resp.as_str());
                                        if ws.send_text(resp.as_bytes()).await.is_err() {
                                            MCP_REBOOT.store(
                                                false,
                                                core::sync::atomic::Ordering::Relaxed,
                                            );
                                            connected = false;
                                            channels.event.send(SessionEvent::Closed).await;
                                        } else {
                                            mcp_maybe_reboot().await;
                                        }
                                    }
                                },
                                ServerMessage::Hello
                                | ServerMessage::Goodbye
                                | ServerMessage::Custom
                                | ServerMessage::Unknown(_) => {
                                    info!("服务器: {}", text);
                                },
                            }
                        },
                        2 => {
                            down_pkts += 1;
                            down_bytes += payload.len() as u64;
                            if DROP_DOWNLINK.load(core::sync::atomic::Ordering::Relaxed) {
                                // 打断后剩余音频直接丢弃, 不占用通道
                                continue;
                            }
                            let (timestamp, opus) =
                                match session::decode_audio(ws_version, &payload) {
                                    Ok(v) => v,
                                    Err(e) => {
                                        warn!("下行帧解析失败: {} — 丢弃", e);
                                        continue;
                                    },
                                };
                            let mut pkt: OpusPacket = heapless::Vec::new();
                            if pkt.extend_from_slice(opus).is_ok() {
                                // 通道满时短暂反压服务器; 但音频侧若停摆,
                                // 不能让网络任务永久卡死 (否则心跳/命令全停)。
                                let send = channels.downlink.send(DownlinkPacket {
                                    timestamp,
                                    data: pkt,
                                    lost: 0,
                                });
                                if embassy_time::with_timeout(
                                    embassy_time::Duration::from_secs(2),
                                    send,
                                )
                                .await
                                .is_err()
                                {
                                    warn!("下行通道拥塞, 丢弃一帧");
                                }
                            }
                        },
                        8 => {
                            info!("服务器关闭连接");
                            // 回 close 并立即释放 socket, 不留半开连接
                            let _ = ws.send_frame(8, &[]).await;
                            ws.socket.abort();
                            connected = false;
                            channels.event.send(SessionEvent::Closed).await;
                        },
                        9 => {
                            let _ = ws.send_pong(&payload).await;
                        },
                        _ => {},
                    },
                    Err(e) => {
                        warn!("WS 接收错误: {} — 会话关闭", e);
                        ws.socket.abort();
                        connected = false;
                        channels.event.send(SessionEvent::Closed).await;
                    },
                }
            }
        }

        embassy_time::Timer::after(embassy_time::Duration::from_millis(5)).await;
    }
}

/// 应用运行依赖 (借用于 main 的常驻资源)
///
/// `'a`: 各组件的重借用; `'b`: 采集句柄持有的 board.capture 借用
pub struct AppArgs<'a, 'b> {
    pub audio: &'a mut Audio,
    pub buttons: &'a mut Buttons,
    pub leds: &'a mut Leds,
    pub display: &'a mut Display,
    pub battery: &'a mut Battery,
    pub power: &'a mut Power,
    pub cap: &'a mut DmaTransferRxCircular<'b, I2sRx<'static, Blocking>>,
    pub channels: &'a Channels,
    pub flash: &'a mut esp_storage::FlashStorage<'static>,
    /// 常驻播放管线: 由 main 只分配一次, CaptureFault 重入时复用
    pub playback: &'a mut Playback,
    /// 常驻唤醒预录环: 同上, 避免每次重入泄漏 32KB
    pub preroll: &'a mut [i16],
    /// 服务端 AEC 已启用 (协议版本 ≥2): 播报期间持续上行, 由服务端消除回声
    pub server_aec: bool,
}

/// 单声道 PCM → 60ms opus 帧 → 上行通道 (非阻塞)
///
/// 返回本轮成功入队的帧数。时间戳取自 [`AecClock`] (服务端 AEC 对齐);
/// 协议版本 1 时该队列始终为空, 时间戳恒为 0。
fn encode_uplink(
    pcm: &[i16],
    acc_buf: &mut [i16],
    acc_len: &mut usize,
    encoder: &mut OpusEncoder,
    channels: &Channels,
    aec_clock: &mut xiaozhi_core::protocol::AecClock,
    uplink_frames: &mut u64,
) -> usize {
    let mut sent = 0usize;
    let mut src = pcm;
    while !src.is_empty() {
        let take = usize::min(acc_buf.len() - *acc_len, src.len());
        acc_buf[*acc_len..*acc_len + take].copy_from_slice(&src[..take]);
        *acc_len += take;
        src = &src[take..];
        while *acc_len >= FRAME_SAMPLES {
            let mut raw = [0u8; MAX_PACKET_BYTES];
            match encoder.encode(&acc_buf[..FRAME_SAMPLES], &mut raw) {
                Ok(len) => {
                    *uplink_frames += 1;
                    if uplink_frames.is_multiple_of(100) {
                        crate::diag!("上行心跳: {} 帧", uplink_frames);
                    }
                    let mut data: OpusPacket = heapless::Vec::new();
                    if data.extend_from_slice(&raw[..len]).is_ok()
                        && channels
                            .uplink
                            .try_send(UplinkPacket {
                                timestamp: aec_clock.pop(),
                                data,
                            })
                            .is_err()
                    {
                        warn!("上行通道满, 丢弃一帧");
                    } else {
                        sent += 1;
                    }
                },
                Err(e) => warn!("opus 编码失败: {}", e),
            }
            acc_buf.copy_within(FRAME_SAMPLES..*acc_len, 0);
            *acc_len -= FRAME_SAMPLES;
        }
    }
    sent
}

/// 主事件循环 (音频侧)
///
/// 返回 [`RunOutcome::CaptureFault`] 时调用方需重建采集句柄后重新进入
/// (esp-hal RX 饱和后只能整体重建, 且重建必须在直线路径上做, 见 AGENTS.md)。
#[allow(clippy::too_many_arguments, unused_variables, unused_assignments)]
pub async fn run(
    args: AppArgs<'_, '_>,
    pop_buf: &mut [u8],
    mono_buf: &mut [i16],
    acc_buf: &mut [i16],
    silence_buf: &mut [u8],
) -> RunOutcome {
    let AppArgs {
        audio,
        buttons,
        leds,
        display,
        battery,
        power,
        cap,
        channels,
        flash,
        playback,
        preroll,
        server_aec,
    } = args;

    // 复用常驻管线: 上一次 CaptureFault 可能留下未播完的帧
    playback.reset();

    let mut sm = DialogSm::new();
    // 下行播放时间戳 → 上行帧 (服务端 AEC 对齐; 版本 1 恒为 0)
    let mut aec_clock = xiaozhi_core::protocol::AecClock::new();
    // 开机预热 1s: codec/DMA 上电瞬态 (含 DC 漂移) 可能越过 VAD 阈值,
    // 曾致开机假唤醒 → 卡死 (见 ROADMAP M8); 与 VAD 去 DC 双保险。
    let mut wake = WakeEngine::new(
        VadWakeDetector::new(VAD_THRESHOLD),
        WakePolicy::new(WakePolicyConfig {
            warmup_ms: 1_000,
            // 4 帧 ≈256ms 持续人声: 滤掉环境噪声瞬态
            debounce_frames: 4,
            ..Default::default()
        }),
    );
    let mut encoder: Option<OpusEncoder> = None;
    let mut decoder: Option<OpusDecoder> = None;
    // 编解码器创建失败退避: 避免每 1ms 重试 + error! 日志风暴
    let mut codec_retry_at: u64 = 0;
    let mut codec_fail_count: u8 = 0;
    let mut pcm_dec: alloc::boxed::Box<[i16]> = alloc::vec![0i16; FRAME_SAMPLES].into_boxed_slice();
    let mut stereo_out: alloc::boxed::Box<[u8]> =
        alloc::vec![0u8; FRAME_SAMPLES * 4].into_boxed_slice();
    // 空闲提示: 60s 无活动 → 播放语音提示 → 进入低功耗待机
    // (idle-selftest: 窗口缩到 5s, 便于真机验证流程)
    const IDLE_WINDOW_MS: u64 = if cfg!(feature = "idle-selftest") {
        5_000
    } else {
        60_000
    };
    let mut idle_policy = IdlePromptPolicy::new(IDLE_WINDOW_MS, 0);
    info!("空闲提示窗口: {}s", IDLE_WINDOW_MS / 1000);
    let mut prompt = PromptPlayer::new();
    let mut prompt_until_ms: u64 = 0;
    let mut prompt_vol_restore_at: u64 = 0;
    let mut low_power = false;
    let mut pending_connect = false;
    let mut connect_deadline: u64 = 0;
    let mut ack_pending = false;
    let mut ack_flush_at: u64 = 0;
    let mut volume_dirty = false;
    let preroll_len = preroll.len();
    let mut preroll_w = 0usize;
    let mut preroll_valid = 0usize;
    let mut preroll_flush = false;
    // 预录补发上限: 编码器/通道异常时不能永远卡在 "flushing" 而丢掉实时音频
    let mut preroll_deadline: u64 = 0;
    let mut preroll_off = 0usize;
    let mut preroll_seq: u64 = 0;
    let mut preroll_from: u64 = 0;
    // 提示音/开机静音窗: 防扬声器声音被麦克风录进上行
    let mut mic_mute_until_ms: u64 = 0;
    // TTS 结束后的静默期: 防止扬声器尾音/回声立刻触发本地收尾
    let mut listen_grace_until_ms: u64 = 0;
    let mut boot_ms: u64 = 0;
    // 会话任务心跳检测 (任务卡死但音频循环存活时兜底)
    let mut hb_last: u32 = 0;
    let mut hb_ms: u64 = 0;
    info!(
        "PCM 环: {}KB ({})",
        playback.capacity() / 1024,
        if playback.capacity() == crate::audio::playback::QUEUE_MAX {
            "PSRAM"
        } else {
            "内部回退"
        }
    );
    let mut barge_in_active = false;
    let mut tx_rate_bytes: u64 = 0;
    let mut tx_rate_ms: u64 = 0;
    let mut acc_len = 0usize;
    let mut held = false;
    // 音量键: 单击 ±10, 长按 (≥800ms) 直达 100/静音 (对齐 C++ 参考)
    let mut vol_dir: i8 = 0;
    let mut vol_since_ms: u64 = 0;
    let mut vol_long = false;
    // 通知行: 音量/低电/错误 (状态行下方的临时文本)
    let mut notify_until_ms: u64 = 0;
    let mut notify_text: heapless::String<32> = heapless::String::new();
    let mut notify_shown: Option<heapless::String<32>> = None;
    // 低电提示锁存: 掉到 20% 以下只提醒一次, 回升到 25% 或充电后复位
    let mut low_batt_latched = false;
    // 最近一次显示用文本 (UI 批次接屏幕时渲染; 当前仅诊断可读)
    let mut last_user_text: heapless::String<160> = heapless::String::new();
    let mut last_assistant_text: heapless::String<160> = heapless::String::new();
    let mut last_emotion: heapless::String<24> = heapless::String::new();
    let mut last_led: Option<LedMode> = None;
    let mut last_state: Option<DialogState> = None;
    let mut state_since_ms = 0u64;
    let mut battery_last_read = 0u64;
    let mut uplink_frames: u64 = 0;
    let mut turn_up_frames: u32 = 0;
    let mut turn_rms_max: i32 = 0;
    let mut preroll_frames: u32 = 0;
    let mut last_level_log_ms: u64 = 0;
    let mut idle_peak_rms: i32 = 0;
    let mut noise_ema: i32 = 0;
    let mut listen_peak_rms: i32 = 0;
    // 下行解码统计 (诊断重复播放)
    let mut dec_pkts: u32 = 0;
    let mut dec_samples: u64 = 0;
    // 上行回合检测 (auto 模式): 说完 → 本地 VAD 静音 → 主动 listen stop
    let mut listen_turn = ListenTurn::new(ListenTurnConfig {
        warmup_ms: LISTEN_WARMUP_MS,
        min_voiced_ms: LISTEN_MIN_VOICED_MS,
        silence_ms: LISTEN_SILENCE_MS,
        no_voice_ms: LISTEN_NO_VOICE_MS,
    });
    let mut was_listening = false;
    let mut raw_dumped = false;
    let mut pending: heapless::Vec<(Action, ListenMode), 8> = heapless::Vec::new();

    for (reg, val) in audio.es7210_debug_regs() {
        crate::diag!("ES7210 0x{:02X}=0x{:02X}", reg, val);
    }
    for (off, val) in Audio::i2s_debug_regs() {
        crate::diag!("I2S0 0x{:02X}=0x{:08X}", off, val);
    }

    info!("=== 主循环启动: 待机监听 (说话唤醒 / 按住 BOOT 对讲) ===");

    loop {
        let now_ms = esp_hal::time::Instant::now()
            .duration_since_epoch()
            .as_millis();
        if boot_ms == 0 {
            boot_ms = now_ms;
            // 空闲窗口从主循环真正开始计（开机/WiFi/OTA 的十多秒不算空闲，
            // 也避免 systimer 未清零时窗口"已经到期"导致复位后立刻提示）
            idle_policy.note_activity(now_ms);
            hb_ms = now_ms;
            hb_last = SESSION_HEARTBEAT.load(core::sync::atomic::Ordering::Relaxed);
        }
        // 喂 RWDT: 主循环存活即视为系统正常 (网络任务独立, 不参与)
        crate::wdt::feed();

        // ---- 会话自测 (feature `session-selftest`, 默认关) ----
        // 开机 5s 后触发, 之后每 60s 空闲时再触发一次 (浸泡测试用)
        #[cfg(feature = "session-selftest")]
        if sm.state() == DialogState::Idle && now_ms >= 5_000 {
            use core::sync::atomic::{AtomicU32, Ordering};
            static LAST: AtomicU32 = AtomicU32::new(0);
            let now32 = now_ms as u32;
            let last = LAST.load(Ordering::Relaxed);
            if last == 0 || now32.wrapping_sub(last) >= 60_000 {
                LAST.store(now32, Ordering::Relaxed);
                info!("会话自测: 自动触发唤醒 (t={}ms)", now_ms);
                dispatch(&mut sm, &mut pending, DialogEvent::WakeTriggered);
            }
        }

        // ---- 1. 会话事件 ----
        loop {
            match channels.event.try_receive() {
                Ok(SessionEvent::Ready) => {
                    // 就绪提示音: 告知用户连接完成、此刻可开口 (手动/语音两种入口)
                    crate::diag!(
                        "[T{}] ready t={}",
                        TURN_ID.load(core::sync::atomic::Ordering::Relaxed),
                        now_ms
                    );
                    audio.beep(150);
                    dispatch(&mut sm, &mut pending, DialogEvent::SessionReady)
                },
                Ok(SessionEvent::TtsStart) => {
                    DROP_DOWNLINK.store(false, core::sync::atomic::Ordering::Relaxed);
                    dec_pkts = 0;
                    dec_samples = 0;
                    // 新一轮播报: 丢弃上一轮残留的 AEC 时间戳
                    aec_clock.clear();
                    playback.on_tts_start();
                    dispatch(&mut sm, &mut pending, DialogEvent::TtsStart)
                },
                Ok(SessionEvent::TtsStop) => {
                    // 等软件队列 + DMA 排空后由 poll_stop 派发, 防回声循环
                    playback.on_tts_stop();
                },
                Ok(SessionEvent::Error) => {
                    error!("会话异常 (网络/建连失败)");
                    // 错误提示音走本地提示通道, 同时安静地结束会话
                    if !prompt.is_active() {
                        // 低功耗待机时功放已断电, 提示音前先恢复
                        if !audio.pa_enabled() {
                            audio.set_pa(true);
                        }
                        low_power = false;
                        prompt.play(Clip::Error);
                        prompt_until_ms = now_ms + prompt.duration_ms() + 400;
                    }
                    notify_text.clear();
                    let _ = write!(&mut notify_text, "NET ERROR");
                    notify_until_ms = now_ms + 5_000;
                    dispatch(&mut sm, &mut pending, DialogEvent::SessionClosed)
                },
                Ok(SessionEvent::Closed) => {
                    dispatch(&mut sm, &mut pending, DialogEvent::SessionClosed)
                },
                Err(_) => break,
            }
        }

        // ---- 1.5 显示消息 (stt / llm / tts 句子 / alert) ----
        // 文本先缓存, 屏幕 UI 批次再渲染; 情绪名是 ASCII, 当前先做通知显示。
        while let Ok(msg) = channels.ui.try_receive() {
            match msg {
                UiMessage::UserText(t) => {
                    crate::diag!("UI 用户: {}", t.as_str());
                    last_user_text = t;
                },
                UiMessage::AssistantText(t) => {
                    crate::diag!("UI 助手: {}", t.as_str());
                    last_assistant_text = t;
                },
                UiMessage::Emotion(e) => {
                    crate::diag!("UI 情绪: {}", e.as_str());
                    if e.as_str() != last_emotion.as_str() {
                        notify_text.clear();
                        let _ = write!(&mut notify_text, "EMO {}", e.as_str());
                        notify_until_ms = now_ms + 1_500;
                    }
                    last_emotion = e;
                },
                UiMessage::Alert {
                    status,
                    message,
                    emotion,
                } => {
                    warn!("UI alert: {} / {}", status.as_str(), message.as_str());
                    last_emotion = emotion;
                    notify_text.clear();
                    let _ = write!(&mut notify_text, "ALERT");
                    notify_until_ms = now_ms + 5_000;
                    // 播报中不插播提示音 (会打断 TTS), 仅显示
                    if sm.state() != DialogState::Speaking && !prompt.is_active() {
                        if !audio.pa_enabled() {
                            audio.set_pa(true);
                        }
                        low_power = false;
                        prompt.play(Clip::Alert);
                        prompt_until_ms = now_ms + prompt.duration_ms() + 400;
                        // 聆听中插播提醒音: 静音窗内不上行, 防把提示音发给服务器
                        mic_mute_until_ms = prompt_until_ms;
                        idle_policy.note_activity(now_ms);
                    }
                },
            }
        }

        // ---- 1.6 会话任务心跳: 任务卡死但音频循环还活着时兜底 ----
        let hb = SESSION_HEARTBEAT.load(core::sync::atomic::Ordering::Relaxed);
        if hb != hb_last {
            hb_last = hb;
            hb_ms = now_ms;
        }
        if sm.state() != DialogState::Idle && now_ms.saturating_sub(hb_ms) > 20_000 {
            warn!("会话任务 20s 无心跳, 结束会话");
            dispatch(&mut sm, &mut pending, DialogEvent::SessionClosed);
            hb_ms = now_ms;
        }

        // ---- 2. 会话拆除 / 编解码器生命周期 ----
        if sm.state() == DialogState::Idle {
            if encoder.is_some() {
                wake.policy_mut().note_playback_end(now_ms);
                encoder = None;
                decoder = None;
                playback.reset();
                preroll_flush = false;
                preroll_off = 0;
                preroll_deadline = 0;
                DROP_DOWNLINK.store(false, core::sync::atomic::Ordering::Relaxed);
                info!("会话已结束, 回到待机监听");
            }
            codec_fail_count = 0;
            codec_retry_at = 0;
        } else if encoder.is_none()
            && sm.state() == DialogState::Listening
            && now_ms >= codec_retry_at
        {
            match (
                OpusEncoder::new_voip_mono(SAMPLE_RATE, FRAME_MS),
                OpusDecoder::new_mono(SAMPLE_RATE),
            ) {
                (Ok(enc), Ok(dec)) => {
                    encoder = Some(enc);
                    decoder = Some(dec);
                    codec_fail_count = 0;
                    codec_retry_at = 0;
                },
                _ => {
                    codec_fail_count = codec_fail_count.saturating_add(1);
                    codec_retry_at = now_ms + 1_000;
                    if codec_fail_count >= 5 {
                        // 连续失败 (堆耗尽/碎片): 结束会话回 Idle, 避免静默卡死
                        error!("opus 编解码器连续创建失败, 结束会话");
                        dispatch(&mut sm, &mut pending, DialogEvent::SessionClosed);
                    } else {
                        error!("opus 编解码器创建失败 (1s 后重试)");
                    }
                },
            }
        }
        // 唤醒预录补发: 非阻塞分片 (每轮 ≤4 帧), 避免阻塞主循环
        // 导致 I2S RX 排空不及时 (DmaError::Late → 采集重建)
        if preroll_flush {
            if preroll_deadline != 0 && now_ms >= preroll_deadline {
                warn!("预录补发超时, 放弃剩余帧并恢复实时上行");
                preroll_flush = false;
                preroll_off = 0;
                preroll_deadline = 0;
            } else if let Some(enc) = encoder.as_mut() {
                let total = ((preroll_seq - preroll_from) as usize).min(preroll_valid);
                let start = (preroll_w + preroll_len - total) % preroll_len;
                let mut sent = 0;
                while preroll_off < total && sent < 4 {
                    let n = (total - preroll_off).min(FRAME_SAMPLES);
                    for i in 0..n {
                        pcm_dec[i] = preroll[(start + preroll_off + i) % preroll_len];
                    }
                    for i in n..FRAME_SAMPLES {
                        pcm_dec[i] = 0;
                    }
                    apply_gain_percent(&mut pcm_dec[..FRAME_SAMPLES], GAIN_PERCENT);
                    limit_peak(&mut pcm_dec[..FRAME_SAMPLES], UPLINK_CEILING);
                    let mut raw = [0u8; MAX_PACKET_BYTES];
                    if let Ok(len) = enc.encode(&pcm_dec[..FRAME_SAMPLES], &mut raw) {
                        let mut pkt: OpusPacket = heapless::Vec::new();
                        let timestamp = aec_clock.pop();
                        if pkt.extend_from_slice(&raw[..len]).is_ok()
                            && channels
                                .uplink
                                .try_send(UplinkPacket {
                                    timestamp,
                                    data: pkt,
                                })
                                .is_ok()
                        {
                            preroll_off += n;
                            sent += 1;
                            preroll_frames += 1;
                        } else {
                            break; // 通道满: 下一轮继续
                        }
                    } else {
                        break;
                    }
                }
                if preroll_off >= total {
                    let t = TURN_ID.load(core::sync::atomic::Ordering::Relaxed);
                    crate::diag!(
                        "[T{}] preroll 完成: {} 帧 ({} 样本)",
                        t,
                        preroll_frames,
                        total
                    );
                    preroll_flush = false;
                    preroll_valid = 0;
                    preroll_off = 0;
                    preroll_deadline = 0;
                }
            }
        }

        // ---- 3. 按键: BOOT 对讲 + 音量 ----
        let pressed = buttons.boot_pressed();
        if pressed != held {
            held = pressed;
            // 按键交互: 重置空闲窗口, 退出低功耗
            idle_policy.note_activity(now_ms);
            low_power = false;
            if !audio.pa_enabled() {
                audio.set_pa(true);
            }
            if held && sm.state() == DialogState::Idle {
                // BOOT 建连期间也会丢语音: 只补发按下之后的样本
                preroll_flush = true;
                preroll_off = 0;
                preroll_from = preroll_seq;
                preroll_deadline = now_ms + 1_000;
            }
            if held && sm.state() == DialogState::Speaking {
                // 播报中按键: 立即丢弃未播完的 TTS, 转入 auto 聆听。
                // 先立标志再清通道, 防止会话任务在清理后又塞入旧包。
                DROP_DOWNLINK.store(true, core::sync::atomic::Ordering::Relaxed);
                while channels.downlink.try_receive().is_ok() {}
                playback.reset();
                aec_clock.clear();
                barge_in_active = true;
                dispatch(&mut sm, &mut pending, DialogEvent::BargeIn);
            } else if !held && barge_in_active {
                // 打断后的松键不结束自动聆听
                barge_in_active = false;
            } else {
                dispatch(
                    &mut sm,
                    &mut pending,
                    if held {
                        DialogEvent::ManualListenStart
                    } else {
                        DialogEvent::ManualListenStop
                    },
                );
            }
        }
        let vol_up = buttons.volume_up_pressed();
        let vol_down = buttons.volume_down_pressed();
        let dir: i8 = if vol_up {
            1
        } else if vol_down {
            -1
        } else {
            0
        };
        if dir != 0 && dir != vol_dir {
            // 按下沿: 单击步进 ±10
            vol_dir = dir;
            vol_since_ms = now_ms;
            vol_long = false;
            let cur = audio.volume() as i16;
            let next = (cur + 10 * dir as i16).clamp(0, 100) as u8;
            audio.set_volume(next);
            VOLUME_PCT.store(next, core::sync::atomic::Ordering::Relaxed);
            volume_dirty = true;
            idle_policy.note_activity(now_ms);
            low_power = false;
            if !audio.pa_enabled() {
                audio.set_pa(true);
            }
            notify_text.clear();
            let _ = write!(&mut notify_text, "VOL {}", next);
            notify_until_ms = now_ms + 2_000;
            info!("音量: {}%", next);
        } else if dir != 0 && !vol_long && now_ms - vol_since_ms >= 800 {
            // 长按: 直达最大/静音 (只触发一次, 松开前不再步进)
            vol_long = true;
            let next: u8 = if dir > 0 { 100 } else { 0 };
            audio.set_volume(next);
            VOLUME_PCT.store(next, core::sync::atomic::Ordering::Relaxed);
            volume_dirty = true;
            notify_text.clear();
            let _ = if next == 0 {
                write!(&mut notify_text, "MUTE")
            } else {
                write!(&mut notify_text, "VOL 100")
            };
            notify_until_ms = now_ms + 2_000;
            info!("音量长按: {}%", next);
        } else if dir == 0 {
            vol_dir = 0;
            vol_long = false;
        }

        // ---- 3.5 MCP SetVolume 待办 + 设备状态发布 ----
        if PENDING_VOLUME_VALID.swap(false, core::sync::atomic::Ordering::Relaxed) {
            let v = PENDING_VOLUME.swap(0, core::sync::atomic::Ordering::Relaxed);
            audio.set_volume(v);
            VOLUME_PCT.store(v, core::sync::atomic::Ordering::Relaxed);
            volume_dirty = true;
            idle_policy.note_activity(now_ms);
            low_power = false;
            info!("MCP 音量: {}%", v);
        }
        // 充电状态: GPIO 实时 (不读 ADC, 与 WiFi PHY 无冲突)。
        // Charged 也视为"接外部电源", 与 main 启动路径保持一致。
        CHARGING.store(
            matches!(
                power.status(),
                xiaozhi_core::board::PowerStatus::Charging
                    | xiaozhi_core::board::PowerStatus::Charged
            ),
            core::sync::atomic::Ordering::Relaxed,
        );
        // 电量: 仅 Idle + 10s 限频 (S3 WiFi PHY 与 SARADC 共用 ADC1,
        // 会话期间频繁读 ADC 干扰射频 — 曾崩 pm_on_beacon_rx)
        if sm.state() == DialogState::Idle && now_ms - battery_last_read >= 10_000 {
            battery_last_read = now_ms;
            // ADC 读失败时保留上次值, 不要把电量误置 0
            let pct = battery
                .reported_percentage(power.status())
                .unwrap_or_else(|| BATTERY_PCT.load(core::sync::atomic::Ordering::Relaxed));
            BATTERY_PCT.store(pct, core::sync::atomic::Ordering::Relaxed);
            // 10s 一次诊断 (M8 项 9 验收): 电量 + 充电态, 确认不再崩 pm_on_beacon_rx
            let status = power.status();
            let charging = matches!(status, xiaozhi_core::board::PowerStatus::Charging);
            let on_external = matches!(
                status,
                xiaozhi_core::board::PowerStatus::Charging
                    | xiaozhi_core::board::PowerStatus::Charged
            );
            info!(
                "电量: {}% (充电: {})",
                pct,
                if charging { "是" } else { "否" }
            );
            // 低电提醒 (对齐 C++: <20% 且放电, 仅一次; 回升到 25% 后复位)
            if on_external || pct >= 25 {
                low_batt_latched = false;
            } else if pct > 0 && pct < 20 && !low_batt_latched {
                low_batt_latched = true;
                warn!("低电量: {}% — 播放提示", pct);
                notify_text.clear();
                let _ = write!(&mut notify_text, "LOW BAT {}%", pct);
                notify_until_ms = now_ms + 5_000;
                if !prompt.is_active() {
                    if !audio.pa_enabled() {
                        audio.set_pa(true);
                    }
                    low_power = false;
                    prompt.play(Clip::LowBattery);
                    prompt_until_ms = now_ms + prompt.duration_ms() + 400;
                    // 重新起算空闲窗口: 播完 60s 仍可正常进入低功耗待机
                    idle_policy.note_activity(now_ms);
                }
            }
            // 堆水位诊断 (10s): 只看内部 RAM 分区, 不能把 8MB PSRAM 算进去
            let hs = esp_alloc::HEAP.stats();
            if let Some(r) = hs.region_stats.iter().flatten().find(|r| {
                r.capabilities
                    .contains(esp_alloc::MemoryCapability::Internal)
            }) {
                let used_pct = if r.size > 0 { r.used * 100 / r.size } else { 0 };
                if used_pct > 90 {
                    warn!(
                        "内部堆水位: {}% ({}KB/{}KB) — 接近耗尽",
                        used_pct,
                        r.used / 1024,
                        r.size / 1024
                    );
                }
            }
        }

        // ---- 4. 状态机动作 → 会话命令 (非阻塞) ----
        for (action, mode) in &pending {
            let cmd = match action {
                Action::ConnectSession => {
                    // 等 WiFi 省电切到 None 再建连 (UDP 音频对 modem sleep 敏感);
                    // 最多等 500ms, 超时照常连接
                    pending_connect = true;
                    connect_deadline = now_ms + 500;
                    turn_up_frames = 0;
                    turn_rms_max = 0;
                    preroll_frames = 0;
                    let id = TURN_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
                    crate::diag!("[T{}] connect 请求 t={}", id, now_ms);
                    // 实际发送由下方 4.5 在 PS 预切换完成后统一处理
                    continue;
                },
                Action::SendListenStart => SessionCommand::SendListenStart(*mode),
                Action::SendListenStop => SessionCommand::SendListenStop,
                Action::SendTtsAbort => SessionCommand::SendTtsAbort,
                _ => continue,
            };
            if channels.command.try_send(cmd).is_err() {
                warn!("命令通道满, 丢弃 {:?} (罕见)", cmd);
            }
        }
        pending.clear();

        // WiFi 省电策略: 默认常开 `None`。
        // ROADMAP 记录自适应切换 (空闲 Minimum / 会话 None) 会因切换瞬间
        // 丢 1~3 包导致长句断序, 已在真机回退; 用 `ps-adaptive` 重新做 A/B,
        // 未复测前不作为默认。
        #[cfg(feature = "ps-adaptive")]
        let session_active = sm.state() != DialogState::Idle;
        #[cfg(not(feature = "ps-adaptive"))]
        let session_active = true;
        crate::wifi::WIFI_AUDIO_ACTIVE.store(session_active, core::sync::atomic::Ordering::Relaxed);
        if pending_connect
            && (crate::wifi::WIFI_PS_READY.load(core::sync::atomic::Ordering::Relaxed)
                || now_ms >= connect_deadline)
        {
            pending_connect = false;
            if channels.command.try_send(SessionCommand::Connect).is_err() {
                warn!("命令通道满, 丢弃建连请求");
            }
        }

        // ---- 4.5 空闲提示: 离开 Idle 视为活动; 60s 无活动播提示音 ----
        if sm.state() != DialogState::Idle {
            idle_policy.note_activity(now_ms);
            low_power = false;
            if !audio.pa_enabled() {
                audio.set_pa(true);
            }
            if prompt.is_active() && !prompt.is_wake_ack() {
                // 空闲提示音期间被唤醒: 立即中止 (唤醒应答会另行播放)
                prompt.stop();
                prompt_vol_restore_at = now_ms + 300;
            }
        }
        // 音量恢复 (提示音/唤醒应答播放前可能压到 60%), 与状态无关
        if prompt_vol_restore_at != 0 && now_ms >= prompt_vol_restore_at {
            audio.set_volume(VOLUME_PCT.load(core::sync::atomic::Ordering::Relaxed));
            prompt_vol_restore_at = 0;
        }
        if sm.state() == DialogState::Idle {
            if !prompt.is_active() && idle_policy.poll(now_ms) {
                let vol = VOLUME_PCT
                    .load(core::sync::atomic::Ordering::Relaxed)
                    .min(60);
                audio.set_volume(vol);
                prompt.play_idle();
                prompt_until_ms = now_ms + prompt.duration_ms() + 400;
                info!(
                    "空闲 {}s: 播放提示音 (音量 {}%)",
                    idle_policy.idle_for_ms(now_ms) / 1000,
                    vol
                );
            }
        }

        // ---- 5. 等回复超时: 20s 无服务器消息恢复待机 ----
        // 状态可能在本轮 step 1/3/4 已经变化 (例如长按 BOOT 后松开):
        // 先同步进入时刻, 否则会拿上一状态的时刻误判超时 (step 9 才更新)。
        // 注意不要提前改 `last_state`: 它还负责触发 step 9 的显示刷新。
        if last_state != Some(sm.state()) {
            state_since_ms = now_ms;
        }
        if sm.state() == DialogState::WaitingReply && now_ms - state_since_ms > 20_000 {
            warn!("等回复超时 (20s), 结束会话");
            // 通知服务器释放通道 (MQTT 网关 bridge 会一直挂着, 下次 hello
            // 触发旧 bridge 关闭并向设备发过期 goodbye)
            if channels
                .command
                .try_send(SessionCommand::CloseSession)
                .is_err()
            {
                warn!("命令通道满, 放弃 close 通知");
            }
            dispatch(&mut sm, &mut pending, DialogEvent::SessionClosed);
        }

        // ---- 6. 采集排空 (每轮必做, 时钟常开期间防饱和) ----
        // auto 回合检测: 进入 Listening 时重置
        let is_listening = sm.state() == DialogState::Listening;
        if is_listening && !was_listening {
            listen_turn.reset();
        }
        was_listening = is_listening;

        let mut triggered = false;
        let avail = match cap.available() {
            Ok(a) => a,
            Err(e) => {
                warn!("RX available 失败: {:?} — 重建采集", e);
                return RunOutcome::CaptureFault;
            },
        };
        if avail >= BLOCK_BYTES {
            // pop 内部重读 available (esp-hal 1.1.2), 传全量缓冲
            // (32736B ≥ max non-Late 28644B) 消除竞态; 长阻塞期
            // 绕回填满时最旧数据自然流失。
            let n = match cap.pop(pop_buf) {
                Ok(n) => n,
                Err(e) => {
                    warn!("RX pop 失败: {:?} — 重建采集", e);
                    return RunOutcome::CaptureFault;
                },
            };
            let ns = extract_slot(&pop_buf[..(n / 4) * 4], 2, 0, mono_buf);
            match sm.state() {
                // VAD 必须在原始电平域 (硬件增益后、无软件增益, 对齐 C++ 参考):
                // 若在软件增益后跑, 阈值等效原始值被放大, 环境噪声即误触发
                DialogState::Idle => {
                    preroll_push(
                        preroll,
                        &mut preroll_w,
                        &mut preroll_valid,
                        &mut preroll_seq,
                        &mono_buf[..ns],
                    );
                    // 1Hz 电平诊断 (原始域): 阈值调参/假唤醒排查
                    // peak = 自上次打印以来的最大子块 rms
                    //
                    // 按 60ms 子块处理: pop 返回的是 64~512ms 的变长块,
                    // 整块算 RMS 会稀释短语音, 整块只算一帧也会让 4 帧去抖
                    // 变成 0.25~2s。固定子块后 VAD/去抖/噪声底才稳定。
                    let mut mean = 0i32;
                    let mut rms = 0i32;
                    let mut wake_threshold = adaptive_threshold(noise_ema, VAD_THRESHOLD, 650);
                    for chunk in mono_buf[..ns].chunks(FRAME_SAMPLES) {
                        let (m, r) = level_stats(chunk);
                        mean = m;
                        rms = r;
                        idle_peak_rms = idle_peak_rms.max(r);
                        // 环境噪声自适应: 慢速 EMA 估计噪声底, 阈值 = 1.5x + 100,
                        // 限制在 [VAD_THRESHOLD, 650]; 只有低于阈值的块才更新,
                        // 避免说话声自己把阈值抬高。
                        wake_threshold = adaptive_threshold(noise_ema, VAD_THRESHOLD, 650);
                        if r < wake_threshold as i32 {
                            noise_ema = update_noise_floor(noise_ema, r);
                        }
                        wake.source_mut().set_threshold(wake_threshold);
                        // 提示音播放期 + 尾音窗口: 不喂 VAD, 防扬声器自唤醒
                        if now_ms >= prompt_until_ms {
                            triggered |= wake.feed(chunk, now_ms) == WakeEvent::Triggered;
                        }
                    }
                    if now_ms.saturating_sub(last_level_log_ms) >= 1_000 {
                        last_level_log_ms = now_ms;
                        #[cfg(feature = "debug-diag")]
                        {
                            // 只在诊断构建里回读寄存器: release 下这 12 次 I2C
                            // 约 5ms/s, 且结果无人使用。
                            let regs = audio.es7210_debug_regs();
                            let rd = |reg: u8| {
                                regs.iter()
                                    .find(|(r, _)| *r == reg)
                                    .map(|(_, v)| *v)
                                    .unwrap_or(0xFF)
                            };
                            crate::diag!(
                                "待机电平: mean={} rms={} peak={} (阈值 {}) ES7210 01={:02X} 08={:02X} 02={:02X}",
                                mean,
                                rms,
                                idle_peak_rms,
                                wake_threshold,
                                rd(0x01),
                                rd(0x08),
                                rd(0x02)
                            );
                        }
                        #[cfg(not(feature = "debug-diag"))]
                        let _ = (mean, rms, wake_threshold);
                        idle_peak_rms = 0;
                    }
                },
                DialogState::Listening => {
                    // 回合结束判定 (auto 模式): 本地能量 VAD 找静音收尾点。
                    // ⚠️ 不做噪声门: 原始人声 rms 仅 200~400, 与噪声 (30~90) 差距小,
                    // 门限稍高就会把说话帧也清零 (真机实测: 服务端只收到 0.6s 零值)。
                    let frame_ms = ns as u64 * 1000 / SAMPLE_RATE as u64;
                    let warmup = listen_turn.total_ms() <= LISTEN_WARMUP_MS;
                    // 按 60ms 子块做 VAD: pop 块可能长达 ~448ms, 整块判定会
                    // 让短语音 (<400ms) 永远累计不够、噪声块却能一票置 heard
                    let mut voiced_ms = 0u64;
                    if !warmup {
                        for chunk in mono_buf[..ns].chunks(FRAME_SAMPLES) {
                            if is_voice_active(chunk, LISTEN_VAD_THRESHOLD) {
                                voiced_ms += chunk.len() as u64 * 1000 / SAMPLE_RATE as u64;
                            }
                        }
                    }
                    let in_grace = now_ms < listen_grace_until_ms;
                    let voiced = voiced_ms > 0 && now_ms >= mic_mute_until_ms && !in_grace;
                    // 一次性原始 I2S 转储 (排查字节序/槽位): 首个有声帧的前 512 帧
                    if !raw_dumped && voiced {
                        raw_dumped = true;
                        let frames = (n / 4).min(512);
                        for k in 0..frames {
                            let o = k * 4;
                            let l = i16::from_le_bytes([pop_buf[o], pop_buf[o + 1]]);
                            let r = i16::from_le_bytes([pop_buf[o + 2], pop_buf[o + 3]]);
                            crate::diag!("RAW {} L={} R={}", k, l, r);
                        }
                    }
                    listen_turn.feed(voiced_ms, frame_ms, now_ms < mic_mute_until_ms, in_grace);
                    apply_gain_percent(&mut mono_buf[..ns], GAIN_PERCENT);
                    let limited = limit_peak(&mut mono_buf[..ns], UPLINK_CEILING);
                    // 1Hz 上行电平诊断 (增益后): 调增益/查削顶
                    let (lmean, lrms) = level_stats(&mono_buf[..ns]);
                    listen_peak_rms = listen_peak_rms.max(lrms);
                    if now_ms.saturating_sub(last_level_log_ms) >= 1_000 {
                        last_level_log_ms = now_ms;
                        crate::diag!(
                            "聆听电平(增益后): mean={} rms={} peak={} voiced={} lim={}",
                            lmean,
                            lrms,
                            listen_peak_rms,
                            voiced,
                            limited
                        );
                        listen_peak_rms = 0;
                    }
                    if preroll_flush || now_ms < mic_mute_until_ms {
                        // 等预录补发完成 / 提示音静音窗内: 不编码实时音频
                    } else if let Some(encoder) = encoder.as_mut() {
                        let sent = encode_uplink(
                            &mono_buf[..ns],
                            acc_buf,
                            &mut acc_len,
                            encoder,
                            channels,
                            &mut aec_clock,
                            &mut uplink_frames,
                        );
                        turn_up_frames += sent as u32;
                        if sent > 0 {
                            turn_rms_max = turn_rms_max.max(lrms);
                        }
                    }
                },
                DialogState::Connecting => {
                    // 建连期间继续预录, 避免唤醒后第一句被吞
                    preroll_push(
                        preroll,
                        &mut preroll_w,
                        &mut preroll_valid,
                        &mut preroll_seq,
                        &mono_buf[..ns],
                    );
                },
                // 服务端 AEC: 播报中继续上行麦克风 (回声由服务器消除),
                // 服务器 VAD 检出人声即发 tts stop → 打断并转聆听。
                // 无 AEC 时不能开: 设备会拾取自己的播报形成自问自答。
                DialogState::Speaking if server_aec => {
                    if let Some(encoder) = encoder.as_mut() {
                        apply_gain_percent(&mut mono_buf[..ns], GAIN_PERCENT);
                        limit_peak(&mut mono_buf[..ns], UPLINK_CEILING);
                        let sent = encode_uplink(
                            &mono_buf[..ns],
                            acc_buf,
                            &mut acc_len,
                            encoder,
                            channels,
                            &mut aec_clock,
                            &mut uplink_frames,
                        );
                        turn_up_frames += sent as u32;
                    }
                },
                _ => {}, // WaitingReply: 排空即弃
            }
        }
        if triggered {
            let t = TURN_ID.load(core::sync::atomic::Ordering::Relaxed);
            crate::diag!("[T{}] wake t={}", t, now_ms);
            // 本地应答「我在」: 应答期间不上行、不跑 VAD, 播完再补发之后的预录
            // (避免把应答本身发给服务器, 也避免服务器听到唤醒词提前收尾)
            if !audio.pa_enabled() {
                audio.set_pa(true);
            }
            let vol = VOLUME_PCT
                .load(core::sync::atomic::Ordering::Relaxed)
                .min(60);
            audio.set_volume(vol);
            prompt.play_wake_ack();
            let ack_end = now_ms + prompt.duration_ms() + 250;
            mic_mute_until_ms = ack_end;
            prompt_vol_restore_at = ack_end;
            ack_pending = true;
            ack_flush_at = ack_end;
            info!("唤醒应答: 我在 ({}ms)", prompt.duration_ms());
            dispatch(&mut sm, &mut pending, DialogEvent::WakeTriggered);
        }

        // 应答结束: 从此刻开始补发预录 (丢弃唤醒词与应答音)
        if ack_pending && now_ms >= ack_flush_at {
            ack_pending = false;
            preroll_flush = true;
            preroll_off = 0;
            preroll_from = preroll_seq;
            preroll_deadline = now_ms + 1_000;
        }

        // auto 模式本地收尾: 说过话且静音 1500ms, 或 12s 无人声 → listen stop
        if sm.state() == DialogState::Listening && sm.mode() == ListenMode::Auto {
            if listen_turn.should_stop() {
                crate::diag!(
                    "[T{}] stop: heard={} 有声={}ms 静音={}ms 总时长={}ms 上行帧={} rms_max={}",
                    TURN_ID.load(core::sync::atomic::Ordering::Relaxed),
                    listen_turn.heard(),
                    listen_turn.voiced_ms(),
                    listen_turn.silence_ms(),
                    listen_turn.total_ms(),
                    turn_up_frames,
                    turn_rms_max
                );
                dispatch(&mut sm, &mut pending, DialogEvent::AutoListenStop);
            }
        }

        // ---- 7. 下行播放: opus → 解码 → 立体声 (每轮 ≤3 帧 ≈180ms,
        // 防止连播期间 RX 积压超过排空缓冲) ----
        // 队列空间不足时停止取包: 包留在通道里 → 会话任务阻塞 → TCP 反压服务器,
        // 而不是丢帧 (丢帧 = 漏字)。
        for _ in 0..3 {
            if !playback.has_space() {
                break;
            }
            let pkt = match channels.downlink.try_receive() {
                Ok(p) => p,
                Err(_) => break,
            };
            let Some(decoder) = decoder.as_mut() else {
                continue;
            };
            // 丢包隐藏: 对缺口补 1~4 帧 PLC, 避免解码断点产生爆音
            if pkt.data.is_empty() {
                for _ in 0..pkt.lost.min(4) {
                    // 外层只按单帧预检过一次; PLC 最多补 4 帧, 必须逐帧再检
                    if !playback.has_space() {
                        break;
                    }
                    match decoder.decode(&[], &mut pcm_dec) {
                        Ok(n) if n > 0 => {
                            let n = n.min(pcm_dec.len());
                            let written = mono_to_stereo(&pcm_dec[..n], &mut stereo_out);
                            playback.push(&stereo_out[..written], 0);
                        },
                        _ => break,
                    }
                }
                continue;
            }
            match decoder.decode(&pkt.data, &mut pcm_dec) {
                Ok(n) if n > 0 => {
                    let n = n.min(pcm_dec.len());
                    dec_pkts += 1;
                    dec_samples += n as u64;
                    if dec_pkts.is_multiple_of(20) {
                        crate::diag!(
                            "下行解码: {} 包 {} 样本 (≈{}ms)",
                            dec_pkts,
                            dec_samples,
                            dec_samples * 1000 / SAMPLE_RATE as u64
                        );
                    }
                    let written = mono_to_stereo(&pcm_dec[..n], &mut stereo_out);
                    // 时间戳随 PCM 入队, 真正播出时由 pump 交给 AEC 时钟
                    playback.push(&stereo_out[..written], pkt.timestamp);
                },
                Ok(_) => {},
                Err(e) => warn!("opus 解码失败: {}", e),
            }
        }

        // ---- 8. 时钟维持: 软件队列 → DMA 环形, 没数据时补静音 ----
        // 空闲提示音优先占用输出; 播完后恢复常规泵送 (补静音)
        if prompt.is_active() && prompt.pump(audio, &mut stereo_out[..]) {
            match prompt.kind() {
                Clip::WakeAck => {
                    // 唤醒应答播完: 恢复用户音量 (预录补发已由 ack_flush_at 触发)
                    prompt_vol_restore_at = now_ms + 200;
                },
                Clip::IdlePrompt => {
                    low_power = true;
                    prompt_vol_restore_at = now_ms + 300;
                    // 功放断电: 消掉空闲期的功放底噪/RF 串扰, 同时省电
                    audio.set_pa(false);
                    info!("提示音播放完成, 进入低功耗待机 (功放关闭)");
                },
                // 低电/错误/alert/成功音: 播完恢复音量; 若仍在待机则回到低功耗
                _ => {
                    prompt_vol_restore_at = now_ms + 200;
                    if sm.state() == DialogState::Idle {
                        low_power = true;
                        audio.set_pa(false);
                    }
                },
            }
        }
        if !prompt.is_active() {
            let (underran, _) = playback.pump(
                audio,
                silence_buf,
                sm.state() == DialogState::Speaking,
                &mut aec_clock,
            );
            if underran && playback.underruns() % 100 == 1 {
                info!("播报欠载 #{} (PCM 队列空, 补静音)", playback.underruns());
            }
        }
        if playback.take_prebuffer_done() {
            crate::diag!("预缓冲完成: {}B", playback.queue_len());
        }
        if sm.state() == DialogState::Speaking && now_ms.saturating_sub(tx_rate_ms) >= 2_000 {
            let tx_bytes = playback.tx_bytes();
            crate::diag!(
                "播报队列: PCM {}KB, 下行 {}, 欠载 {}, TX {}B/s",
                playback.queue_len() / 1024,
                channels.downlink.len(),
                playback.underruns(),
                (tx_bytes - tx_rate_bytes) * 1000 / now_ms.saturating_sub(tx_rate_ms).max(1),
            );
            tx_rate_ms = now_ms;
            tx_rate_bytes = tx_bytes;
        }

        // TtsStop 延迟派发: 队列空 + DMA 播完后才回到 Listening (防回声)
        if playback.poll_stop(now_ms) {
            wake.policy_mut().note_playback_end(now_ms);
            // 软件队列已空, 但 TX DMA 环里可能还有 ~128ms 尾音:
            // 留 500ms 本地静默期, 避免一进入 Listening 就录到自己的尾音。
            listen_grace_until_ms = now_ms + 500;
            // 尾音同样禁止上行: 否则服务端 ASR 会把扬声器尾音/混响识别成
            // 用户说话, 回一句「我听到啦」形成自问自答 (真机日志可见)。
            mic_mute_until_ms = mic_mute_until_ms.max(now_ms + 600);
            // 播报期间积压的上行帧里全是扬声器回声, 不能等会话任务慢慢发完:
            // 全部丢弃, 否则 tts stop 之后服务器还会收到 1~2s 的“用户说话”。
            while channels.uplink.try_receive().is_ok() {}
            acc_len = 0;
            dispatch(&mut sm, &mut pending, DialogEvent::TtsStop);
        }
        // 让出 CPU: 会话任务/网络任务需要被轮询才能推进连接与收发。
        // 活跃态 4ms (原 1ms): 下行解码 3 帧/轮仍 >45x 实时, RX 环 512ms,
        // 足以覆盖; 唤醒次数降 4 倍。Idle 保持 10ms。
        let wait_ms = if sm.state() == DialogState::Idle {
            10
        } else {
            4
        };
        embassy_time::Timer::after(embassy_time::Duration::from_millis(wait_ms)).await;

        // ---- 9. 灯语 + 显示状态 ----
        let led = match sm.state() {
            DialogState::Idle if low_power => LedMode::Off,
            DialogState::Idle => LedMode::Breathing(BreathSpeed::Slow),
            DialogState::Connecting => LedMode::Breathing(BreathSpeed::Fast),
            DialogState::Listening => LedMode::Breathing(BreathSpeed::Medium),
            DialogState::WaitingReply => LedMode::Breathing(BreathSpeed::Fast),
            DialogState::Speaking => LedMode::On,
        };
        if last_led != Some(led) {
            leds.set_mode(led);
            last_led = Some(led);
        }
        // 音量落盘: 仅在 Idle 稳定 1s 后写, 避免 Listening/Speaking 期擦写 Flash
        if volume_dirty
            && sm.state() == DialogState::Idle
            && now_ms.saturating_sub(state_since_ms) >= 1_000
        {
            let v = VOLUME_PCT.load(core::sync::atomic::Ordering::Relaxed);
            crate::wifi::store::save_volume(flash, v);
            volume_dirty = false;
        }
        leds.tick();
        if last_state != Some(sm.state()) {
            last_state = Some(sm.state());
            state_since_ms = now_ms;
            display.draw_status(
                state_text(sm.state()),
                match sm.state() {
                    DialogState::Speaking => Rgb565::YELLOW,
                    DialogState::Listening => Rgb565::GREEN,
                    _ => Rgb565::CYAN,
                },
            );
        }
        // 通知行 (音量/低电/错误/alert): 显示到超时, 文本变化时重绘
        if now_ms < notify_until_ms {
            if notify_shown.as_deref() != Some(notify_text.as_str()) {
                display.draw_notification(notify_text.as_str(), Rgb565::YELLOW);
                notify_shown = Some(notify_text.clone());
            }
        } else if notify_shown.is_some() {
            display.clear_notification();
            notify_shown = None;
        }
        // 文本缓存供下一批屏幕 UI 渲染 (当前 3x5 字库无中文, 暂不显示)
        let _ = (&last_user_text, &last_assistant_text);
    }
}

/// 显示面板状态文本 (3x5 点阵字库仅 ASCII)
/// 16-bit 单声道 → 立体声 (左右复制) 交错 little-endian 字节; 返回写入字节数
/// 把软件 PCM 队列泵入 I2S DMA 环形, 环形空闲部分用静音填满
///
/// 关键: DMA 环形永远保持满载。有真实音频就写真实音频, 没有就写静音,
/// 杜绝环形欠载时 DMA 绕回重放旧数据 (听感为逐字重复/杂音)。
/// 返回是否发生「播报中队列空、只能补静音」的欠载
/// PSRAM 分配唤醒预录环 (失败回退内部 RAM)
pub fn alloc_preroll() -> &'static mut [i16] {
    crate::mem::psram_i16(PREROLL_SAMPLES)
}

/// 追加原始 PCM 到预录环 (保留最近 len 个样本)
fn preroll_push(buf: &mut [i16], w: &mut usize, valid: &mut usize, seq: &mut u64, samples: &[i16]) {
    for &s in samples {
        buf[*w] = s;
        *seq += 1;
        *w = (*w + 1) % buf.len();
        if *valid < buf.len() {
            *valid += 1;
        }
    }
}

fn mono_to_stereo(src: &[i16], dst: &mut [u8]) -> usize {
    let mut o = 0;
    for &s in src {
        let le = s.to_le_bytes();
        dst[o..o + 2].copy_from_slice(&le);
        dst[o + 2..o + 4].copy_from_slice(&le);
        o += 4;
    }
    o
}

fn state_text(state: DialogState) -> &'static str {
    match state {
        DialogState::Idle => "IDLE",
        DialogState::Connecting => "CONN",
        DialogState::Listening => "LISTEN",
        DialogState::WaitingReply => "WAIT",
        DialogState::Speaking => "TTS",
    }
}

/// 事件注入: 状态转移 + 动作暂存 (Connect 类动作由状态即触发器承担)
fn dispatch(
    sm: &mut DialogSm,
    pending: &mut heapless::Vec<(Action, ListenMode), 8>,
    ev: DialogEvent,
) {
    let (state, actions_ref) = sm.transition(ev);
    // Action 是 Copy: 先复制释放 sm 的 &mut 借用, 再读 post-transition mode
    // (barge-in 修复: Speaking+ManualListenStart 转移后 mode 变 Manual,
    //  pre-transition 读会错标为 Auto)
    let actions: heapless::Vec<Action, 8> = actions_ref.iter().copied().collect();
    let mode = sm.mode();
    for &a in &actions {
        match a {
            Action::PlayWakeChime => {}, // 提示音在唤醒注入点同步执行
            _ => {
                let _ = pending.push((a, mode));
            },
        }
    }
    // 打印触发事件, 便于确认唤醒来源 (VAD 唤醒 vs 按键 vs 会话生命周期)
    info!("状态: {} (事件: {})", state_label(state), event_label(ev));
}

fn state_label(state: DialogState) -> &'static str {
    match state {
        DialogState::Idle => "待机",
        DialogState::Connecting => "建连",
        DialogState::Listening => "聆听",
        DialogState::WaitingReply => "等回复",
        DialogState::Speaking => "播报",
    }
}

fn event_label(ev: DialogEvent) -> &'static str {
    match ev {
        DialogEvent::WakeTriggered => "唤醒触发",
        DialogEvent::ManualListenStart => "按住BOOT",
        DialogEvent::ManualListenStop => "松开BOOT",
        DialogEvent::AutoListenStop => "本地静音收尾",
        DialogEvent::BargeIn => "按键打断",
        DialogEvent::SessionReady => "会话就绪",
        DialogEvent::TtsStart => "播报开始",
        DialogEvent::TtsStop => "播报结束",
        DialogEvent::SessionClosed => "会话关闭",
    }
}

/// MCP 请求应答 (会话任务内; 设备状态经原子量读取, set_volume 写待办)
///
/// 由两种传输在收到服务器 `type=mcp` 文本帧时调用, 回送
/// initialize/tools_list/tools_call 应答 (见 session.rs HELLO 的 `features.mcp`)。
pub(crate) fn mcp_handle(text: &str, volume: u8) -> Option<heapless::String<1024>> {
    use xiaozhi_core::mcp;
    let Some(req) = mcp::parse_request(text) else {
        return None; // notifications / 无法解析: 无需回应
    };
    const BOARD_NAME: &str = crate::bsp::active::BOARD.name;
    const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

    Some(match req.method {
        mcp::McpMethod::Initialize => mcp::initialize_response(req.id, BOARD_NAME, APP_VERSION),
        mcp::McpMethod::ToolsList => mcp::tools_list_response(req.id),
        mcp::McpMethod::ToolsCall { name, volume: vol } => match (name, vol) {
            ("self.get_device_status", _) => {
                let level = BATTERY_PCT.load(core::sync::atomic::Ordering::Relaxed);
                let charging = CHARGING.load(core::sync::atomic::Ordering::Relaxed);
                let status = mcp::status_json(volume, Some(level), charging);
                mcp::text_result_response(req.id, status.as_str())
            },
            ("self.audio_speaker.set_volume", Some(v)) => {
                let v = v.clamp(0, 100) as u8;
                // 直接投递到音频侧待办: WS/MQTT 两种传输共用同一条副作用路径
                VOLUME_PCT.store(v, core::sync::atomic::Ordering::Relaxed);
                PENDING_VOLUME.store(v, core::sync::atomic::Ordering::Relaxed);
                PENDING_VOLUME_VALID.store(true, core::sync::atomic::Ordering::Relaxed);
                mcp::text_result_response(req.id, "true")
            },
            ("self.reboot", _) => {
                MCP_REBOOT.store(true, core::sync::atomic::Ordering::Relaxed);
                mcp::text_result_response(req.id, "rebooting")
            },
            ("self.get_system_info", _) => {
                let level = BATTERY_PCT.load(core::sync::atomic::Ordering::Relaxed);
                let charging = CHARGING.load(core::sync::atomic::Ordering::Relaxed);
                let uptime = esp_hal::time::Instant::now()
                    .duration_since_epoch()
                    .as_millis();
                let info = mcp::system_info_json(APP_VERSION, uptime, volume, level, charging);
                mcp::text_result_response(req.id, info.as_str())
            },
            ("self.audio_speaker.set_volume", None) => {
                mcp::error_response(req.id, "Missing argument: volume")
            },
            _ => mcp::error_response(req.id, "Unknown tool"),
        },
        mcp::McpMethod::Unknown(_) => mcp::error_response(req.id, "Method not implemented"),
    })
}

/// MCP `self.reboot`: 应答成功发出后延迟复位 (WS / MQTT 共用)
///
/// `mcp_handle` 只置位; 调用方必须在本条应答真正写入 socket 后调用本函数,
/// 发送失败时应清掉标志, 避免下一条无关应答误触发重启。
pub async fn mcp_maybe_reboot() {
    if MCP_REBOOT.swap(false, core::sync::atomic::Ordering::Relaxed) {
        info!("MCP: 应答已发送, 200ms 后重启");
        embassy_time::Timer::after(embassy_time::Duration::from_millis(200)).await;
        esp_hal::system::software_reset();
    }
}
