# 小智音箱 (ESP32-S3)

基于 ESP32-S3-AI-2 模组的智能音箱，使用 Rust 开发。

## 硬件

- **主控**: ESP32-S3R8 (160MHz, 512KB SRAM)
- **Flash**: 16MB
- **音频Codec**: ES8311 (I2S + I2C)
- **麦克风阵列**: ES7210 (4通道)
- **功放**: LPA2103A (2W/4Ω)
- **充电**: LGS4056HDA (1A)
- **尺寸**: 19.5mm × 18mm

详见 [硬件文档](../HARDWARE.md)

## 项目结构

```
xiaozhi-s3/
├── src/
│   ├── lib.rs          # 库定义
│   └── bin/
│       └── main.rs     # 主入口
├── .github/
│   └── workflows/
│       ├── ci.yml      # CI 工作流
│       └── release.yml # Release 工作流
├── .cargo/
│   └── config.toml     # ESP 工具链配置
├── docs/               # 文档目录
│   ├── README.md       # 文档导航
│   ├── DEVELOPMENT.md  # 开发指南
│   ├── QUICK_START.md  # 快速开始
│   ├── PRECOMMIT.md    # Pre-commit 说明
│   └── GITHUB_ACTIONS.md # GitHub Actions 文档
├── rust-toolchain.toml # 指定 esp 工具链
├── build.rs            # 构建脚本
└── Cargo.toml
```

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
# 使用 espflash
cargo run --release

# 或手动
espflash flash --chip esp32s3 --baud 460800 target/riscv32imc-esp-espidf/release/xiaozhi-s3
```

## 引脚映射

| 功能 | GPIO | 说明 |
|------|------|------|
| USB D- | 19 | USB通信 |
| USB D+ | 20 | USB通信 |
| UART0 TX | 43 | 调试串口 |
| UART0 RX | 44 | 调试串口 |
| LED 电源 | 4 | 红色 |
| LED 呼吸 | 5 | 白色 |
| 音量减 | 18 | SW1 |
| 音量加 | 1 | SW2 |
| 配网 | 2 | SW3 |
| 充电完成 | 28 | LGS4056HDA |
| 充电中 | 29 | LGS4056HDA |

## 依赖

- `esp-hal` v1.1.0 - ESP32 HAL
- `esp-alloc` v0.10.0 - 内存分配器
- `esp-println` v0.17.0 - 打印支持
- `esp-bootloader-esp-idf` v0.5.0 - Bootloader
- `defmt` v1.0.1 - 日志框架

## 文档

### 开发文档

- [开发指南](docs/DEVELOPMENT.md) - 完整的开发流程
- [快速开始](docs/QUICK_START.md) - 5 分钟上手
- [项目概述](docs/PROJECT.md) - 项目介绍
- [项目状态](docs/PROJECT_STATUS.md) - 开发进度

### 工具文档

- [Pre-commit 说明](docs/PRECOMMIT.md) - 代码检查配置
- [Pre-commit 指南](docs/PRECOMMIT_GUIDE.md) - 详细使用说明
- [Pre-commit 速查表](docs/PRECOMMIT_CHEATSHEET.md) - 快速参考
- [GitHub Actions 文档](docs/GITHUB_ACTIONS.md) - CI/CD 配置
- [GitHub Actions 速查表](docs/GITHUB_ACTIONS_CHEATSHEET.md) - 快速参考

### 总结文档

- [完成总结](docs/COMPLETION_SUMMARY.md) - 已完成的工作
- [Pre-commit 总结](docs/PRECOMMIT_SUMMARY.md) - 配置总结
- [GitHub Actions 总结](docs/GITHUB_ACTIONS_SUMMARY.md) - 配置总结

## 下一步

- [ ] 完善 I2S 驱动 (ES8311/ES7210)
- [ ] 实现按键检测 (长按/短按)
- [ ] LED 呼吸灯动画
- [ ] 电池电量检测 (ADC)
- [ ] WiFi 连接
- [ ] MQTT 通信
- [ ] 语音识别 (ASR)
- [ ] 语音合成 (TTS)
- [ ] 语音对话 (LLM)

## 贡献

欢迎提交 Issue 和 Pull Request！

## 许可证

MIT License
