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

use xiaozhi_core::audio::capture::{
    apply_gain_percent, extract_slot, is_voice_active, level_stats, limit_peak,
};
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
    Audio, Battery, BreathSpeed, Buttons, Display, LedMode, Leds, Power, Rgb565, TX_RING_BYTES,
};
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
/// TTS 播报期的 TX 低水位 (20ms): 队列快见底才补静音。
///
/// 播报期每轮都填满会在 TTS 帧之间插静音 (一卡一卡); 完全不补则 DMA 绕环
/// 重放旧数据 (真机 2026-09-26: 提示音/尾音反复 → 「嘟嘟嘟」)。取低水位兼得。
const TTS_TX_LOW_WATER: usize = 16_000 / 1000 * 20 * 4; // 20ms 立体声 16bit
/// 唤醒 VAD 阈值 (均方幅度, 原始采集电平域)
///
/// 真机实测 (2026-09-26): 本机麦克风数字电平偏低 — 环境噪声 rms 15~85,
/// 正常说话峰值仅 200~750 (未达旧阈值 800 → 完全无法唤醒)。取 300:
/// 约 3.5x 噪声余量, 且低于说话峰值; 去抖 2 帧 (128ms) 滤瞬态。
/// audio-loopback 亦取 300。
pub const VAD_THRESHOLD: i16 = 300;
/// 回合结束用 VAD 阈值: 比唤醒阈值低 (原始人声多数帧 rms 200~400);
/// 误判 "说过话" 只影响收尾时机, 不会误唤醒, 故取低值保证不丢话。
const LISTEN_VAD_THRESHOLD: i16 = 180;

/// 乒乓窗口 = 单个 DMA 描述符 (4092B ≈ 64ms): available() 按完整描述符计数
const BLOCK_BYTES: usize = 4092;

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
            if connected {
                // 已连接: 补发就绪。上一轮"按下 BOOT→建连中松开→回 Idle"时,
                // 迟到的 Ready 被状态机忽略, 但会话仍活着; 若不补发,
                // 下次按键会一直停在 Connecting (永远等不到就绪) — 真机复现。
                channels.event.try_send(SessionEvent::Ready).ok();
            } else {
                info!("会话任务: 开始连接...");
                let started = esp_hal::time::Instant::now().duration_since_epoch();
                match embassy_time::with_timeout(
                    embassy_time::Duration::from_secs(8),
                    session::connect(&mut socket, server, &mac),
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
                        channels.event.try_send(SessionEvent::Ready).ok();
                    },
                    Ok(Err(e)) => {
                        warn!("会话连接失败: {}", e);
                        channels.event.try_send(SessionEvent::Closed).ok();
                    },
                    Err(_) => {
                        warn!("会话连接超时 (8s) — 中止");
                        socket.abort();
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
                                _ => {
                                    // MCP 帧 (type=mcp): 解析并回送 initialize/tools_list/tools_call 应答
                                    if session::json_str_field(text, "type") == Some("mcp") {
                                        let volume =
                                            VOLUME_PCT.load(core::sync::atomic::Ordering::Relaxed);
                                        if let Some(resp) = mcp_handle(text, volume, channels) {
                                            info!("MCP 应答: {}", resp.as_str());
                                            if ws.send_text(resp.as_bytes()).await.is_err() {
                                                connected = false;
                                                channels.event.try_send(SessionEvent::Closed).ok();
                                            }
                                        }
                                    } else {
                                        info!("服务器: {}", text);
                                    }
                                },
                            }
                        },
                        2 => {
                            let mut pkt: OpusPacket = heapless::Vec::new();
                            if pkt.extend_from_slice(&payload).is_ok()
                                && channels.downlink.try_send(pkt).is_err()
                            {
                                warn!("下行通道满, 丢弃一帧");
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
    pub flash: &'a mut esp_storage::FlashStorage<'static>,
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
        flash,
    } = args;

    let mut sm = DialogSm::new();
    // 开机预热 1s: codec/DMA 上电瞬态 (含 DC 漂移) 可能越过 VAD 阈值,
    // 曾致开机假唤醒 → 卡死 (见 ROADMAP M8); 与 VAD 去 DC 双保险。
    let mut wake = WakeEngine::new(
        VadWakeDetector::new(VAD_THRESHOLD),
        WakePolicy::new(WakePolicyConfig {
            warmup_ms: 1_000,
            ..Default::default()
        }),
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
    let mut battery_last_read = 0u64;
    let mut uplink_frames: u64 = 0;
    let mut last_level_log_ms: u64 = 0;
    let mut idle_peak_rms: i32 = 0;
    let mut listen_peak_rms: i32 = 0;
    // 上行回合检测 (auto 模式): 说完 → 本地 VAD 静音 → 主动 listen stop
    let mut listen_heard = false;
    let mut listen_silence_ms: u64 = 0;
    let mut listen_total_ms: u64 = 0;
    let mut was_listening = false;
    let mut raw_dumped = false;
    let mut pending: heapless::Vec<(Action, ListenMode), 8> = heapless::Vec::new();

    for (reg, val) in audio.es7210_debug_regs() {
        info!("ES7210 0x{:02X}=0x{:02X}", reg, val);
    }
    for (off, val) in Audio::i2s_debug_regs() {
        info!("I2S0 0x{:02X}=0x{:08X}", off, val);
    }

    info!("=== 主循环启动: 待机监听 (说话唤醒 / 按住 BOOT 对讲) ===");

    loop {
        let now_ms = esp_hal::time::Instant::now()
            .duration_since_epoch()
            .as_millis();
        // ---- 1. 会话事件 ----
        loop {
            match channels.event.try_receive() {
                Ok(SessionEvent::Ready) => {
                    // 就绪提示音: 告知用户连接完成、此刻可开口 (手动/语音两种入口)
                    audio.beep(150);
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
            VOLUME_PCT.store(next, core::sync::atomic::Ordering::Relaxed);
            crate::wifi::store::save_volume(flash, next);
            info!("音量: {}%", next);
        }
        vol_edge = vol_up || vol_down;

        // ---- 3.5 MCP SetVolume 待办 + 设备状态发布 ----
        if PENDING_VOLUME_VALID.swap(false, core::sync::atomic::Ordering::Relaxed) {
            let v = PENDING_VOLUME.swap(0, core::sync::atomic::Ordering::Relaxed);
            audio.set_volume(v);
            VOLUME_PCT.store(v, core::sync::atomic::Ordering::Relaxed);
            crate::wifi::store::save_volume(flash, v);
            info!("MCP 音量: {}%", v);
        }
        // 充电状态: GPIO 实时 (不读 ADC, 与 WiFi PHY 无冲突)
        CHARGING.store(
            matches!(power.status(), xiaozhi_core::board::PowerStatus::Charging),
            core::sync::atomic::Ordering::Relaxed,
        );
        // 电量: 仅 Idle + 10s 限频 (S3 WiFi PHY 与 SARADC 共用 ADC1,
        // 会话期间频繁读 ADC 干扰射频 — 曾崩 pm_on_beacon_rx)
        if sm.state() == DialogState::Idle && now_ms - battery_last_read >= 10_000 {
            battery_last_read = now_ms;
            let pct = battery.reported_percentage(power.status()).unwrap_or(0);
            BATTERY_PCT.store(pct, core::sync::atomic::Ordering::Relaxed);
            // 10s 一次诊断 (M8 项 9 验收): 电量 + 充电态, 确认不再崩 pm_on_beacon_rx
            let charging = matches!(power.status(), xiaozhi_core::board::PowerStatus::Charging);
            info!(
                "电量: {}% (充电: {})",
                pct,
                if charging { "是" } else { "否" }
            );
            // 堆水位诊断 (10s): 内部堆 128KB, >90% 告警 (防 opus/PSRAM 泄漏)
            let hs = esp_alloc::HEAP.stats();
            let used_pct = if hs.size > 0 {
                hs.current_usage * 100 / hs.size
            } else {
                0
            };
            if used_pct > 90 {
                warn!(
                    "堆水位: {}% ({}KB/{}KB) — 接近耗尽",
                    used_pct,
                    hs.current_usage / 1024,
                    hs.size / 1024
                );
            }
        }

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
        // auto 回合检测: 进入 Listening 时重置
        let is_listening = sm.state() == DialogState::Listening;
        if is_listening && !was_listening {
            listen_heard = false;
            listen_silence_ms = 0;
            listen_total_ms = 0;
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
                    // 1Hz 电平诊断 (原始域): 阈值调参/假唤醒排查
                    // peak = 自上次打印以来的最大块 rms (说话峰值可能落在采样点之间)
                    let (mean, rms) = level_stats(&mono_buf[..ns]);
                    idle_peak_rms = idle_peak_rms.max(rms);
                    if now_ms.saturating_sub(last_level_log_ms) >= 1_000 {
                        last_level_log_ms = now_ms;
                        let regs = audio.es7210_debug_regs();
                        let rd = |reg: u8| {
                            regs.iter()
                                .find(|(r, _)| *r == reg)
                                .map(|(_, v)| *v)
                                .unwrap_or(0xFF)
                        };
                        info!(
                            "待机电平: mean={} rms={} peak={} (阈值 {}) ES7210 01={:02X} 08={:02X} 02={:02X}",
                            mean,
                            rms,
                            idle_peak_rms,
                            VAD_THRESHOLD,
                            rd(0x01),
                            rd(0x08),
                            rd(0x02)
                        );
                        idle_peak_rms = 0;
                    }
                    triggered |= wake.feed(&mono_buf[..ns], now_ms) == WakeEvent::Triggered;
                },
                DialogState::Listening => {
                    // 回合结束判定 (auto 模式): 本地能量 VAD 找静音收尾点。
                    // ⚠️ 不做噪声门: 原始人声 rms 仅 200~400, 与噪声 (30~90) 差距小,
                    // 门限稍高就会把说话帧也清零 (真机实测: 服务端只收到 0.6s 零值)。
                    let frame_ms = ns as u64 * 1000 / SAMPLE_RATE as u64;
                    listen_total_ms += frame_ms;
                    let warmup = listen_total_ms <= 600;
                    let voiced = !warmup && is_voice_active(&mono_buf[..ns], LISTEN_VAD_THRESHOLD);
                    // 一次性原始 I2S 转储 (排查字节序/槽位): 首个有声帧的前 512 帧
                    if !raw_dumped && voiced {
                        raw_dumped = true;
                        let frames = (n / 4).min(512);
                        for k in 0..frames {
                            let o = k * 4;
                            let l = i16::from_le_bytes([pop_buf[o], pop_buf[o + 1]]);
                            let r = i16::from_le_bytes([pop_buf[o + 2], pop_buf[o + 3]]);
                            info!("RAW {} L={} R={}", k, l, r);
                        }
                    }
                    if voiced {
                        listen_heard = true;
                        listen_silence_ms = 0;
                    } else {
                        if listen_heard {
                            listen_silence_ms += frame_ms;
                        }
                    }
                    apply_gain_percent(&mut mono_buf[..ns], GAIN_PERCENT);
                    let limited = limit_peak(&mut mono_buf[..ns], UPLINK_CEILING);
                    // 1Hz 上行电平诊断 (增益后): 调增益/查削顶
                    let (lmean, lrms) = level_stats(&mono_buf[..ns]);
                    listen_peak_rms = listen_peak_rms.max(lrms);
                    if now_ms.saturating_sub(last_level_log_ms) >= 1_000 {
                        last_level_log_ms = now_ms;
                        info!(
                            "聆听电平(增益后): mean={} rms={} peak={} voiced={} lim={}",
                            lmean, lrms, listen_peak_rms, voiced, limited
                        );
                        listen_peak_rms = 0;
                    }
                    if let Some(encoder) = encoder.as_mut() {
                        let mut src = &mono_buf[..ns];
                        while !src.is_empty() {
                            let take = usize::min(acc_buf.len() - acc_len, src.len());
                            acc_buf[acc_len..acc_len + take].copy_from_slice(&src[..take]);
                            acc_len += take;
                            src = &src[take..];
                            while acc_len >= FRAME_SAMPLES {
                                let mut raw = [0u8; MAX_PACKET_BYTES];
                                match encoder.encode(&acc_buf[..FRAME_SAMPLES], &mut raw) {
                                    Ok(len) => {
                                        uplink_frames += 1;
                                        if uplink_frames.is_multiple_of(100) {
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
        }
        if triggered {
            audio.beep(120);
            dispatch(&mut sm, &mut pending, DialogEvent::WakeTriggered);
        }

        // auto 模式本地收尾: 说完静音 700ms, 或 8s 无人声 → listen stop
        if sm.state() == DialogState::Listening && sm.mode() == ListenMode::Auto {
            let done = (listen_heard && listen_silence_ms >= 500)
                || (!listen_heard && listen_total_ms >= 8_000);
            if done {
                info!(
                    "本地收尾: heard={} 静音={}ms 总时长={}ms",
                    listen_heard, listen_silence_ms, listen_total_ms
                );
                dispatch(&mut sm, &mut pending, DialogEvent::AutoListenStop);
            }
        }

        // ---- 7. 下行播放: opus → 解码 → 立体声 (每轮 ≤3 帧 ≈180ms,
        // 防止连播期间 RX 积压超过排空缓冲) ----
        for _ in 0..3 {
            let pkt = match channels.downlink.try_receive() {
                Ok(p) => p,
                Err(_) => break,
            };
            let Some(decoder) = decoder.as_mut() else {
                continue;
            };
            match decoder.decode(&pkt, &mut pcm_dec) {
                Ok(n) if n > 0 => {
                    let n = n.min(pcm_dec.len());
                    let written = mono_to_stereo(&pcm_dec[..n], &mut stereo_out);
                    audio.play_block(&stereo_out[..written]);
                },
                Ok(_) => {},
                Err(e) => warn!("opus 解码失败: {}", e),
            }
        }

        // ---- 8. 时钟维持 (环形常流; 空闲补静音保 BCLK/WS 不停) ----
        // 播报期用低水位: 只在队列快见底时补静音, 既不在 TTS 帧之间插静音,
        // 也不让 DMA 绕环重放旧数据 (真机 2026-09-26: 嘟嘟嘟/一卡一卡)。
        let tx_target = if sm.state() == DialogState::Speaking {
            TTS_TX_LOW_WATER
        } else {
            TX_RING_BYTES
        };
        audio.maintain_silence(silence_buf, tx_target);
        // 让出 CPU: 会话任务/网络任务需要被轮询才能推进连接与收发
        // Idle 时放宽到 10ms (降 CPU 唤醒频率, I2S 缓冲 512ms 容量足够)
        let wait_ms = if sm.state() == DialogState::Idle {
            10
        } else {
            1
        };
        embassy_time::Timer::after(embassy_time::Duration::from_millis(wait_ms)).await;

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

/// 显示面板状态文本 (3x5 点阵字库仅 ASCII)
/// 16-bit 单声道 → 立体声 (左右复制) 交错 little-endian 字节; 返回写入字节数
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
        DialogEvent::SessionReady => "会话就绪",
        DialogEvent::TtsStart => "播报开始",
        DialogEvent::TtsStop => "播报结束",
        DialogEvent::SessionClosed => "会话关闭",
    }
}

/// MCP 请求应答 (会话任务内; 设备状态经原子量读取, set_volume 经命令下发)
///
/// 由 [`session_task`] 在收到服务器 `type=mcp` 文本帧时调用, 回送
/// initialize/tools_list/tools_call 应答 (见 session.rs HELLO 的 `features.mcp`)。
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
