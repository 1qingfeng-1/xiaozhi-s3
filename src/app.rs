//! 智能音箱主应用 — 对话状态机驱动的编排层
//!
//! 单任务事件循环 (对齐 audio-loopback 的时序模型):
//! 1. 轮询服务器消息 (文本 → 状态机事件; 二进制 → opus 解码播放)
//! 2. 轮询按键 (BOOT manual 对讲 / VOL± 音量)
//! 3. 排空采集: Idle 时喂唤醒引擎, Listening 时 opus 上行, 其余丢弃
//! 4. I2S 时钟维持 (静音窗; RX 从机依赖 TX 时钟)
//! 5. LED 灯语映射 (待机慢呼吸 / 建连快呼吸 / 聆听中呼吸 / 播报常亮)
//!
//! 状态转移全部委托 [`xiaozhi_core::dialog::DialogSm`] (host 单测覆盖)。
//! 建连时机 = 状态为 Connecting 且尚无会话 (状态即触发器, 不需要额外标记);
//! listen/abort 类动作由状态机输出后经待发队列发送。

use embassy_net::{Ipv4Address, Stack};

use esp_hal::{Blocking, dma::DmaTransferRxCircular, i2s::master::I2sRx};

use defmt::{error, info, warn};

use xiaozhi_core::audio::capture::{apply_gain_percent, extract_slot};
use xiaozhi_core::audio::wake::{
    VadWakeDetector, WakeEngine, WakeEvent, WakePolicy, WakePolicyConfig,
};
use xiaozhi_core::dialog::{Action, DialogEvent, DialogSm, DialogState, ListenMode};
use xiaozhi_core::mcp;

use crate::audio::opus::{MAX_PACKET_BYTES, OpusDecoder, OpusEncoder};
use crate::bsp::components::{
    Audio, Battery, BreathSpeed, Buttons, Display, LedMode, Leds, Power, Rgb565,
};
use crate::session;

/// 上行音频参数 (16kHz 单声道 60ms, 对齐 hello.audio_params)
const SAMPLE_RATE: u32 = 16_000;
const FRAME_MS: u32 = 60;
const FRAME_SAMPLES: usize = SAMPLE_RATE as usize / 1000 * FRAME_MS as usize; // 960
/// 软件增益百分比: 麦克风数字电平 1~5% 满幅, +18dB 后 ASR 可用
const GAIN_PERCENT: u16 = 800;
/// 唤醒 VAD 阈值 (均方幅度, 原始采集电平域; 环境噪声几十 / 正常说话 >1k)
pub const VAD_THRESHOLD: i16 = 800;

/// 乒乓窗口 = 单个 DMA 描述符 (4092B ≈ 64ms): available() 按完整描述符计数
const BLOCK_BYTES: usize = 4092;
/// 主循环返回原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// RX 饱和/故障: esp-hal 的 available 计数已损坏, 需重建采集句柄
    CaptureFault,
}

/// 会话运行时资源 (WS 传输 + 编解码器 + 解码缓冲)
struct SessionState {
    ws: session::WsClient<'static>,
    encoder: OpusEncoder,
    decoder: OpusDecoder,
    /// 解码输出 (frame_ms @ 16kHz) 与立体声交织缓冲
    pcm_dec: alloc::boxed::Box<[i16]>,
    stereo_out: alloc::boxed::Box<[u8]>,
}

/// 应用运行依赖 (借用于 main 的常驻资源)
///
/// `'a`: 各组件的重借用; `'b`: 采集句柄持有的 board.capture 借用
/// (两个生命周期分离, 否则 cap 无法在主循环直线路径上被 drop 重建)
pub struct AppArgs<'a, 'b> {
    pub audio: &'a mut Audio,
    pub buttons: &'a mut Buttons,
    pub leds: &'a mut Leds,
    pub display: &'a mut Display,
    pub battery: &'a mut Battery,
    pub power: &'a mut Power,
    pub cap: &'a mut DmaTransferRxCircular<'b, I2sRx<'static, Blocking>>,
    pub stack: Stack<'static>,
    pub server: Ipv4Address,
    pub mac: heapless::String<32>,
}

/// 主事件循环
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
        stack,
        server,
        mac,
    } = args;

    let mut sm = DialogSm::new();
    let mut wake = WakeEngine::new(
        VadWakeDetector::new(VAD_THRESHOLD),
        WakePolicy::new(WakePolicyConfig::default()),
    );
    let mut session: Option<SessionState> = None;
    let mut acc_len = 0usize;
    let mut held = false;
    let mut vol_edge = false;
    let mut last_led: Option<LedMode> = None;
    let mut last_state: Option<DialogState> = None;
    // 状态机输出的待发网络动作 (listen/abort; Connect/提示音在注入点就地处理)
    let mut pending: heapless::Vec<Action, 8> = heapless::Vec::new();

    info!("=== 主循环启动: 待机监听 (说话唤醒 / 按住 BOOT 对讲) ===");

    loop {
        let now_ms = esp_hal::time::Instant::now()
            .duration_since_epoch()
            .as_millis();

        // ---- 1. 服务器消息 → 状态机事件 / 音频播放 ----
        if let Some(sess) = session.as_mut() {
            if sess.ws.socket.can_recv() {
                match sess.ws.recv_frame().await {
                    Ok((opcode, payload)) => match opcode {
                        1 => {
                            let text = core::str::from_utf8(&payload).unwrap_or("");
                            match (
                                session::json_str_field(text, "type"),
                                session::json_str_field(text, "state"),
                            ) {
                                (Some("tts"), Some("start")) => {
                                    info!("服务器: tts start");
                                    dispatch(&mut sm, &mut pending, DialogEvent::TtsStart);
                                },
                                (Some("tts"), Some("stop")) => {
                                    info!("服务器: tts stop");
                                    wake.policy_mut().note_playback_end(now_ms);
                                    dispatch(&mut sm, &mut pending, DialogEvent::TtsStop);
                                },
                                (Some("mcp"), _) => {
                                    handle_mcp(text, audio, battery, power, &mut sess.ws).await;
                                },
                                _ => info!("服务器: {}", text),
                            }
                        },
                        2 => match sess.decoder.decode(&payload, &mut sess.pcm_dec) {
                            Ok(n) if n > 0 => {
                                let mut o = 0;
                                for &s in &sess.pcm_dec[..n] {
                                    let le = s.to_le_bytes();
                                    sess.stereo_out[o..o + 2].copy_from_slice(&le);
                                    sess.stereo_out[o + 2..o + 4].copy_from_slice(&le);
                                    o += 4;
                                }
                                audio.play_block(&sess.stereo_out[..n * 4]);
                            },
                            Ok(_) => {},
                            Err(e) => warn!("opus 解码失败: {}", e),
                        },
                        8 => {
                            info!("服务器关闭连接");
                            dispatch(&mut sm, &mut pending, DialogEvent::SessionClosed);
                        },
                        9 => {
                            let _ = sess.ws.send_pong(&payload).await;
                        },
                        _ => {},
                    },
                    Err(e) => {
                        warn!("WS 接收错误: {} — 会话关闭", e);
                        dispatch(&mut sm, &mut pending, DialogEvent::SessionClosed);
                    },
                }
            }
        }

        // ---- 2. 会话关闭善后 ----
        if session.is_some() && sm.state() == DialogState::Idle {
            wake.policy_mut().note_session_end(now_ms);
            session = None; // 丢弃 → TcpSocket 关闭, 编解码器销毁
            info!("会话已结束, 回到待机监听");
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

        // ---- 4. 建连 (状态即触发器: Connecting 且尚无会话) ----
        if sm.state() == DialogState::Connecting && session.is_none() {
            let mut ok = None;
            let mut last_err = "";
            for attempt in 1..=3u8 {
                match session::connect(stack, server, &mac).await {
                    Ok(r) => {
                        ok = Some(r);
                        break;
                    },
                    Err(e) => {
                        warn!("会话连接 (第 {} 次): {}", attempt, e);
                        last_err = e;
                        embassy_time::Timer::after(embassy_time::Duration::from_secs(1)).await;
                    },
                }
            }
            match ok {
                Some((ws, rate, frame_ms)) => {
                    info!("服务器 hello: {}Hz / {}ms", rate, frame_ms);
                    let dec_samples = frame_ms as usize * SAMPLE_RATE as usize / 1000;
                    match (
                        OpusEncoder::new_voip_mono(SAMPLE_RATE, FRAME_MS),
                        OpusDecoder::new_mono(SAMPLE_RATE),
                    ) {
                        (Ok(encoder), Ok(decoder)) => {
                            session = Some(SessionState {
                                ws,
                                encoder,
                                decoder,
                                pcm_dec: alloc::vec![0i16; dec_samples].into_boxed_slice(),
                                stereo_out: alloc::vec![0u8; dec_samples * 4].into_boxed_slice(),
                            });
                            acc_len = 0;
                            dispatch(&mut sm, &mut pending, DialogEvent::SessionReady);
                        },
                        _ => {
                            error!("opus 编解码器创建失败");
                            dispatch(&mut sm, &mut pending, DialogEvent::SessionClosed);
                        },
                    }
                },
                None => {
                    error!("会话建立失败: {} — 回到待机", last_err);
                    dispatch(&mut sm, &mut pending, DialogEvent::SessionClosed);
                },
            }
        }

        // ---- 5. 执行待发的网络动作 ----
        for i in 0..pending.len() {
            let action = pending[i];
            let Some(sess) = session.as_mut() else {
                continue;
            };
            let msg: &str = match action {
                Action::SendListenStart => match sm.mode() {
                    ListenMode::Auto => session::LISTEN_START_AUTO,
                    ListenMode::Manual => session::LISTEN_START_MANUAL,
                },
                Action::SendListenStop => session::LISTEN_STOP,
                Action::SendTtsAbort => session::TTS_ABORT,
                _ => continue,
            };
            if let Err(e) = sess.ws.send_text(msg.as_bytes()).await {
                warn!("发送 {} 失败: {}", msg, e);
                dispatch(&mut sm, &mut pending, DialogEvent::SessionClosed);
                break;
            }
        }
        pending.clear();

        // ---- 6. 采集排空 (每轮必做, 时钟运行期间防饱和) ----
        let mut triggered = false;
        match cap.available() {
            Ok(avail) if avail > pop_buf.len() => {
                error!("采集积压 {}B 超限, 重建采集", avail);
                return RunOutcome::CaptureFault;
            },
            Ok(avail) if avail >= BLOCK_BYTES => match cap.pop(&mut pop_buf[..avail]) {
                Ok(n) => {
                    let ns = extract_slot(&pop_buf[..(n / 4) * 4], 2, 0, mono_buf);
                    apply_gain_percent(&mut mono_buf[..ns], GAIN_PERCENT);
                    match sm.state() {
                        DialogState::Idle => {
                            // 唤醒监听 (真实毫秒时钟注入策略层)
                            if wake.feed(&mono_buf[..ns], now_ms) == WakeEvent::Triggered {
                                triggered = true;
                            }
                        },
                        DialogState::Listening => {
                            if let Some(sess) = session.as_mut() {
                                // 累积 960 样本 → 编码 → 裸 opus 二进制帧
                                let mut src = &mono_buf[..ns];
                                while !src.is_empty() {
                                    let take = usize::min(acc_buf.len() - acc_len, src.len());
                                    acc_buf[acc_len..acc_len + take].copy_from_slice(&src[..take]);
                                    acc_len += take;
                                    src = &src[take..];
                                    while acc_len >= FRAME_SAMPLES {
                                        let mut pkt = [0u8; MAX_PACKET_BYTES];
                                        match sess
                                            .encoder
                                            .encode(&acc_buf[..FRAME_SAMPLES], &mut pkt)
                                        {
                                            Ok(len) => {
                                                if sess.ws.send_frame(2, &pkt[..len]).await.is_err()
                                                {
                                                    dispatch(
                                                        &mut sm,
                                                        &mut pending,
                                                        DialogEvent::SessionClosed,
                                                    );
                                                    break;
                                                }
                                            },
                                            Err(e) => warn!("opus 编码失败: {}", e),
                                        }
                                        acc_buf.copy_within(FRAME_SAMPLES..acc_len, 0);
                                        acc_len -= FRAME_SAMPLES;
                                    }
                                    if sm.state() == DialogState::Idle {
                                        break; // 会话在发送中被关闭
                                    }
                                }
                            }
                        },
                        _ => {}, // Speaking/Connecting/WaitingReply: 排空即弃 (回声/残留)
                    }
                },
                Err(e) => {
                    warn!("RX pop 失败: {:?} — 重建采集", e);
                    return RunOutcome::CaptureFault;
                },
            },
            Ok(_) => {},
            Err(e) => {
                warn!("RX available 失败: {:?} — 重建采集", e);
                return RunOutcome::CaptureFault;
            },
        }
        if triggered {
            // 唤醒提示音 (阻塞 ~120ms) → 状态机进入 Connecting
            audio.beep(120);
            dispatch(&mut sm, &mut pending, DialogEvent::WakeTriggered);
        }

        // ---- 7. 时钟维持 (Listening 无下行时补静音窗) ----
        if sm.state() == DialogState::Listening {
            audio.play_block(silence_buf);
        }

        // ---- 8. 灯语映射 (状态变化时切换) + 呼吸 tick ----
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

        // ---- 9. 显示状态 (状态切换时刷新, 英文点阵字库) ----
        if last_state != Some(sm.state()) {
            last_state = Some(sm.state());
            let text = state_text(sm.state());
            let color = match sm.state() {
                DialogState::Speaking => Rgb565::YELLOW,
                DialogState::Listening => Rgb565::GREEN,
                _ => Rgb565::CYAN,
            };
            display.draw_status(text, color);
        }
    }
}

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

/// 对话状态标签 (core 类型不依赖 defmt, 日志用文本映射)
fn state_label(state: DialogState) -> &'static str {
    match state {
        DialogState::Idle => "待机",
        DialogState::Connecting => "建连",
        DialogState::Listening => "聆听",
        DialogState::WaitingReply => "等回复",
        DialogState::Speaking => "播报",
    }
}

/// 事件注入: 状态转移 + 动作入队
///
/// `ConnectSession` 不入队 (建连由 "Connecting 且无会话" 条件驱动);
/// `PlayWakeChime` 不入队 (唤醒注入点同步 beep)。其余 (listen/abort) 入队。
fn dispatch(sm: &mut DialogSm, pending: &mut heapless::Vec<Action, 8>, ev: DialogEvent) {
    let (state, actions) = sm.transition(ev);
    for a in actions {
        match *a {
            Action::ConnectSession | Action::PlayWakeChime => {},
            other => {
                let _ = pending.push(other);
            },
        }
    }
    info!("状态: {}", state_label(state));
}

/// MCP 请求处理: 解析 JSON-RPC 并执行工具 (音量/设备状态), 回应答帧
async fn handle_mcp(
    text: &str,
    audio: &mut Audio,
    battery: &mut Battery,
    power: &mut Power,
    ws: &mut session::WsClient<'_>,
) {
    let Some(req) = mcp::parse_request(text) else {
        return; // notifications / 无法解析: 无需回应
    };
    const BOARD_NAME: &str = crate::bsp::active::BOARD.name;
    const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

    let resp: heapless::String<1024> = match req.method {
        mcp::McpMethod::Initialize => mcp::initialize_response(req.id, BOARD_NAME, APP_VERSION),
        mcp::McpMethod::ToolsList => mcp::tools_list_response(req.id),
        mcp::McpMethod::ToolsCall { name, volume } => match (name, volume) {
            ("self.get_device_status", _) => {
                let battery_pct = battery.reported_percentage(power.status());
                let status = mcp::status_json(
                    audio.volume(),
                    battery_pct,
                    power.status() == xiaozhi_core::board::PowerStatus::Charging,
                );
                mcp::text_result_response(req.id, status.as_str())
            },
            ("self.audio_speaker.set_volume", Some(v)) => {
                let v = v.clamp(0, 100) as u8;
                audio.set_volume(v);
                info!("MCP: 音量设为 {}%", v);
                mcp::text_result_response(req.id, "true")
            },
            ("self.audio_speaker.set_volume", None) => {
                mcp::error_response(req.id, "Missing argument: volume")
            },
            (_other, _) => mcp::error_response(req.id, "Unknown tool"),
        },
        mcp::McpMethod::Unknown(_) => mcp::error_response(req.id, "Method not implemented"),
    };
    if let Err(e) = ws.send_text(resp.as_bytes()).await {
        warn!("MCP 响应发送失败: {}", e);
    }
}
