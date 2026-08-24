# 小智音箱 - ESP32-S3 智能音箱

## 📋 项目概述

基于 ESP32-S3-AI-2 模组的智能音箱，使用 Rust 开发。支持语音识别、语音合成和大语言模型对话。

## ✨ 特性

- **硬件平台**: ESP32-S3R8 (160MHz, 512KB SRAM, 16MB Flash)
- **音频系统**: ES8311 Codec + ES7210 麦克风阵列 (4通道)
- **功放**: LPA2103A (2W/4Ω)
- **充电管理**: LGS4056HDA (1A)
- **开发语言**: Rust (nightly, esp 工具链)
- **框架**: esp-hal v1.1.2

## 🚀 快速开始

```bash
# 1. 安装依赖
cargo build

# 2. 烧录到硬件
cargo run --release

# 3. 监视日志
espflash monitor --chip esp32s3
```

## 📁 项目结构

```
xiaozhi-s3/
├── src/
│   ├── lib.rs              # 库定义
│   ├── audio/
│   │   └── mod.rs          # 音频模块
│   └── bin/
│       ├── main.rs         # 主程序
│       └── gpio-test.rs    # GPIO 测试
├── Cargo.toml
├── rust-toolchain.toml
├── README.md
├── DEVELOPMENT.md
├── HARDWARE.md
├── QUICK_START.md
└── COMPLETION_SUMMARY.md
```

## 📚 文档

- [README.md](README.md) - 项目文档
- [QUICK_START.md](QUICK_START.md) - 快速开始
- [DEVELOPMENT.md](DEVELOPMENT.md) - 开发指南
- [HARDWARE.md](../HARDWARE.md) - 硬件文档
- [COMPLETION_SUMMARY.md](COMPLETION_SUMMARY.md) - 完成总结

## 🛠️ 技术栈

- **语言**: Rust
- **HAL**: esp-hal v1.1.2
- **音频**: ES8311 (Codec) + ES7210 (Mic Array)
- **网络**: WiFi + MQTT/HTTP (待实现)
- **AI**: ASR + TTS + LLM (待实现)

## 📈 开发进度

### ✅ 已完成
- [x] 开发环境搭建
- [x] 项目初始化
- [x] 硬件文档
- [x] GPIO 基础功能
- [x] 编译验证

### ⏳ 进行中
- [ ] I2S 音频驱动
- [ ] WiFi 连接
- [ ] 语音识别 (ASR)
- [ ] 语音合成 (TTS)
- [ ] LLM 对话

## 📞 联系与支持

- GitHub: https://github.com/esp-rs/esp-hal
- 文档: https://docs.espressif.com/projects/rust/book/

## 📄 许可证

MIT License

## 📊 版本历史

- **v0.1.0** (2026-08-24) - 项目初始化完成
  - 开发环境搭建
  - GPIO 基础功能
  - 项目结构创建

---

**项目状态**: ✅ 开发环境就绪，准备进入下一阶段开发
