//! 主入口 — 智能音箱产品固件
//!
//! 流程: create_board (音频+PSRAM) → esp-rtos → 读凭据 (无则配网) →
//! WiFi 连接 → 主应用循环 (唤醒监听 / 对话 / 播报 / 打断)。
//!
//! 采集常驻运行; RX 饱和故障时在直线路径重建句柄后重进主循环
//! (async 生成器里条件路径的 drop→重启借用检查过不去, 见 AGENTS.md)。
//!
//! panic 时打印回溯栈 (bt: 0x...) 后软复位; 符号化:
//! ```bash
//! xtensa-esp32s3-elf-addr2line -pfiaC -e target/xtensa-esp32s3-none-elf/release/xiaozhi-s3 <地址...>
//! ```

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

use embassy_executor::Spawner;
use embassy_sync::channel::Channel;
use esp_hal::{
    clock::CpuClock, interrupt::software::SoftwareInterruptControl, peripherals::Peripherals,
    timer::timg::TimerGroup,
};
use esp_storage::FlashStorage;

use defmt::{error, info};
use esp_println as _;

use xiaozhi_s3::app::{self, AppArgs, Channels};
use xiaozhi_s3::bsp::components::Rgb565;
use xiaozhi_s3::bsp::create_board;
use xiaozhi_s3::wifi::{self, Mode, store};

#[panic_handler]
fn panic(panic_info: &core::panic::PanicInfo) -> ! {
    error!("{}", panic_info);
    // 回溯栈: 异常/panic 时的调用链 PC (用 addr2line 映射到源码位置,
    // 命令见 main 文档注释)
    let bt = esp_backtrace::Backtrace::capture();
    for frame in bt.frames() {
        error!("bt: 0x{:x}", frame.program_counter());
    }
    esp_hal::system::software_reset()
}

extern crate alloc;

// This creates a default app-descriptor required by the esp-idf bootloader.
esp_bootloader_esp_idf::esp_app_desc!();

/// 采集排空缓冲 = 整条 RX 链 (8×4092): esp-hal 的 pop 是全量语义
/// (缓冲必须容纳当前 accumulated 的全部数据, 否则 BufferTooSmall)
const POP_BUF_SIZE: usize = 4092 * 8;
/// 单 DMA 描述符字节数 (4092B ≈ 64ms @ 16kHz 立体声)
const BLOCK_BYTES: usize = 4092;

/// 从 PSRAM (External) 分配字节缓冲; 失败回退内部 RAM
fn psram_bytes(len: usize) -> &'static mut [u8] {
    use core::alloc::Layout;
    let layout = Layout::from_size_align(len, 4).expect("bad layout");
    // SAFETY: 非零布局, 分配块常驻 (泄漏至重启)
    let p =
        unsafe { esp_alloc::HEAP.alloc_caps(esp_alloc::MemoryCapability::External.into(), layout) };
    if p.is_null() {
        alloc::vec![0u8; len].leak()
    } else {
        // SAFETY: alloc_caps 返回 len 字节可写内存; 必须清零 (否则"静音"是随机字节)
        unsafe {
            core::ptr::write_bytes(p, 0, len);
            core::slice::from_raw_parts_mut(p, len)
        }
    }
}

fn psram_i16(len: usize) -> &'static mut [i16] {
    let b = psram_bytes(len * 2);
    // SAFETY: 4 字节对齐, 长度按 i16 计
    unsafe { core::slice::from_raw_parts_mut(b.as_mut_ptr().cast(), len) }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    info!("=== 小智音箱启动 ===");
    info!(
        "复位原因: {:?}",
        defmt::Debug2Format(&esp_hal::system::reset_reason())
    );
    #[cfg(feature = "ota-selftest-crash")]
    {
        defmt::error!("OTA 回滚自测: 确认前主动 panic");
        panic!("ota-selftest-crash");
    }

    // 板子工厂: 内部堆第一行注册 → 音频 DMA 缓冲 → PSRAM 最后注册
    let mut board = create_board(peripherals);
    info!("板子 [{}] 初始化完成", board.name);

    // 电池电量: ADC 只在 WiFi 启动前读一次 (S3 的 WiFi PHY 与 SARADC 共用,
    // WiFi 激活期间读 ADC1 会干扰 PHY, 曾触发 WiFi blob 崩溃, 见 app.rs)
    let power_status = board.power.status();
    xiaozhi_s3::app::BATTERY_PCT.store(
        board.battery.reported_percentage(power_status).unwrap_or(0),
        core::sync::atomic::Ordering::Relaxed,
    );
    // 供电状态供 OTA 低电保护使用 (充电中/已充满不拦截升级)
    xiaozhi_s3::app::CHARGING.store(
        matches!(
            power_status,
            xiaozhi_core::board::PowerStatus::Charging | xiaozhi_core::board::PowerStatus::Charged
        ),
        core::sync::atomic::Ordering::Relaxed,
    );

    // create_board 按值消耗了 Peripherals; 音频不用的 WIFI/FLASH/TIMG0 经 steal 再取
    let p = unsafe { Peripherals::steal() };
    let timg0 = TimerGroup::new(p.TIMG0);
    // MWDT 与 timer0 同组: 先取出看门狗句柄, timer0 交给 embassy 时基
    let mwdt = timg0.wdt;
    let sw_interrupt = SoftwareInterruptControl::new(p.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    let flash = xiaozhi_s3::mk_static!(FlashStorage<'static>, FlashStorage::new(p.FLASH));

    // 音量恢复 (NVS 0xD000; 无记录 → 默认 60): 原子量 + ES8311 硬件同步
    let saved_vol = xiaozhi_s3::wifi::store::load_volume(flash);
    xiaozhi_s3::app::VOLUME_PCT.store(saved_vol, core::sync::atomic::Ordering::Relaxed);
    // 开机软启动: 提示音/首轮限到 50%, 防高音量瞬时电流掉压复位
    let boot_vol = saved_vol.min(50);
    board.audio.set_volume(boot_vol);
    info!("音量恢复: {}% (软启动 {}%)", saved_vol, boot_vol);

    // xiaozhi WS 协议版本 (NVS 0xB000, 默认 1): ≥2 时上行帧带时间戳
    // (最终传输方式由 OTA 是否下发 MQTT 配置决定, 见下方会话任务选择)
    let ws_version = store::load_ws_version(flash);
    let mut server_aec = ws_version >= 2;
    info!("WS 协议版本: v{}", ws_version);

    // BOOT 键开机按住: 清除凭据强制重回配网
    if board.buttons.boot_pressed() {
        info!("BOOT 按下: 清除 WiFi 配置...");
        let _ = store::clear(flash);
        embassy_time::Timer::after(embassy_time::Duration::from_millis(200)).await;
    }

    // 任务间通道 (静态常驻)
    let channels = xiaozhi_s3::mk_static!(
        Channels,
        Channels {
            uplink: Channel::new(),
            downlink: Channel::new(),
            command: Channel::new(),
            event: Channel::new(),
        }
    );

    // WiFi: 有凭据连 STA; 无凭据/连接失败 → 配网模式 (SoftAP 表单, 保存后重启)
    // controller 必须活到 main 结束 (保活射频状态), 勿提前 drop
    let (stack, server, controller, mac) = match wifi::boot_mode(flash) {
        Mode::Station(config) => match wifi::connect_station(&spawner, &config, p.WIFI).await {
            Ok((stack, controller, mac)) => {
                let server = match config.server.as_str().parse() {
                    Ok(ip) => ip,
                    Err(_) => {
                        error!("服务器地址无效: {}", config.server.as_str());
                        esp_hal::system::software_reset();
                    },
                };
                (stack, server, controller, mac)
            },
            Err(_) => {
                error!("WiFi 连接失败, 清除凭据重启进入配网");
                let _ = store::clear(flash);
                esp_hal::system::software_reset();
            },
        },
        Mode::Provision => wifi::provision::run(spawner, p.WIFI, flash).await,
    };
    // WiFi 省电: UDP 音频对丢包/抖动敏感, modem sleep 会让 AP 缓存并突发
    // 投递下行包 (实测长句 TTS 断序 ~18% → 卡顿/杂音), 会话设备必须关闭。
    // 代价是待机功耗略升; 后续可按会话状态自适应切换。
    use esp_radio::wifi::PowerSaveMode;
    let mut controller = controller;
    let _ = controller.set_power_saving(PowerSaveMode::None);
    // 控制器交给重连任务保活射频: 运行期断线自动重连 (原先仅 main 持有)
    let reconnect_token = wifi::reconnect_task(controller).unwrap();
    spawner.spawn(reconnect_token);
    info!("设备 ID: {}", mac.as_str());

    // 启动确认: 新镜像首次跑到这里 (WiFi 已连通) 才置 VALID, 否则引导器回滚
    let _ = xiaozhi_s3::ota_check::confirm_running_image(flash);

    // 开机 OTA: **独立任务** (勿内联 main — main future 膨胀会挤掉 opus 主栈)
    // 采集未 start; flash 交出后等 done 再用; 成功路径任务内 reset
    let ota_done = xiaozhi_s3::mk_static!(
        Channel<embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex, (), 1>,
        Channel::new()
    );
    let ota_token = xiaozhi_s3::ota_check::boot_ota_task(
        stack,
        server,
        mac.clone(),
        xiaozhi_s3::ota_check::FlashPtr(flash as *mut _),
        ota_done.sender(),
    )
    .unwrap();
    spawner.spawn(ota_token);
    ota_done.receiver().receive().await;
    // flash 指针交还: OTA 任务已结束 (或已 reset), main 可继续用

    // 会话常驻缓冲 (帧长/常量与 app.rs 对齐: 960 样本 @16kHz 60ms)
    // 堆分配而非 mk_static: 大数组进 bss 会把主栈压到 <87KB (opus SILK)
    // 大块 CPU 缓冲放 PSRAM: 内部堆只有 128KB, 不能把 40KB 常驻缓冲留在内部
    let pop_buf: &'static mut [u8] = psram_bytes(POP_BUF_SIZE);
    let mono_buf: &'static mut [i16] = psram_i16(POP_BUF_SIZE / 4);
    let acc_buf: &'static mut [i16] = psram_i16(1920);
    let silence_buf: &'static mut [u8] = psram_bytes(BLOCK_BYTES);

    // wakenet 自检 (feature `wakenet`): 强制链接 esp-sr 符号闭包
    #[cfg(feature = "wakenet")]
    match xiaozhi_s3::audio::wake_net::wakenet_selftest() {
        Ok(()) => info!("wakenet 自检通过"),
        Err(e) => error!("wakenet 自检失败: {}", e),
    }

    // 开机预热: 先让 I2S/DAC 输出 300ms 静音, 稳定 Codec/功放后再出声
    // (reset/刷机后的第一轮播报杂音即上电瞬态)
    embassy_time::Timer::after(embassy_time::Duration::from_millis(300)).await;
    // 开机画面 + 提示音
    board.display.draw_status("XIAOZHI S3", Rgb565::WHITE);
    board.audio.beep(150);
    // 提示音播完 (~150ms + 环形延迟) 再恢复用户音量
    embassy_time::Timer::after(embassy_time::Duration::from_millis(400)).await;
    board.audio.set_volume(saved_vol);

    // 传输方式: OTA 下发过 MQTT 网关配置 → MQTT+UDP, 否则 WebSocket
    // embassy_executor::task: 调用返回 SpawnToken, 必须交给 spawner.spawn
    // (丢弃 token 会 panic)
    let mqtt_cfg = store::load_mqtt(flash);
    // UDP 通道恒带时间戳, 服务端 AEC 可用 → 播报中持续上行
    server_aec |= mqtt_cfg.is_some();
    if server_aec {
        info!("服务端 AEC: 开 (上行帧携带时间戳)");
    }
    if let Some(cfg) = mqtt_cfg {
        info!(
            "传输: MQTT+UDP ({} → {})",
            cfg.endpoint.as_str(),
            cfg.subscribe_topic.as_str()
        );
        let token = xiaozhi_s3::mqtt_session::mqtt_session_task(
            xiaozhi_s3::mqtt_session::MqttSessionArgs {
                stack,
                config: cfg,
                channels,
            },
        )
        .unwrap();
        spawner.spawn(token);
    } else {
        // 会话任务 (WS 连接/收发; 长阻塞 await 集中于此)
        let token = app::session_task(app::SessionArgs {
            stack,
            server,
            mac: mac.clone(),
            channels,
            ws_version,
        })
        .unwrap();
        spawner.spawn(token);
    }

    // 看门狗: OTA/配网之后启用, 主循环每轮喂狗 (卡死 8s 硬复位)
    xiaozhi_s3::wdt::init(mwdt);
    #[cfg(feature = "watchdog-selftest")]
    {
        error!("看门狗自测: 阻塞主循环 12s, 8s 内应复位");
        esp_hal::delay::Delay::new().delay_millis(12_000);
    }

    // 采集常驻 + 主循环 (采集故障 → 直线路径重建 → 重进)
    let mut cap = match board.capture.start() {
        Ok(t) => t,
        Err(e) => {
            error!("采集启动失败: {:?}", e);
            esp_hal::system::software_reset()
        },
    };
    loop {
        let outcome = app::run(
            AppArgs {
                audio: &mut board.audio,
                buttons: &mut board.buttons,
                leds: &mut board.leds,
                display: &mut board.display,
                battery: &mut board.battery,
                power: &mut board.power,
                cap: &mut cap,
                channels,
                flash,
                server_aec,
            },
            &mut pop_buf[..],
            &mut mono_buf[..],
            &mut acc_buf[..],
            &mut silence_buf[..],
        )
        .await;
        // app::run 仅在采集故障时返回: 直线路径 drop→重建 (async 借用约束)
        debug_assert_eq!(outcome, app::RunOutcome::CaptureFault);
        drop(cap);
        cap = match board.capture.start() {
            Ok(t) => t,
            Err(e) => {
                error!("采集重建失败: {:?}", e);
                esp_hal::system::software_reset()
            },
        };
    }
}
