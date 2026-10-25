# 小智音箱 (ESP32-S3)

[![CI](https://github.com/1qingfeng-1/xiaozhi-s3/actions/workflows/ci.yml/badge.svg)](https://github.com/1qingfeng-1/xiaozhi-s3/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

基于 ESP32-S3-AI-2 模组的智能音箱，使用 Rust 开发。

## 硬件

- **主控**: ESP32-S3R8 (160MHz, 512KB SRAM)
- **Flash**: 16MB
- **音频Codec**: ES8311 (I2S + I2C)
- **麦克风阵列**: ES7210 (4通道)
- **功放**: LPA2103A (2W/4Ω)
- **充电**: LGS4056HDA (1A)
- **尺寸**: 19.5mm × 18mm

引脚与硬件细节见下方[引脚映射](#引脚映射)，唯一事实来源为
[`src/bsp/gezipai.rs`](src/bsp/gezipai.rs)。

## 项目结构

```text
xiaozhi-s3/
├── src/
│   ├── lib.rs          # 库定义
│   ├── bsp/            # 板级支持包 (编译时选板, 参照 xiaozhi-esp32)
│   │   ├── mod.rs      # 选板守卫 + active/create_board 转发
│   │   ├── board.rs    # BoardConfig 契约 (按功能细分子 struct)
│   │   ├── components/ # 运行时组件目录 (组合成 Board)
│   │   │   ├── mod.rs  # Board 组合对象 + 组件导出
│   │   │   ├── leds.rs / buttons.rs / power.rs / battery.rs
│   │   └── gezipai.rs  # gezipai 板: BOARD 配置 + create_board 工厂
│   ├── audio/          # 音频模块 (esp-hal 薄封装)
│   └── bin/
│       └── main.rs     # 主入口
├── xiaozhi-core/       # 纯逻辑核心层 (git submodule, host 可测试)
├── .github/
│   └── workflows/
│       ├── ci.yml      # CI 工作流 (含 core-test job)
│       └── release.yml # Release 工作流
├── .cargo/
│   └── config.toml     # ESP 工具链配置 (-nostartfiles 为 per-target)
├── docs/               # 文档目录
│   ├── README.md       # 文档导航
│   ├── DEVELOPMENT.md  # 开发指南
│   ├── QUICK_START.md  # 快速开始
│   ├── CI.md           # 门禁 / CI / 发布流程
│   └── adr/            # 架构决策记录
├── rust-toolchain.toml # 指定 esp 工具链
├── build.rs            # 构建脚本
└── Cargo.toml
```

### 板级选择 (BSP)

参照 [xiaozhi-esp32](https://github.com/78/xiaozhi-esp32) 的编译时选板架构，
Rust 版用 cargo feature 实现同等能力：

| xiaozhi-esp32 (C++)           | xiaozhi-s3 (Rust)              |
|-------------------------------|--------------------------------|
| `main/boards/<board>/config.h` | `src/bsp/<board>.rs` 的 `BOARD` 常量 |
| Kconfig `CONFIG_BOARD_TYPE_X` | cargo feature `board-x`        |
| `Board` 基类 + GetXxx() 虚函数 | `board.rs` 的 `BoardConfig` 与组合式 `Board` |
| 子类 override GetXxx()        | 各板 `create_board()` 工厂组装组件 |
| `DECLARE_BOARD(XxxBoard)` 宏  | feature 门控的 `create_board` 转发 |
| `GetInstance()` 单例          | main 创建一次, `&mut Board` 传递 |

**组合优先于继承**: `BoardConfig`(纯数据, 编译期常量, 按功能细分 audio/codec_ctrl/
buttons/power/leds/display) + `Board`(运行时组件集合, `components.rs`)。
`BoardConfig` 中 Codec 地址**按 dac/adc 角色命名，与芯片型号解耦**（型号仅作日志描述字符串），
上层驱动只认角色 (`BOARD.codec_ctrl.dac_addr`)，换 Codec 型号零改动。

```rust
// main.rs — 应用只面向组件, 不接触 GPIO
let mut board = xiaozhi_s3::bsp::create_board(peripherals);
board.leds.on();
if board.buttons.boot_pressed() { /* ... */ }
let st = board.power.status();          // 分类逻辑在 xiaozhi-core (host 单测)
```

```bash
cargo +esp build --release                                  # 默认板 gezipai
cargo +esp build --no-default-features --features board-x   # 其他板
```

新增板子：复制任一 `src/bsp/<board>.rs` → 填写 `BOARD` 配置 + 实现 `create_board()`
工厂（含硬接线编译期断言）→ `src/bsp/mod.rs` 注册两行 → `Cargo.toml [features]` 加一行。
驱动层只允许消费 `BOARD` 配置与 `Board` 组件 API，禁止硬编码 GPIO。

> **重要更正**: 此前充电脚误判为 GPIO12/13。官方配置实为
> **CHARGING=GPIO41 / DONE=GPIO42**（GPIO12/13 是灯效串口 LIGHT_RX/TX）；
> I2S 数据线 **DIN=GPIO21(录音 RX) / DOUT=GPIO48(播放 TX)**；
> 功放 PA 使能=**GPIO46**；I2C SDA/SCL=**GPIO38/GPIO47**（已硬件扫描验证双 Codec 在线）。

## 开发环境

### 1. 安装 Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### 2. 安装 ESP 工具链

```bash
cargo install espup --locked
espup install
source ~/.espup/export-esp.sh
```

### 3. 构建

```bash
cd xiaozhi-s3
cargo build --release
```

### 4. 烧录

```bash
# 构建 + 烧录 + defmt 监视
cargo run --release

# 或烧录 release 产物中的应用镜像（写入 ota_0）
espflash write-bin 0x20000 xiaozhi-s3-<tag>.bin

# 首次全量烧录可用整片镜像
espflash write-bin 0x0 xiaozhi-s3-<tag>-merged.bin
```

## xiaozhi-core 纯逻辑核心层

`xiaozhi-core` 是独立的纯逻辑 crate（git submodule），与硬件无关，**可在 host (x86_64) 上单元测试**，便于在硬件到货前开发音频逻辑。

包含：

- **音量映射**: 用户百分比 (0~100) ↔ ES8311 寄存器值 (0~63)
- **时钟计算**: I2S BCLK/MCLK 频率、分频系数
- **Codec 寄存器序列**: ES8311 (输出) / ES7210 (输入) 上电初始化序列
- **音频状态机**: Idle/Initializing/Ready/Active/Paused/Error 状态转移

### 运行单元测试

```bash
cd xiaozhi-core
cargo +stable nextest run     # 推荐 (并行, 197 个测试)
# 或
cargo +stable test
```

> **注意**: 必须用 `+stable`（host 工具链）。`+esp` 是 no_std xtensa 工具链，无 host 的 std/test crate，无法跑测试。
> 不要在 esp 仓库根目录跑 `cargo test`（会作用到 xtensa target 而失败），必须 `cd xiaozhi-core`。

### 克隆仓库（含 submodule）

```bash
git clone --recurse-submodules <repo-url>
# 或已克隆后
git submodule update --init --recursive
```

## 引脚映射

| 功能 | GPIO | 说明 |
|------|------|------|
| USB D- | 19 | USB通信 (WROOM-1U 引脚35) |
| USB D+ | 20 | USB通信 (WROOM-1U 引脚36) |
| UART0 TX | 43 | 调试串口 (引脚6) |
| UART0 RX | 44 | 调试串口 (引脚7) |
| LED 电源 | 4 | 红色 (引脚20) |
| LED 呼吸 | 17 | 白色, 下拉亮/上拉熄 (规格书 IO17) |
| BOOT 键 | 1 | 官方 BOOT_BUTTON=1 (已验证可检测) |
| 音量+键 | 18 | 官方 VOLUME_UP=18 (已验证可检测) |
| 音量-键 | 2 | 官方 VOLUME_DOWN=2 (已验证可检测) |
| Codec I2C SDA | 38 | ES8311+ES7210 共享总线 (I2C 扫描已验证) |
| Codec I2C SCL | 47 | 同上 (已验证) |
| ES8311 DAC 地址 | - | 0x18 (应答正常) |
| ES7210 ADC 地址 | - | 0x40 (应答正常) |
| I2S MCLK | 40 | 主时钟 |
| I2S BCLK | 39 | 位时钟 |
| I2S WS/LRCK | 45 | 字选择 |
| I2S DIN | 21 | 录音数据输入 (麦克风→ESP32) |
| I2S DOUT | 48 | 播放数据输出 (ESP32→DAC) |
| 功放 PA 使能 | 46 | LPA2103A, 高电平开启 |
| 充电中 (CHARGING) | 41 | 低电平=充电中 (官方 config.h) |
| 充电完成 (DONE) | 42 | 低电平=充满 (官方 config.h) |
| 电池 ADC | 3 | ADC1_CH2, 分压比 1/3 |
| 显示屏 SCK/MOSI | 7 / 6 | ST7789 SPI |
| 显示屏 CS/DC/RES/BLK | 15/16/5/4 | 240x240 |
| 灯效串口 RX/TX | 12 / 13 | LIGHT UART (非充电脚!) |
| PWM LED | 11 | TX 脚 10 |

> **引脚来源**: 全部以 `src/bsp/gezipai.rs`（源自官方 config.h）为准。
> **更正记录**: 此前把 GPIO12/13 当作充电检测是误判（那是灯效串口）；充电实为
> GPIO41(CHARGING)/GPIO42(DONE)。早期规格书"引脚28/29"推断一并废弃。
> ESP32-S3-WROOM-1U 的 Quad PSRAM 占用 GPIO33-37，GPIO0-32/38-48 均可作普通 GPIO。

## 依赖

- `xiaozhi-core` (git submodule) - 纯逻辑核心层 (音量/时钟/寄存器序列/状态机)
- `esp-hal` v1.1.0 - ESP32 HAL
- `esp-alloc` v0.10.0 - 内存分配器
- `esp-println` v0.17.0 - 打印支持
- `esp-bootloader-esp-idf` v0.5.0 - Bootloader
- `defmt` v1.0.1 - 日志框架

## 文档

完整索引见 [`docs/README.md`](docs/README.md)。

- [工程指南](AGENTS.md) - 分层、坑位、真机验证记录（改代码前必读）
- [开发指南](docs/DEVELOPMENT.md) - 完整的开发流程
- [快速开始](docs/QUICK_START.md) - 5 分钟上手
- [门禁 / CI / 发布](docs/CI.md) - 本地检查与 release 流程
- [架构决策记录](docs/adr/) - 为什么这样设计
- [移植指南](docs/PORTING.md) - 换板与跨芯片移植
- [路线图](docs/ROADMAP.md) - 阶段计划与验收清单

## 下一步

> ASR / TTS / LLM 由 xiaozhi 服务器完成；设备端只负责采集、编码、播放与设备控制。

- [x] 纯逻辑核心层 xiaozhi-core（音量/时钟/寄存器序列/状态机/OTA/VAD/MCP/AES/MQTT + 136 单测）
- [x] I2S 全双工音频通路（录音/播放 + opus 编解码）
- [x] WiFi 连接 + SoftAP 配网 + NVS 凭据
- [x] WebSocket 会话（xiaozhi 协议 v1/v2/v3）+ TTS 播放 + 按钮打断
- [x] MQTT + UDP 传输（AES-128-CTR、服务端 AEC 时间戳、网关下发配置），见 [docs/MQTT_UDP.md](docs/MQTT_UDP.md)
- [x] VAD 声音唤醒、MCP 设备控制、OTA 下载烧写、ST7789 状态显示、电池/充电
- [ ] v1.0 真机验收与发布（见 [ROADMAP](docs/ROADMAP.md) 的 M8/M10 清单）
- [x] v1.1：MCP 工具扩展、WS v2 时间戳、MQTT+UDP、主循环看门狗、OTA 低电保护
- [ ] v1.2：跨芯片移植（C3/C6/P4，见 [docs/PORTING.md](docs/PORTING.md)）、设备激活/OTA 签名、显示与提示音资源

## 贡献

欢迎提交 Issue 和 Pull Request！请先阅读：

- [贡献指南](CONTRIBUTING.md)（含子模块工作流与门禁）
- [行为准则](CODE_OF_CONDUCT.md)
- [安全策略](SECURITY.md)（漏洞请走私密渠道，勿公开 issue）

## 许可证

[MIT](LICENSE)
