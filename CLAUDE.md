# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository. <!-- markdownlint-disable-line MD013 -->

> 仓库根目录的 `AGENTS.md` 是完整主指南（分层规则、坑位、M10/OTA 细节、真机验证记录），
> **改代码前必读**；本文件是面向 Claude Code 的速查版，冲突时以 `AGENTS.md` 为准。
> 对话回复、代码注释、文档、commit message 一律用中文。

## 项目概览

ESP32-S3 智能音箱固件（Rust `no_std` + esp-hal v1.1 + esp-rtos/embassy-net）。
开发板 gezipai：ES8311 (DAC) + ES7210 (ADC/麦克风) + ST7789 (显示)，ESP32-S3R8，
16MB Flash / 8MB Octal PSRAM。全语音链路已接通：VAD 唤醒 → WebSocket 会话
（xiaozhi 协议 v1，二进制帧 = 裸 opus）→ TTS 播放/打断；另含配网、MCP、OTA、显示、电池。

## 常用命令

```bash
# 固件（父仓库 .cargo/config.toml 已锁 xtensa target + build-std，直接 cargo）
cargo build --release
cargo run --release --bin xiaozhi-s3      # espflash 烧录 + defmt 监视
cargo run --release --bin audio-loopback  # 其他调试 bin: wifi-test / gpio-test / i2c-scan

# 门禁（顺序与 pre-commit/CI 一致）
cargo fmt --all -- --check
cargo clippy --all-features -- -D warnings --cap-lints allow
cargo check --all
```

`rust-toolchain.toml` 指定 `esp` 工具链；**host 侧命令必须显式 `+stable`**。

### 单元测试（只有 xiaozhi-core 有测试，且只能在 host 跑）

```bash
cd xiaozhi-core && cargo +stable nextest run          # 156 个，全绿
cd xiaozhi-core && cargo +stable nextest run ota_crc  # 按名过滤单个测试
cd xiaozhi-core && cargo +stable test                 # 无 nextest 时
```

必须 `cd xiaozhi-core`：父仓库 `.cargo/config.toml` 把 target 锁到 xtensa，
从根目录跑会拿父配置编 host 依赖（`E0463: can't find crate for core`）。
**不要在仓库根目录跑 `cargo test`**（作用于 xtensa target，必失败）。

### 崩溃定位

```bash
xtensa-esp32s3-elf-addr2line -pfiaC \
  -e target/xtensa-esp32s3-none-elf/release/xiaozhi-s3 <bt 地址...>
```

## 架构（大图）

```text
xiaozhi-s3/            固件 crate (no_std)
├── src/bin/main.rs    main(): create_board → WiFi/配网 → OTA 任务 → app::run()
├── src/app.rs         双任务编排 + 音频主循环（对话/唤醒/播放/灯效/按键）
├── src/session.rs     WS 传输层（手帧编解码、hello、ping/pong、:8000 /xiaozhi/v1/）
├── src/mqtt_session.rs MQTT+UDP 传输（MQTT 控制 + UDP/AES-128-CTR 音频，与 WS 二选一）
├── src/wdt.rs         主循环看门狗（TIMG0 MWDT，8s 未喂狗硬复位）
├── src/ota_check.rs   OTA 任务（POST :8003 检查 → 下载 → 4KB 擦写 → otadata → 复位）
├── src/wifi/          STA 连接 + SoftAP 配网 + NVS 凭据/音量 (esp-storage)
├── src/audio/         opus FFI 安全封装 + wakenet (esp-sr, feature 门控)
├── src/bsp/           板级支持：board.rs 契约 / components/ 组件 / gezipai.rs 工厂
└── xiaozhi-core/      git submodule：纯逻辑层（host 可测，禁止硬件依赖）
```

### 双任务模型

- **音频主循环**（`app::run`）非阻塞：每轮排空 I2S RX、跑 VAD/状态机、上行编码、
  下行解码播放、维持 TX 时钟，然后 `Timer::after` 让出 CPU（活跃 1ms / Idle 10ms）。
- **会话任务**（`app::session_task`）承担所有长阻塞 await（TCP 建连、WS 收发），
  通过 `Channels`（`mk_static!` 常驻，uplink/downlink/command/event）与主循环通信。
  任务参数必须 `'static`，`spawner.spawn` 返回的 token 不可丢弃。

### 数据通路

I2S 全双工：TX 为主机并经 `signal_loopback` 把 BCLK/WS 共享给 RX；**RX 从机依赖 TX 时钟**，
所以 TX 必须常流（空闲时主循环补静音维持时钟）。采集排空是**全量语义**（pop 必须容纳
整条 RX 链，`POP_BUF_SIZE = 8×4092`），时钟运行期间每轮必须排空，否则 `DmaError::Late`
且内部计数损坏，只能丢弃句柄重建采集（仅允许在 main 直线路径重建）。

上行：16kHz/60ms 单声道 opus，VAD 在**原始电平域**判定；软件增益后限幅再编码。
下行：服务器 24kHz TTS 由 libopus 解码器内部重采样到 16kHz，每轮 ≤3 帧防 RX 回积压。

### 纯逻辑与硬件分层

- `xiaozhi-core`（`#![deny(unsafe_code)]`）只放可分类/可计算逻辑：对话状态机
  `dialog`、VAD/增益/槽位提取 `audio::capture`、codec 寄存器序列、OTA 清单/版本/
  otadata 编解码、MCP JSON-RPC 解析、显示与板级规则。**禁止引入 esp-hal**。
- 固件侧只做 I2S/I2C、DMA、Flash、网络等硬件操作；状态机只输出 `Action`，
  固件在 `app::dispatch` 里落实副作用。

### BSP

`BoardConfig` 是编译期常量（`src/bsp/gezipai.rs` 为引脚唯一事实来源），
`create_board()` 组装运行时 `Board`（audio/display/leds/buttons/power/battery）。
应用与驱动只读 `bsp::active::BOARD` 和 `Board` 组件 API，**禁止硬编码 GPIO/I2C 地址**；
硬接线 `const _: () = assert!` 只允许出现在板子工厂内。新增板子见 `src/bsp/mod.rs` 四步。

### OTA / Flash 布局

无 factory 分区，双 app 乒乓：`ota_0 @0x20000` / `ota_1 @0x410000`（各 0x3F0000），
`otadata @0x10000` 两条各占一扇区（4KB 间距），CRC 为 reflected CRC-32。
NVS 分区内约定：WiFi 凭据 `0xE000`、音量 `0xD000`、OTA 失败计数 `0xC000`、
MQTT 网关配置 `0xA000`（OTA 下发）、WS 协议版本 `0xB000`。
OTA 在独立 `boot_ota_task` 中执行（不内联 main，否则 main future 膨胀压垮主栈），
**flash 擦写绝不能放进音频主循环或 `session_task`**（I2S RX DMA 会溢出）。

## 内存与资源硬约束

- 内部堆 **128KB**，主任务栈 ≈ RWDATA 剩余（Channels 迁 PSRAM 后实测 ~100KB，
  CI 下限 96KiB）；opus 编码 60ms 帧需 **~87KB 栈**（SILK VLA 走任务栈）。
  改动堆大小或帧长前必须重估。
- 音频 DMA 缓冲必须在**内部 RAM**；`psram_allocator!` 必须在 DMA 缓冲分配之后注册
  （`create_board` 末尾），否则 I2S 报 `DmaError(UnsupportedMemoryRegion)`。
- embassy-net `StackResources` 按每张表计数，超容 `TcpSocket::new` 会 panic 重启：
  STA `<6>`（DHCP+OTA+会话 TCP+UDP 音频）、SoftAP 配网 `<8>`。socket 跨重连复用，勿每次新建。
- MQTT/UDP 的大缓冲（TCP 收发、包解析、UDP 收发）必须走 PSRAM；留在内部会把
  128KB 堆推到 95%+（真机实测）。
- S3 WiFi PHY 与 SARADC 共用：电池 ADC（GPIO3）只在 **Idle 且 10s 限频**时读，
  会话/播报期读会干扰 PHY 导致 WiFi blob 崩溃。
- 单个 opus 包上限 1276B；通道容量 uplink 12 / downlink 6 / command 4 / event 4。

## 约定

- Conventional Commits，中文 commit message；changelog 由 git-cliff（`cliff.toml`）生成。
- pre-commit：fmt / clippy / check / cargo-deny / typos / markdownlint；`vendor/` 整体排除
  （三方代码保持上游原样）。
- `xiaozhi-core` 是 submodule：core 改动先在子仓库提交推送，再在父仓库更新指针。
- Release：推 `v[0-9]+.*` tag 触发 `.github/workflows/release.yml`。

## 文档索引

- `AGENTS.md` — 完整工程指南（分层、坑位、真机验证记录），**优先阅读**
- `README.md` — 硬件与引脚映射
- `docs/ROADMAP.md` — 阶段计划与真机验证清单（下一步上机项以此为准）
- `docs/MQTT_UDP.md` — MQTT 网关部署、UDP 加密/时间戳、真机验证记录
- `docs/PORTING.md` — 同芯片换板四步法；C3/C6/P4 移植差异清单
- `docs/DEVELOPMENT.md` / `docs/QUICK_START.md` — 开发流程
- C++ 参考实现：`/home/mrpeng/Documents/xiaozhi-esp32`（ESP-IDF，板级/协议/OTA 对照物；
  移植到 esp-hal 时 Register 层以 `xiaozhi-core` 序列为准）
