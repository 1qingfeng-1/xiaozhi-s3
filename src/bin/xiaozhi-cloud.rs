//! M4.2 调试: opus 音频集成 — 麦克风 opus 编码上行 + 服务器 TTS 解码播放
//!
//! 流程: create_board (音频 + 内部堆 + PSRAM) → STA 连接 → WebSocket →
//! hello 握手 → 会话循环:
//! - 按住 BOOT = 对讲: listen start/stop + 60ms opus 帧上行 (16kHz 单声道)
//! - 服务器二进制帧 = TTS opus → 以 16kHz 解码 (libopus 内部重采样) → 播放
//!
//! 上行/下行二进制帧均为裸 opus 载荷 (协议 version 1, 与 C++ 参考一致)。
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
use embassy_net::{Ipv4Address, StackResources, tcp::TcpSocket};
use esp_hal::{
    Blocking, clock::CpuClock, dma::DmaTransferRxCircular, i2s::master::I2sRx,
    interrupt::software::SoftwareInterruptControl, peripherals::Peripherals,
    timer::timg::TimerGroup,
};
use esp_storage::FlashStorage;

use defmt::{error, info, warn};
use esp_println as _;

use xiaozhi_core::audio::capture::{apply_gain_percent, extract_slot};
use xiaozhi_s3::audio::opus::{MAX_PACKET_BYTES, OpusDecoder, OpusEncoder};
use xiaozhi_s3::bsp::components::{Audio, Buttons};
use xiaozhi_s3::bsp::create_board;
use xiaozhi_s3::wifi::{self, Mode, store};

#[panic_handler]
fn panic(panic_info: &core::panic::PanicInfo) -> ! {
    error!("{}", panic_info);
    esp_hal::system::software_reset()
}

extern crate alloc;
use alloc::{format, string::ToString, vec};

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

/// 上行音频参数 (与 hello.audio_params 一致, 对齐 C++ 参考)
const SAMPLE_RATE: u32 = 16_000;
const FRAME_MS: u32 = 60;
const FRAME_SAMPLES: usize = SAMPLE_RATE as usize / 1000 * FRAME_MS as usize; // 960
/// 软件增益百分比 (100 = 0dB): 麦克风数字电平约 1~5% 满幅, +18dB 后 ASR 可用
const GAIN_PERCENT: u16 = 800;

/// 乒乓窗口 = 单个 DMA 描述符 (4092B ≈ 64ms): available() 按完整描述符计数
const BLOCK_BYTES: usize = 4092;
/// 采集积压缓冲上限 16KB (4 描述符); 超过说明处理跟不上 → 结束会话重连
const POP_BUF_SIZE: usize = 16 * 1024;
/// 帧累积缓冲: 2 帧余量 (pop 分块追加, 见 uplink)
const ACC_SAMPLES: usize = FRAME_SAMPLES * 2;

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    info!("=== xiaozhi 云连接 + opus 音频 (M4.2) ===");

    // 板子工厂: 内部堆第一行注册 → 音频 DMA 缓冲 → PSRAM 最后注册
    // (顺序不可换: S3 GDMA 访问不了 PSRAM, 见 bsp/gezipai.rs)
    let mut board = create_board(peripherals);
    info!("板子 [{}] 初始化完成", board.name);

    // create_board 按值消耗了 Peripherals; 音频不用的 WIFI/FLASH/TIMG0 经
    // steal 再取 (各自仅初始化一次, 无重复初始化风险)
    let p = unsafe { Peripherals::steal() };
    let timg0 = TimerGroup::new(p.TIMG0);
    let sw_interrupt = SoftwareInterruptControl::new(p.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    let flash = mk_static!(FlashStorage<'static>, FlashStorage::new(p.FLASH));

    // BOOT 键: 清除配置强制重回配网
    if board.buttons.boot_pressed() {
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
        p.WIFI,
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

    // 采集常驻启动: RX 为 I2S 从机, 无 TX 时钟时自然无数据, 无需按 listen
    // 状态启停 (跨 await 重启采集有借用/时序双坑)。会话结束时在下方主循环
    // 重建句柄。TX 时钟由会话循环里的静音窗/TTS 播放维持。
    let mut cap = match board.capture.start() {
        Ok(t) => t,
        Err(e) => {
            error!("采集启动失败: {:?}", e);
            esp_hal::system::software_reset()
        },
    };

    // 会话常驻缓冲 (mk_static 一次, 重连不重复分配)
    let pop_buf = mk_static!([u8; POP_BUF_SIZE], [0u8; POP_BUF_SIZE]);
    let mono_buf = mk_static!([i16; POP_BUF_SIZE / 4], [0i16; POP_BUF_SIZE / 4]);
    let acc_buf = mk_static!([i16; ACC_SAMPLES], [0i16; ACC_SAMPLES]);
    let silence_buf = mk_static!([u8; BLOCK_BYTES], [0u8; BLOCK_BYTES]);

    // WebSocket 重连循环
    loop {
        info!(
            "连接 WebSocket: {}:{}{}",
            config.server.as_str(),
            WS_PORT,
            WS_PATH
        );
        // 重连循环内不可用 mk_static! (同一调用点二次初始化会 panic), 改堆泄漏
        // (仅断线重连时分配一次, 量级极小)
        let rx = vec![0u8; 4096].leak();
        let tx = vec![0u8; 4096].leak();
        let mut socket = TcpSocket::new(stack, rx, tx);
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

        // 等待服务器 hello: 解析 frame_duration 决定解码缓冲大小
        let server_hello = match ws.recv_frame().await {
            Ok((1, payload)) => core::str::from_utf8(&payload).unwrap_or("?").to_string(),
            Ok((opcode, _)) => {
                warn!("服务器首帧非文本 (opcode {})", opcode);
                continue;
            },
            Err(e) => {
                error!("服务器 hello 接收失败: {} — 重连", e);
                continue;
            },
        };
        let server_rate = json_num_field(&server_hello, "sample_rate").unwrap_or(24_000);
        let frame_ms = json_num_field(&server_hello, "frame_duration").unwrap_or(60);
        info!(
            "服务器 hello: rate={}Hz frame={}ms — {}",
            server_rate,
            frame_ms,
            server_hello.as_str()
        );

        // 编解码器 (每会话新建, 重连即复位状态)
        // 解码器建在 16kHz: libopus 内部把服务器 (通常 24kHz) 流重采样到本地
        let mut encoder = match OpusEncoder::new_voip_mono(SAMPLE_RATE, FRAME_MS) {
            Ok(e) => e,
            Err(e) => {
                error!("opus 编码器创建失败: {} — 重连", e);
                embassy_time::Timer::after(embassy_time::Duration::from_secs(5)).await;
                continue;
            },
        };
        let mut decoder = match OpusDecoder::new_mono(SAMPLE_RATE) {
            Ok(d) => d,
            Err(e) => {
                error!("opus 解码器创建失败: {} — 重连", e);
                embassy_time::Timer::after(embassy_time::Duration::from_secs(5)).await;
                continue;
            },
        };
        // 解码输出 = frame_ms @ 16kHz (与服务器采样率无关)
        let dec_samples = frame_ms as usize * SAMPLE_RATE as usize / 1000;
        let pcm_dec = vec![0i16; dec_samples].leak();
        let stereo_out = vec![0u8; dec_samples * 4].leak();

        info!("=== 会话就绪 — 按住 BOOT 对讲, 服务器 TTS 自动播放 ===");
        board.audio.beep(150);
        // 板子组件按字段拆分借用: cap 持有 capture 的借用贯穿会话,
        // audio/buttons 需与其 disjoint (async 生成器要求借用区域完整)
        session_loop(
            &mut ws,
            &mut board.audio,
            &mut board.buttons,
            &mut cap,
            &mut encoder,
            &mut decoder,
            &mut pcm_dec[..],
            &mut stereo_out[..],
            &mut pop_buf[..],
            &mut mono_buf[..],
            &mut acc_buf[..],
            &mut silence_buf[..],
        )
        .await;
        // 会话结束 (WS 断开/关闭, 或采集积压超限) —— 无论原因都重建采集句柄:
        // RX 饱和触发 Late 后 available() 计数已损坏, 只能整体重建; 常规重连
        // 场景下重启也仅损失 ~28B 采集。此处 drop→start 是直线路径 (async
        // 生成器里条件路径的 drop→重启借用检查过不去, 见 AGENTS.md)。
        drop(cap);
        cap = match board.capture.start() {
            Ok(t) => t,
            Err(e) => {
                error!("采集重启失败: {:?}", e);
                esp_hal::system::software_reset()
            },
        };
        // session_loop 返回 = 连接断开, 重连
        embassy_time::Timer::after(embassy_time::Duration::from_secs(2)).await;
    }
}

/// 会话循环: BOOT 对讲 (采集→opus→上行) + 服务器 TTS (下行→解码→播放)
///
/// 半双工: 时钟由任一方向的 I2S 播放维持 (RX 从机依赖 TX 时钟,
/// 见 bsp/components/audio.rs) —— 对讲时无下行则补静音窗。
#[allow(clippy::too_many_arguments)]
async fn session_loop(
    ws: &mut WsClient<'_>,
    audio: &mut Audio,
    buttons: &mut Buttons,
    cap: &mut DmaTransferRxCircular<'_, I2sRx<'static, Blocking>>,
    encoder: &mut OpusEncoder,
    decoder: &mut OpusDecoder,
    pcm_dec: &mut [i16],
    stereo_out: &mut [u8],
    pop_buf: &mut [u8],
    mono_buf: &mut [i16],
    acc_buf: &mut [i16],
    silence_buf: &mut [u8],
) {
    let mut held = false;
    let mut acc_len = 0usize;
    let mut enc_out = [0u8; MAX_PACKET_BYTES];

    loop {
        let mut played = false;

        // ---- 服务器消息 (非阻塞轮询) ----
        if ws.socket.can_recv() {
            match ws.recv_frame().await {
                Ok((opcode, payload)) => match opcode {
                    1 => {
                        info!("服务器: {}", core::str::from_utf8(&payload).unwrap_or("?"));
                    },
                    2 => {
                        // TTS opus → 16kHz 解码 → 立体声播放
                        match decoder.decode(&payload, pcm_dec) {
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
                    },
                    8 => {
                        info!("服务器关闭连接");
                        return;
                    },
                    9 => {
                        // ping → pong 维持长连接
                        let _ = ws.send_pong(&payload).await;
                    },
                    _ => {},
                },
                Err(e) => {
                    warn!("WS 接收错误: {}", e);
                    return;
                },
            }
        }

        // ---- BOOT 按键 → listen 状态切换 ----
        let pressed = buttons.boot_pressed();
        if pressed != held {
            held = pressed;
            if held {
                info!("listen start (对讲)");
                acc_len = 0;
                // 冲掉 listen 期间残留的采集积压 (TTS 播放期间时钟在跑,
                // RX 里存着回声/旧数据)
                if let Ok(avail) = cap.available() {
                    let discard = usize::min(avail, pop_buf.len());
                    let _ = cap.pop(&mut pop_buf[..discard]);
                }
            } else {
                info!("listen stop");
            }
            let msg = if held {
                "{\"type\":\"listen\",\"state\":\"start\",\"mode\":\"manual\"}"
            } else {
                "{\"type\":\"listen\",\"state\":\"stop\"}"
            };
            if let Err(e) = ws.send_text(msg.as_bytes()).await {
                error!("listen 发送失败: {}", e);
                return;
            }
        }

        // ---- 采集排空 (每轮必做, 无论是否对讲) ----
        // I2S 时钟运行期间 (TTS 播放或对讲静音窗) RX 持续填充, 不排空会让
        // 描述符链 ~0.5s 后全部饱和 → 之后 available() 永远返回 Late,
        // 且内部计数已损坏 (只能重建采集句柄恢复)。对讲时取走的数据走
        // opus 上行, 其余 (TTS 回声/残留) 排空即弃。
        match cap.available() {
            Ok(avail) if avail > pop_buf.len() => {
                // 积压超限 (处理 >0.25s 没跟上): 结束会话, 由 main 重建采集
                error!("采集积压 {}B 超限, 结束会话", avail);
                return;
            },
            Ok(avail) if avail >= BLOCK_BYTES => {
                match cap.pop(&mut pop_buf[..avail]) {
                    Ok(n) => {
                        if held {
                            // 提取 MIC1 → 增益 → 累积 960 样本 → opus 上行
                            let frames = n / 4;
                            let ns = extract_slot(&pop_buf[..frames * 4], 2, 0, mono_buf);
                            apply_gain_percent(&mut mono_buf[..ns], GAIN_PERCENT);
                            if uplink(
                                &mut acc_buf[..],
                                &mut acc_len,
                                &mono_buf[..ns],
                                encoder,
                                ws,
                                &mut enc_out,
                            )
                            .await
                            .is_err()
                            {
                                return;
                            }
                        }
                    },
                    Err(e) => {
                        warn!("RX pop 失败: {:?}", e);
                        return;
                    },
                }
            },
            Ok(_) => {},
            Err(e) => {
                warn!("RX available 失败: {:?} — 结束会话重建采集", e);
                return;
            },
        }
        // 时钟维持: 对讲期间无下行播放则补静音窗 (RX 从机依赖 TX 时钟,
        // 与 audio-loopback 相同策略)
        if held && !played {
            audio.play_block(silence_buf);
        }

        embassy_time::Timer::after(embassy_time::Duration::from_millis(10)).await;
    }
}

/// 把新采集的样本追加进帧累积缓冲, 凑满 960 样本即编码上行
///
/// 返回 Err 表示 WS 发送失败 (会话结束)。
async fn uplink(
    acc_buf: &mut [i16],
    acc_len: &mut usize,
    new_samples: &[i16],
    encoder: &mut OpusEncoder,
    ws: &mut WsClient<'_>,
    enc_out: &mut [u8; MAX_PACKET_BYTES],
) -> Result<(), ()> {
    let mut src = new_samples;
    while !src.is_empty() {
        // 追加到累积缓冲满为止
        let take = usize::min(acc_buf.len() - *acc_len, src.len());
        acc_buf[*acc_len..*acc_len + take].copy_from_slice(&src[..take]);
        *acc_len += take;
        src = &src[take..];
        // 凑满整帧 → 编码 → 二进制帧上行 (裸 opus, 协议 version 1)
        while *acc_len >= FRAME_SAMPLES {
            match encoder.encode(&acc_buf[..FRAME_SAMPLES], enc_out) {
                Ok(n) => {
                    if ws.send_frame(2, &enc_out[..n]).await.is_err() {
                        return Err(());
                    }
                },
                Err(e) => {
                    warn!("opus 编码失败: {}", e);
                },
            }
            acc_buf.copy_within(FRAME_SAMPLES..*acc_len, 0);
            *acc_len -= FRAME_SAMPLES;
        }
    }
    Ok(())
}

/// 从 JSON 文本提取 `"key":<数字>` 字段值 (xiaozhi hello 的 audio_params 用)
fn json_num_field(json: &str, key: &str) -> Option<u32> {
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

/// 极简 WebSocket 客户端 (客户端帧强制掩码, 服务端帧不掩码)
struct WsClient<'a> {
    socket: TcpSocket<'a>,
}

impl WsClient<'_> {
    /// 发送文本帧
    async fn send_text(&mut self, payload: &[u8]) -> Result<(), &'static str> {
        self.send_frame(0x1, payload).await
    }

    /// 回复 ping (pong = opcode 0xA, 载荷原样返回)
    async fn send_pong(&mut self, payload: &[u8]) -> Result<(), &'static str> {
        self.send_frame(0xA, payload).await
    }

    /// 发送帧 (FIN + opcode, 掩码); 载荷上限 = opus 单包最大值
    async fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), &'static str> {
        if payload.len() > MAX_PACKET_BYTES {
            return Err("帧过长");
        }
        let rng = esp_hal::rng::Rng::new();
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
        ws_write_all(&mut self.socket, &frame).await
    }

    /// 接收一帧 (服务端帧不掩码), 返回 (opcode, payload)
    async fn recv_frame(&mut self) -> Result<(u8, heapless::Vec<u8, 2048>), &'static str> {
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
        if len > 2048 {
            return Err("帧过长");
        }
        let mut mask = [0u8; 4];
        if masked {
            read_exact(&mut self.socket, &mut mask)
                .await
                .map_err(|_| "读取掩码失败")?;
        }
        let mut payload = heapless::Vec::<u8, 2048>::new();
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

/// 发送 hello 消息 (xiaozhi 协议, version 1 = 裸二进制音频帧)
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
