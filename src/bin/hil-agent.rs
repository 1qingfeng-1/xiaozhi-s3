//! HIL 控制代理 — 真机自动化测试的控制通道 (feature = "hil")
//!
//! 与主固件互斥: 通过 USB Serial/JTAG 接收 `core::hil` 定义的命令,
//! 回写稳定的 `HIL key=value` 行。音频主链路不在此 bin 内 (避免和
//! 主固件争用 I2S), 因此 `CAPTURE`/`WAKE`/`BARGE_IN` 暂返回 not_implemented。
//!
//! ```bash
//! cargo run --release --features hil --bin hil-agent
//! ```
//!
//! 设计见 `docs/HIL.md`; 命令解析/事件格式化在 `xiaozhi-core::hil`。

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

use core::fmt::Write;

use defmt::info;
use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_hal::{
    clock::CpuClock, interrupt::software::SoftwareInterruptControl, time::Instant,
    timer::timg::TimerGroup, usb_serial_jtag::UsbSerialJtag,
};
use esp_println as _;

use xiaozhi_core::hil::{Command, Event, format_event, parse_command};
use xiaozhi_s3::bsp::{Board, create_board};
use xiaozhi_s3::mem::psram_available;

#[panic_handler]
fn panic(panic_info: &core::panic::PanicInfo) -> ! {
    defmt::error!("{}", panic_info);
    esp_hal::system::software_reset()
}

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

#[derive(Default)]
struct Counters {
    commands: u32,
    errors: u32,
    /// SIM_LOW_BATTERY 注入值 (未注入时为 None)
    battery: Option<u8>,
}

#[esp_rtos::main]
async fn main(_spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    let (mut board, extras) = create_board(peripherals);

    // esp-radio 已无条件入链: 调度器必须启动
    let timg0 = TimerGroup::new(extras.timg0);
    let sw_interrupt = SoftwareInterruptControl::new(extras.sw_interrupt);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    let mut usb = UsbSerialJtag::new(extras.usb_device);
    board.audio.set_volume(60);
    info!("HIL agent ready ({}), USB Serial/JTAG", board.name);

    let boot = Event::Boot {
        version: env!("CARGO_PKG_VERSION"),
        sha: option_env!("GIT_SHA").unwrap_or("unknown"),
        // ESP32-S3R8 固定 8MB Octal PSRAM; 不可用时如实报 0
        psram_bytes: if psram_available() {
            8 * 1024 * 1024
        } else {
            0
        },
        reset: "UNKNOWN",
    };
    if let Some(line) = format_event(&boot) {
        let _ = write!(&mut usb, "{line}\r\n");
    }

    let mut line = heapless::String::<96>::new();
    let mut counters = Counters::default();

    loop {
        match usb.read_byte() {
            Ok(byte) => {
                if byte == b'\n' {
                    let text = line.as_str();
                    if !text.is_empty() {
                        counters.commands += 1;
                        match parse_command(text) {
                            Some(cmd) => {
                                if handle(cmd, &mut board, &mut usb, &mut counters) {
                                    let _ = write!(&mut usb, "HIL ok reboot\r\n");
                                    Timer::after(Duration::from_millis(100)).await;
                                    esp_hal::system::software_reset();
                                }
                            },
                            None => {
                                counters.errors += 1;
                                let _ = write!(&mut usb, "HIL error reason=bad_command\r\n");
                            },
                        }
                    }
                    line.clear();
                } else if byte != b'\r' && line.push(byte as char).is_err() {
                    line.clear();
                    counters.errors += 1;
                    let _ = write!(&mut usb, "HIL error reason=line_too_long\r\n");
                }
            },
            // WouldBlock/错误都退避 2ms; HIL agent 不跑音频, 轮询足够
            Err(_) => Timer::after(Duration::from_millis(2)).await,
        }
    }
}

/// 执行命令; 返回 true 表示请求复位
fn handle<W: Write>(cmd: Command, board: &mut Board, out: &mut W, c: &mut Counters) -> bool {
    match cmd {
        Command::Ping => {
            let _ = write!(out, "HIL pong\r\n");
        },
        Command::Version => {
            let _ = write!(out, "HIL version={}\r\n", env!("CARGO_PKG_VERSION"));
        },
        Command::GetCounters => {
            let uptime_ms = Instant::now().duration_since_epoch().as_millis();
            match c.battery {
                Some(p) => {
                    let _ = write!(
                        out,
                        "HIL counters commands={} errors={} uptime_ms={} battery={p}\r\n",
                        c.commands, c.errors, uptime_ms
                    );
                },
                None => {
                    let _ = write!(
                        out,
                        "HIL counters commands={} errors={} uptime_ms={} battery=none\r\n",
                        c.commands, c.errors, uptime_ms
                    );
                },
            }
        },
        Command::ResetCounters => {
            c.commands = 0;
            c.errors = 0;
            c.battery = None;
            let _ = write!(out, "HIL ok reset\r\n");
        },
        Command::SetVolume(percent) => {
            board.audio.set_volume(percent);
            let _ = write!(out, "HIL ok volume={percent}\r\n");
        },
        Command::PlayTone {
            freq_hz,
            duration_ms,
        } => {
            board.audio.set_pa(true);
            board.audio.beep_hz(freq_hz, duration_ms);
            let _ = write!(out, "HIL ok tone_hz={freq_hz} tone_ms={duration_ms}\r\n");
        },
        Command::SimLowBattery { percent } => {
            c.battery = Some(percent);
            let _ = write!(out, "HIL ok battery={percent}\r\n");
        },
        Command::Reboot => return true,
        Command::Capture { .. } | Command::Wake | Command::BargeIn => {
            c.errors += 1;
            let _ = write!(out, "HIL error reason=not_implemented\r\n");
        },
    }
    false
}
