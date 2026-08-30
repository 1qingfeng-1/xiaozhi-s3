//! 智能音箱主应用 — 对话状态机驱动的编排层 (双任务架构)
//!
//! - 音频主循环 ([`run`]): 采集排空/唤醒 VAD/编码/解码/播放/灯效/显示,
//!   每轮 ≤64ms; 所有网络交互经 embassy-sync 通道非阻塞交换
//!   (单循环长阻塞 await 会让 I2S RX 饱和触发 Late, 见真机回归记录)
//! - 会话任务 ([`session_task`]): WS 连接/握手/收发, 长阻塞 await 集中于此
//!
//! 状态转移委托 [`xiaozhi_core::dialog::DialogSm`] (host 单测覆盖)。

use embassy_net::{Ipv4Address, Stack};
use embassy_sync::channel::Channel;

use esp_hal::{Blocking, dma::DmaTransferRxCircular, i2s::master::I2sRx};

use defmt::{error, info, warn};

use xiaozhi_core::audio::capture::{apply_gain_percent, extract_slot};
use xiaozhi_core::audio::wake::{
    VadWakeDetector, WakeEngine, WakeEvent, WakePolicy, WakePolicyConfig,
};
use xiaozhi_core::dialog::{Action, DialogEvent, DialogSm, DialogState, ListenMode};

use crate::audio::opus::{MAX_PACKET_BYTES, OpusDecoder, OpusEncoder};

impl defmt::Format for SessionCommand {
    fn format(&self, f: defmt::Formatter) {
        match self {
            SessionCommand::Connect => defmt::write!(f, "Connect"),
            SessionCommand::SendListenStart(_) => defmt::write!(f, "ListenStart"),
            SessionCommand::SendListenStop => defmt::write!(f, "ListenStop"),
            SessionCommand::SendTtsAbort => defmt::write!(f, "TtsAbort"),
            SessionCommand::SetVolume(_) => defmt::write!(f, "SetVolume"),
        }
    }
}
use crate::bsp::components::{
    Audio, Battery, BreathSpeed, Buttons, Display, LedMode, Leds, Power, Rgb565,
};
use crate::session;

/// 上行音频参数 (16kHz 单声道; 帧长 20ms — SILK 编码的栈上 VLA 随帧长
/// 线性增长, 60ms 帧需 ~87KB 任务栈必然溢出, 20ms 实测可控; 服务器按
/// hello 宣告的 frame_duration 适配上行)
const SAMPLE_RATE: u32 = 16_000;
const FRAME_MS: u32 = 20;
const FRAME_SAMPLES: usize = SAMPLE_RATE as usize / 1000 * FRAME_MS as usize; // 960
/// 软件增益百分比: 麦克风数字电平 1~5% 满幅, +18dB 后 ASR 可用
const GAIN_PERCENT: u16 = 800;
/// 唤醒 VAD 阈值 (均方幅度, 原始采集电平域; 环境噪声几十 / 正常说话 >1k)
pub const VAD_THRESHOLD: i16 = 800;

/// 乒乓窗口 = 单个 DMA 描述符 (4092B ≈ 64ms): available() 按完整描述符计数
const BLOCK_BYTES: usize = 4092;
/// 采集排空缓冲 = 整条 RX 链 32736B (pop 全量语义, 缓冲必须 ≥ accumulated)
const POP_BUF_SIZE: usize = 4092 * 8;
/// 帧累积缓冲: 2 帧余量
const ACC_SAMPLES: usize = FRAME_SAMPLES * 2;

/// opus 包通道项 (单包上限 1276B)
pub type OpusPacket = heapless::Vec<u8, MAX_PACKET_BYTES>;

/// 跨任务共享的设备状态 (MCP get_device_status / set_volume 用)
pub static VOLUME_PCT: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(60);
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
    /// MCP: 设置音量 (音频任务执行 I2C 写)
    SetVolume(u8),
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
    /// 会话关闭
    Closed,
}

/// 任务间通道 (main 装配一次, 静态常驻)
pub struct Channels {
    /// 音频 → 会话: 编码后的 opus 帧
    pub uplink: Channel<RawMutex, OpusPacket, 12>,
    /// 会话 → 音频: 下行 opus 帧 (TTS)
    pub downlink: Channel<RawMutex, OpusPacket, 6>,
    /// 音频 → 会话: 命令
    pub command: Channel<RawMutex, SessionCommand, 4>,
    /// 会话 → 音频: 事件
    pub event: Channel<RawMutex, SessionEvent, 4>,
}

/// 会话任务依赖 ('static: embassy 任务要求参数永久存活)
pub struct SessionArgs {
    pub stack: Stack<'static>,
    pub server: Ipv4Address,
    pub mac: heapless::String<32>,
    pub channels: &'static Channels,
}

/// 会话任务: 连接/握手/收发 (所有长阻塞 await 集中在此, 不影响音频主循环)
#[embassy_executor::task]
pub async fn session_task(args: SessionArgs) {
    let SessionArgs {
        stack,
        server,
        mac,
        channels,
    } = args;

    // socket 持有于任务内, 跨重连复用 (每次 connect 泄漏缓冲会耗尽内部堆)
    let rx = alloc::vec![0u8; 4096].leak();
    let tx = alloc::vec![0u8; 4096].leak();
    let mut socket = embassy_net::tcp::TcpSocket::new(stack, rx, tx);
    let mut connected = false;
    let mut want_connect = false;

    loop {
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
                            channels.event.try_send(SessionEvent::Closed).ok();
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
                            channels.event.try_send(SessionEvent::Closed).ok();
                        }
                    }
                },
                Ok(SessionCommand::SetVolume(v)) => {
                    // 音频主循环轮询 PENDING_VOLUME 并执行 I2C 写
                    PENDING_VOLUME.store(v, core::sync::atomic::Ordering::Relaxed);
                    PENDING_VOLUME_VALID.store(true, core::sync::atomic::Ordering::Relaxed);
                },
                Ok(SessionCommand::SendTtsAbort) => {
                    if connected {
                        let mut ws = session::WsClient {
                            socket: &mut socket,
                        };
                        if ws.send_text(session::TTS_ABORT.as_bytes()).await.is_err() {
                            connected = false;
                            channels.event.try_send(SessionEvent::Closed).ok();
                        }
                    }
                },
                Err(_) => break,
            }
        }

        // ---- 建连 (阻塞仅影响本任务; 音频主循环照常排空采集) ----
        if want_connect {
            want_connect = false;
            if !connected {
                info!("会话任务: 开始连接...");
                match session::connect(&mut socket, server, &mac).await {
                    Ok((_rate, _frame_ms)) => {
                        connected = true;
                        channels.event.try_send(SessionEvent::Ready).ok();
                    },
                    Err(e) => {
                        warn!("会话连接失败: {}", e);
                        channels.event.try_send(SessionEvent::Closed).ok();
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
                if ws.send_frame(2, &pkt).await.is_err() {
                    connected = false;
                    channels.event.try_send(SessionEvent::Closed).ok();
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
                            match (
                                session::json_str_field(text, "type"),
                                session::json_str_field(text, "state"),
                            ) {
                                (Some("tts"), Some("start")) => {
                                    info!("服务器: tts start");
                                    channels.event.try_send(SessionEvent::TtsStart).ok();
                                },
                                (Some("tts"), Some("stop")) => {
                                    info!("服务器: tts stop");
                                    channels.event.try_send(SessionEvent::TtsStop).ok();
                                },
                                _ => info!("服务器: {}", text),
                            }
                        },
                        2 => {
                            let mut pkt: OpusPacket = heapless::Vec::new();
                            if pkt.extend_from_slice(&payload).is_ok() {
                                if channels.downlink.try_send(pkt).is_err() {
                                    warn!("下行通道满, 丢弃一帧");
                                }
                            }
                        },
                        8 => {
                            info!("服务器关闭连接");
                            connected = false;
                            channels.event.try_send(SessionEvent::Closed).ok();
                        },
                        9 => {
                            let _ = ws.send_pong(&payload).await;
                        },
                        _ => {},
                    },
                    Err(e) => {
                        warn!("WS 接收错误: {} — 会话关闭", e);
                        connected = false;
                        channels.event.try_send(SessionEvent::Closed).ok();
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
}

/// 主事件循环 (音频侧)
///
/// 返回 [`RunOutcome::CaptureFault`] 时调用方需重建采集句柄后重新进入
/// (esp-hal RX 饱和后只能整体重建, 且重建必须在直线路径上做, 见 AGENTS.md)。
#[allow(clippy::too_many_arguments)]
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
    } = args;

    let mut sm = DialogSm::new();
    let mut wake = WakeEngine::new(
        VadWakeDetector::new(VAD_THRESHOLD),
        WakePolicy::new(WakePolicyConfig::default()),
    );
    let mut encoder: Option<OpusEncoder> = None;
    let mut decoder: Option<OpusDecoder> = None;
    let mut pcm_dec: alloc::boxed::Box<[i16]> = alloc::vec![0i16; FRAME_SAMPLES].into_boxed_slice();
    let mut stereo_out: alloc::boxed::Box<[u8]> =
        alloc::vec![0u8; FRAME_SAMPLES * 4].into_boxed_slice();
    let mut acc_len = 0usize;
    let mut held = false;
    let mut vol_edge = false;
    let mut last_led: Option<LedMode> = None;
    let mut last_state: Option<DialogState> = None;
    let mut state_since_ms = 0u64;
    let mut uplink_frames: u64 = 0;
    let mut pending: heapless::Vec<(Action, ListenMode), 8> = heapless::Vec::new();

    info!("=== 主循环启动: 待机监听 (说话唤醒 / 按住 BOOT 对讲) ===");

    loop {
        let now_ms = esp_hal::time::Instant::now()
            .duration_since_epoch()
            .as_millis();
        let mut played = false;

        // ---- 1. 会话事件 ----
        loop {
            match channels.event.try_receive() {
                Ok(SessionEvent::Ready) => {
                    dispatch(&mut sm, &mut pending, DialogEvent::SessionReady)
                },
                Ok(SessionEvent::TtsStart) => {
                    dispatch(&mut sm, &mut pending, DialogEvent::TtsStart)
                },
                Ok(SessionEvent::TtsStop) => {
                    wake.policy_mut().note_playback_end(now_ms);
                    dispatch(&mut sm, &mut pending, DialogEvent::TtsStop);
                },
                Ok(SessionEvent::Closed) => {
                    dispatch(&mut sm, &mut pending, DialogEvent::SessionClosed)
                },
                Err(_) => break,
            }
        }

        // ---- 2. 会话拆除 / 编解码器生命周期 ----
        if sm.state() == DialogState::Idle {
            if encoder.is_some() {
                wake.policy_mut().note_playback_end(now_ms);
                encoder = None;
                decoder = None;
                info!("会话已结束, 回到待机监听");
            }
        } else if encoder.is_none() && sm.state() == DialogState::Listening {
            match (
                OpusEncoder::new_voip_mono(SAMPLE_RATE, FRAME_MS),
                OpusDecoder::new_mono(SAMPLE_RATE),
            ) {
                (Ok(enc), Ok(dec)) => {
                    encoder = Some(enc);
                    decoder = Some(dec);
                },
                _ => error!("opus 编解码器创建失败"),
            }
        }

        // ---- 3. 按键: BOOT 对讲 + 音量 ----
        let pressed = buttons.boot_pressed();
        if pressed != held {
            held = pressed;
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
        let vol_up = buttons.volume_up_pressed();
        let vol_down = buttons.volume_down_pressed();
        if (vol_up || vol_down) && !vol_edge {
            let cur = audio.volume() as i16;
            let next = (cur + if vol_up { 5 } else { -5 }).clamp(0, 100) as u8;
            audio.set_volume(next);
            info!("音量: {}%", next);
        }
        vol_edge = vol_up || vol_down;

        // ---- 3.5 MCP SetVolume 待办 + 设备状态发布 ----
        if PENDING_VOLUME_VALID.swap(false, core::sync::atomic::Ordering::Relaxed) {
            let v = PENDING_VOLUME.swap(0, core::sync::atomic::Ordering::Relaxed);
            audio.set_volume(v);
            VOLUME_PCT.store(v, core::sync::atomic::Ordering::Relaxed);
            info!("MCP 音量: {}%", v);
        }
        BATTERY_PCT.store(
            battery.reported_percentage(power.status()).unwrap_or(0),
            core::sync::atomic::Ordering::Relaxed,
        );
        CHARGING.store(
            matches!(power.status(), xiaozhi_core::board::PowerStatus::Charging),
            core::sync::atomic::Ordering::Relaxed,
        );

        // ---- 4. 状态机动作 → 会话命令 (非阻塞) ----
        for (action, mode) in &pending {
            let cmd = match action {
                Action::ConnectSession => SessionCommand::Connect,
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

        // ---- 5. 等回复超时: 10s 无服务器消息恢复待机 ----
        if last_state != Some(sm.state()) {
            last_state = Some(sm.state());
            state_since_ms = now_ms;
        }
        if sm.state() == DialogState::WaitingReply && now_ms - state_since_ms > 10_000 {
            warn!("等回复超时 (10s), 结束会话");
            dispatch(&mut sm, &mut pending, DialogEvent::SessionClosed);
        }

        // ---- 6. 采集排空 (每轮必做, 时钟常开期间防饱和) ----
        let mut triggered = false;
        match cap.available() {
            Ok(avail) if avail >= BLOCK_BYTES => {
                // pop 是全量语义: 缓冲必须 ≥ accumulated (描述符粒度是 4095
                // 而非 4092, 不能自作对齐)。排空缓冲已扩至整条 RX 链,
                // take = avail 恒可 pop; 长阻塞期绕回填满时最旧数据自然流失。
                let take = usize::min(avail, pop_buf.len());
                match cap.pop(&mut pop_buf[..take]) {
                    Ok(n) => {
                        let ns = extract_slot(&pop_buf[..(n / 4) * 4], 2, 0, mono_buf);
                        apply_gain_percent(&mut mono_buf[..ns], GAIN_PERCENT);
                        match sm.state() {
                            DialogState::Idle => {
                                if wake.feed(&mono_buf[..ns], now_ms) == WakeEvent::Triggered {
                                    triggered = true;
                                }
                            },
                            DialogState::Listening => {
                                if let Some(encoder) = encoder.as_mut() {
                                    let mut src = &mono_buf[..ns];
                                    while !src.is_empty() {
                                        let take = usize::min(acc_buf.len() - acc_len, src.len());
                                        acc_buf[acc_len..acc_len + take]
                                            .copy_from_slice(&src[..take]);
                                        acc_len += take;
                                        src = &src[take..];
                                        while acc_len >= FRAME_SAMPLES {
                                            let mut raw = [0u8; MAX_PACKET_BYTES];
                                            match encoder
                                                .encode(&acc_buf[..FRAME_SAMPLES], &mut raw)
                                            {
                                                Ok(len) => {
                                                    uplink_frames += 1;
                                                    if uplink_frames % 100 == 0 {
                                                        info!("上行心跳: {} 帧", uplink_frames);
                                                    }
                                                    let mut pkt: OpusPacket = heapless::Vec::new();
                                                    if pkt.extend_from_slice(&raw[..len]).is_ok()
                                                        && channels.uplink.try_send(pkt).is_err()
                                                    {
                                                        warn!("上行通道满, 丢弃一帧");
                                                    }
                                                },
                                                Err(e) => warn!("opus 编码失败: {}", e),
                                            }
                                            acc_buf.copy_within(FRAME_SAMPLES..acc_len, 0);
                                            acc_len -= FRAME_SAMPLES;
                                        }
                                    }
                                }
                            },
                            _ => {}, // Speaking/Connecting/WaitingReply: 排空即弃
                        }
                    },
                    Err(e) => {
                        warn!("RX pop 失败: {:?} — 重建采集", e);
                        return RunOutcome::CaptureFault;
                    },
                }
            },
            Ok(_) => {},
            Err(e) => {
                warn!("RX available 失败: {:?} — 重建采集", e);
                return RunOutcome::CaptureFault;
            },
        }
        if triggered {
            audio.beep(120);
            dispatch(&mut sm, &mut pending, DialogEvent::WakeTriggered);
        }

        // ---- 7. 下行播放: opus → 解码 → 立体声 (每轮 ≤3 帧 ≈180ms,
        // 防止连播期间 RX 积压超过排空缓冲) ----
        for _ in 0..3 {
            let pkt = match channels.downlink.try_receive() {
                Ok(p) => p,
                Err(_) => break,
            };
            if let Some(decoder) = decoder.as_mut() {
                match decoder.decode(&pkt, &mut pcm_dec) {
                    Ok(n) if n > 0 => {
                        let mut o = 0;
                        for &s in &pcm_dec[..n] {
                            let le = s.to_le_bytes();
                            stereo_out[o..o + 2].copy_from_slice(&le);
                            stereo_out[o + 2..o + 4].copy_from_slice(&le);
                            o += 4;
                        }
                        audio.play_block(&stereo_out[..n * 4]);
                        played = true;
                    },
                    Ok(_) => {},
                    Err(e) => warn!("opus 解码失败: {}", e),
                }
            }
        }

        // ---- 8. 时钟维持 (全程常开; TTS 轮次由下行音频维持) ----
        if !played {
            audio.play_block(silence_buf);
        }
        // 让出 CPU: 会话任务/网络任务需要被轮询才能推进连接与收发
        embassy_time::Timer::after(embassy_time::Duration::from_millis(1)).await;

        // ---- 9. 灯语 + 显示状态 ----
        let led = match sm.state() {
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
        leds.tick();
        if last_state != Some(sm.state()) {
            last_state = Some(sm.state());
            display.draw_status(
                state_text(sm.state()),
                match sm.state() {
                    DialogState::Speaking => Rgb565::YELLOW,
                    DialogState::Listening => Rgb565::GREEN,
                    _ => Rgb565::CYAN,
                },
            );
        }
    }
}

fn now_label() {}

/// 显示面板状态文本 (3x5 点阵字库仅 ASCII)
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
    let mode = sm.mode();
    let (state, actions) = sm.transition(ev);
    let actions: heapless::Vec<(Action, ListenMode), 8> =
        actions.iter().map(|a| (*a, mode)).collect();
    for (action, mode) in &actions {
        match action {
            Action::PlayWakeChime => {}, // 提示音在唤醒注入点同步执行
            other => {
                let _ = pending.push((*other, *mode));
            },
        }
    }
    info!("状态: {}", state_label(state));
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

/// MCP 请求应答 (会话任务内; 设备状态经原子量读取, set_volume 经命令下发)
fn mcp_handle(text: &str, volume: u8, channels: &Channels) -> Option<heapless::String<1024>> {
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
                VOLUME_PCT.store(v, core::sync::atomic::Ordering::Relaxed);
                if channels
                    .command
                    .try_send(SessionCommand::SetVolume(v))
                    .is_err()
                {
                    warn!("MCP: 命令通道满, 音量设置丢弃");
                }
                mcp::text_result_response(req.id, "true")
            },
            ("self.audio_speaker.set_volume", None) => {
                mcp::error_response(req.id, "Missing argument: volume")
            },
            _ => mcp::error_response(req.id, "Unknown tool"),
        },
        mcp::McpMethod::Unknown(_) => mcp::error_response(req.id, "Method not implemented"),
    })
}

/// set_volume 经命令通道下发音频主循环 (I2C 写在音频任务上下文执行)
fn channels_command_set_volume(v: u8) {
    // 由调用处 (session_task) 的 channels 静态引用发送;
    // 这里经全局静态通道句柄不可行, 故以命令事件转发 — 见 session_task 内
    // SessionCommand::SetVolume 分支。
    let _ = v;
}
