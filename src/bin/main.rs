//! 主入口 — 智能音箱产品固件
//!
//! 流程: create_board (音频+PSRAM) → esp-rtos → 读凭据 (无则配网) →
//! WiFi 连接 → 主应用循环 (唤醒监听 / 对话 / 播报 / 打断)。
//!
//! 采集常驻运行; RX 饱和故障时在直线路径重建句柄后重进主循环
//! (async 生成器里条件路径的 drop→重启借用检查过不去, 见 AGENTS.md)。

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

use embassy_executor::Spawner;
use esp_hal::{
    clock::CpuClock, interrupt::software::SoftwareInterruptControl, peripherals::Peripherals,
    timer::timg::TimerGroup,
};
use esp_storage::FlashStorage;

use defmt::{error, info};
use esp_println as _;

use xiaozhi_s3::app::{self, AppArgs};
use xiaozhi_s3::bsp::components::Rgb565;
use xiaozhi_s3::bsp::create_board;
use xiaozhi_s3::ota_check;
use xiaozhi_s3::wifi::{self, Mode, store};

#[panic_handler]
fn panic(panic_info: &core::panic::PanicInfo) -> ! {
    error!("{}", panic_info);
    esp_hal::system::software_reset()
}

extern crate alloc;

// This creates a default app-descriptor required by the esp-idf bootloader.
esp_bootloader_esp_idf::esp_app_desc!();

/// 会话常驻缓冲尺寸
const POP_BUF_SIZE: usize = 16 * 1024;
/// 单 DMA 描述符字节数 (4092B ≈ 64ms @ 16kHz 立体声)
const BLOCK_BYTES: usize = 4092;

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    info!("=== 小智音箱启动 ===");

    // 板子工厂: 内部堆第一行注册 → 音频 DMA 缓冲 → PSRAM 最后注册
    let mut board = create_board(peripherals);
    info!("板子 [{}] 初始化完成", board.name);

    // create_board 按值消耗了 Peripherals; 音频不用的 WIFI/FLASH/TIMG0 经 steal 再取
    let p = unsafe { Peripherals::steal() };
    let timg0 = TimerGroup::new(p.TIMG0);
    let sw_interrupt = SoftwareInterruptControl::new(p.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    let flash = xiaozhi_s3::mk_static!(FlashStorage<'static>, FlashStorage::new(p.FLASH));

    // BOOT 键开机按住: 清除凭据强制重回配网
    if board.buttons.boot_pressed() {
        info!("BOOT 按下: 清除 WiFi 配置...");
        let _ = store::clear(flash);
        embassy_time::Timer::after(embassy_time::Duration::from_millis(200)).await;
    }

    // WiFi: 有凭据连 STA; 无凭据/连接失败 → 配网模式 (SoftAP 表单, 保存后重启)
    let (stack, server, _controller, mac) = match wifi::boot_mode(flash) {
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
    info!("设备 ID: {}", mac.as_str());

    // 会话常驻缓冲 (帧长/常量与 app.rs 对齐: 960 样本 @16kHz 60ms)
    let pop_buf = xiaozhi_s3::mk_static!([u8; POP_BUF_SIZE], [0u8; POP_BUF_SIZE]);
    let mono_buf = xiaozhi_s3::mk_static!([i16; POP_BUF_SIZE / 4], [0i16; POP_BUF_SIZE / 4]);
    let acc_buf = xiaozhi_s3::mk_static!([i16; 1920], [0i16; 1920]);
    let silence_buf = xiaozhi_s3::mk_static!([u8; BLOCK_BYTES], [0u8; BLOCK_BYTES]);

    // 开机画面 + 提示音
    board.display.draw_status("XIAOZHI S3", Rgb565::WHITE);
    board.audio.beep(150);

    // OTA 版本检查 (仅上报, 下载烧写待分区表改造)
    match ota_check::check(stack, server).await {
        Some(report) if report.update_available => {
            info!("OTA: 新版本 {} 可升级", report.latest_version.as_str());
        },
        _ => {},
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
                stack,
                server,
                mac: mac.clone(),
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
