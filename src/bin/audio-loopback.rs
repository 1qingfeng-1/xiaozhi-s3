//! M1 调试工具: 录音→播放回环 (音频全双工通路验证)
//!
//! 数据流: I2S RX (2ch 立体声, MIC1=左槽) → 提取 MIC1 → 软件增益 →
//! 复制为立体声 → I2S TX (ES8311 → PA → 喇叭)。
//!
//! ⚠️ 喇叭与麦克风距离过近会啸叫: 回放音量压低 (35%), 必要时移开设备。
//!
//! ```bash
//! cargo run --release --bin audio-loopback
//! ```

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use embassy_executor::Spawner;
use esp_hal::{
    clock::CpuClock,
    time::{Duration, Instant},
};

use defmt::{error, info};
use esp_println as _;

use xiaozhi_core::audio::capture::{apply_gain_percent, extract_slot, is_voice_active};

use xiaozhi_s3::bsp::create_board;

#[panic_handler]
fn panic(panic_info: &core::panic::PanicInfo) -> ! {
    error!("{}", panic_info);
    // 打印后软复位, 避免 loop{} 卡死掩盖问题 (调试期现场自动恢复)
    esp_hal::system::software_reset()
}

extern crate alloc;

// This creates a default app-descriptor required by the esp-idf bootloader.
esp_bootloader_esp_idf::esp_app_desc!();

/// 乒乓窗口 = 单个 DMA 描述符 (4092B ≈ 64ms): esp-hal 的 available() 按完整
/// 描述符计数, 窗口必须与描述符等大, 否则永远凑不满一个描述符
const BLOCK_BYTES: usize = 4092;
const BLOCK_FRAMES: usize = BLOCK_BYTES / 4;
/// 软件增益百分比 (100 = 0dB): 麦克风数字电平约 1~5% 满幅 (RMS 300~1500),
/// 需 +18dB 左右才能在回放端听清; 饱和处理防溢出
const GAIN_PERCENT: u16 = 800;
/// VAD 均方幅度阈值 (采集增益 30dB 下的经验值, 仅日志用)
const VAD_THRESHOLD: i16 = 300;
/// pop 缓冲上限 16KB; 积压超过此值说明处理跟不上, 重启采集丢弃积压
const POP_BUF_SIZE: usize = 16 * 1024;

#[esp_rtos::main]
#[allow(clippy::large_stack_frames)] // 调试 bin: 单任务局部缓冲较大, 可接受 (CI 以 cap-lints 压住)
async fn main(_spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    info!("=== 音频回环调试 (M1) ===");

    let mut board = create_board(peripherals);

    // esp-radio 已无条件入链: 调度器必须启动 (外设经 steal 重取)
    let p = unsafe { esp_hal::peripherals::Peripherals::steal() };
    let timg0 = esp_hal::timer::timg::TimerGroup::new(p.TIMG0);
    let sw_interrupt = esp_hal::interrupt::software::SoftwareInterruptControl::new(p.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);
    info!("板子 [{}] 初始化完成", board.name);

    // 回放音量: ES8311 REG32 为 dB 线性映射, 35% = -51dB 几乎不可闻;
    // 60% ≈ -19.5dB (与开机 beep 同级, 已验证可闻)。啸叫则调低。
    board.audio.set_volume(60);
    info!(
        "回环: MIC1→喇叭, 块 {}ms, 增益 {}%, 音量 {}%",
        BLOCK_FRAMES * 1000 / 16_000,
        GAIN_PERCENT,
        board.audio.volume()
    );

    // pop/处理/回放缓冲走堆 (合计 >24KB, 超过 clippy large_stack_frames 限制)
    let pop_buf: &'static mut [u8] =
        alloc::boxed::Box::leak(alloc::vec![0u8; POP_BUF_SIZE].into_boxed_slice());
    let out_buf: &'static mut [u8] =
        alloc::boxed::Box::leak(alloc::vec![0u8; POP_BUF_SIZE].into_boxed_slice());
    let mono_buf: &'static mut [i16] =
        alloc::boxed::Box::leak(alloc::vec![0i16; POP_BUF_SIZE / 4].into_boxed_slice());

    // 时钟维持静音窗: RX 从机跟随 TX 时钟, TX 不跑则采集停滞。
    // 采集不足一个描述符时补播静音, 既维持时钟又顺带推进采集。
    let silence_buf: &'static mut [u8] =
        alloc::boxed::Box::leak(alloc::vec![0u8; BLOCK_BYTES].into_boxed_slice());

    let mut rx = match board.capture.start() {
        Ok(t) => t,
        Err(e) => {
            error!("采集启动失败: {:?}", e);
            esp_hal::system::software_reset()
        },
    };

    let mut last_voice: Option<bool> = None;
    let mut blocks: u64 = 0;

    info!("开始回环, 对着麦克风说话...");

    loop {
        // 等待采集数据 (两臂都返回, 无需 loop)
        let avail = match rx.available() {
            Ok(n) => n,
            Err(e) => {
                error!("RX available 失败: {:?}, 重启采集", e);
                drop(rx); // 先释放旧借用, 再重启采集
                rx = match board.capture.start() {
                    Ok(t) => t,
                    Err(e) => {
                        error!("采集重启失败: {:?}", e);
                        esp_hal::system::software_reset()
                    },
                };
                0
            },
        };

        // 积压超过 pop 缓冲: 处理跟不上, 重启采集丢弃积压 (重新对齐)
        if avail > POP_BUF_SIZE {
            error!("采集积压 {}B > {}B, 重启采集", avail, POP_BUF_SIZE);
            drop(rx);
            rx = match board.capture.start() {
                Ok(t) => t,
                Err(e) => {
                    error!("采集重启失败: {:?}", e);
                    esp_hal::system::software_reset()
                },
            };
            continue;
        }

        if avail >= BLOCK_BYTES {
            // pop 全部积压 (available ≤ pop_buf, 满足 pop 的 all-or-nothing 语义)
            match rx.pop(&mut pop_buf[..avail]) {
                Ok(n) => {
                    let frames = n / 4;
                    blocks += 1;
                    if blocks.is_multiple_of(50) {
                        info!("心跳: 已处理 {} 块 (约 {}ms)", blocks, blocks * 64);
                    }
                    // 提取 MIC1 (左槽) → 增益
                    let n_samples = extract_slot(&pop_buf[..frames * 4], 2, 0, mono_buf);
                    if n_samples == 0 {
                        continue;
                    }
                    apply_gain_percent(&mut mono_buf[..n_samples], GAIN_PERCENT);

                    // VAD 状态变化时打日志 (粗判, 供半双工状态机参考)
                    let voice = is_voice_active(&mono_buf[..n_samples], VAD_THRESHOLD);
                    if last_voice != Some(voice) {
                        info!(
                            "VAD: {} (块 {})",
                            if voice { "有声" } else { "静音" },
                            blocks
                        );
                        last_voice = Some(voice);
                    }

                    // 单声道复制为立体声并回放 (播放即维持时钟, 推进下一窗采集)
                    for (i, s) in mono_buf[..n_samples].iter().enumerate() {
                        let le = s.to_le_bytes();
                        out_buf[i * 4..i * 4 + 2].copy_from_slice(&le);
                        out_buf[i * 4 + 2..i * 4 + 4].copy_from_slice(&le);
                    }
                    board.audio.play_block(&out_buf[..n_samples * 4]);
                },
                Err(e) => error!("RX pop 失败: {:?}", e),
            }
        } else {
            // 时钟维持: 补播一段静音 (每次一次性 TX 重启约有 28B 采集损耗,
            // 由下次补播自然补回, 不影响回环正确性)
            board.audio.play_block(silence_buf);
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_millis(60) {}
        }
    }
}
