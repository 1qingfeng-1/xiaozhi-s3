# 小智音箱 - 快速开始

## 环境检查

```bash
# 检查 Rust
rustc --version
cargo --version

# 检查 ESP 工具链
espup --version

# 检查项目
cd xiaozhi-s3
cargo check
```

## 构建项目

```bash
# 开发构建
cargo build

# 发布构建
cargo build --release
```

## 烧录到硬件

### 方法 1: 使用 cargo

```bash
# 自动构建并烧录
cargo run --release
```

### 方法 2: 使用 espflash

```bash
# 手动烧录
espflash flash --chip esp32s3 --baud 460800 target/xtensa-esp32s3-none-elf/release/xiaozhi-s3
```

### 方法 3: 使用 probe-rs

```bash
# 使用 probe-rs (需要 USB 连接)
probe-rs run --chip ESP32S3 target/xtensa-esp32s3-none-elf/release/xiaozhi-s3
```

## 监视日志

```bash
# 方法 1: espflash
espflash monitor --chip esp32s3

# 方法 2: probe-rs
probe-rs reset --chip ESP32S3

# 方法 3: 串口工具
# 使用 minicom, screen 或 Python 的 pyserial
# 波特率: 115200
```

## 测试 GPIO

```bash
# 运行 GPIO 测试程序
cargo run --bin gpio-test
```

**预期行为:**
1. LED (GPIO4) 闪烁 10 次
2. 显示按键状态
3. 持续检测按键 (按任意键显示状态)

## 硬件连接

### USB 连接
- 使用 USB-C 线连接 ESP32-S3-AI-2 的 USB 口
- 确保电脑识别为串口设备

### 串口设置
- 波特率: 115200
- 数据位: 8
- 停止位: 1
- 无校验

### 电源
- USB 供电 (5V/1A)
- 或锂电池 (3.7V, 通过 LGS4056HDA 充电)

## 常见问题

### Q: 无法连接串口
**A:**
- 检查 USB 线是否支持数据传输
- 检查设备管理器中是否有 COM 端口
- 尝试更换 USB 口或 USB 线

### Q: 烧录失败
**A:**
- 检查 BOOT 按钮是否在按下状态 (启动时)
- 检查 EN 按钮是否正常工作
- 尝试降低烧录波特率: `--baud 921600`

### Q: 没有日志输出
**A:**
- 检查串口工具波特率是否设置为 115200
- 检查设备是否已正确烧录
- 尝试按下 EN 按钮重启

### Q: GPIO 测试没有反应
**A:**
- 检查 LED 是否连接正确
- 检查按键是否连接正确
- 查看串口日志是否有错误信息

## 下一步

1. **验证硬件**
   - 运行 GPIO 测试: `cargo run --bin gpio-test`
   - 检查 LED 和按键是否正常工作

2. **实现音频**
   - 参考 `src/audio/mod.rs`
   - 实现 ES8311/ES7210 驱动
   - 测试 I2S 音频采集和播放

3. **连接网络**
   - 实现 WiFi 连接
   - 测试 HTTP/MQTT 通信

4. **语音处理**
   - 实现 ASR (语音识别)
   - 实现 TTS (语音合成)

5. **智能对话**
   - 连接 LLM API
   - 实现多轮对话

## 获取帮助

- 项目文档: README.md, DEVELOPMENT.md
- 硬件文档: HARDWARE.md
- esp-hal 文档: https://docs.espressif.com/projects/rust/book/
- GitHub: https://github.com/esp-rs/esp-hal

## 版本信息

- 项目版本: v0.1.0
- Rust 版本: stable
- esp-hal: v1.1.2
- 生成日期: 2026-08-24
