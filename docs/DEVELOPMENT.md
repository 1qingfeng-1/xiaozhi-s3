# 小智音箱 - 开发指南

## 项目状态

✅ **工程初始化完成**
- ESP32-S3 Rust 开发环境已配置
- 基础项目结构已创建
- 编译通过

## 文件结构

```
xiaozhi-s3/
├── src/
│   ├── lib.rs              # 库定义
│   ├── audio/
│   │   └── mod.rs          # 音频模块 (I2S, ES8311, ES7210)
│   └── bin/
│       └── main.rs         # 主程序入口
├── .cargo/
│   └── config.toml         # ESP 工具链配置
├── rust-toolchain.toml     # 指定 esp 工具链
├── build.rs                # 构建脚本
├── Cargo.toml              # 项目配置
├── README.md               # 项目文档
└── .gitignore
```

## 开发环境

### 已安装工具

- Rust (stable)
- esp 工具链 (通过 espup)
- esp-generate v1.3.0
- espup v0.17.1

### 构建命令

```bash
# 检查
cargo check

# 开发构建
cargo build

# 发布构建
cargo build --release

# 烧录 (需要连接 ESP32-S3)
cargo run --release
# 或
espflash flash --chip esp32s3 --baud 460800 target/xtensa-esp32s3-none-elf/release/xiaozhi-s3

# 监视日志
espflash monitor --chip esp32s3
```

## 硬件引脚 (ESP32-S3-AI-2)

### 核心外设

| 功能 | GPIO | 说明 |
|------|------|------|
| **音频 Codec** | I2S0/I2C0 | ES8311 (ADC/DAC) |
| **麦克风阵列** | I2S0/I2C0 | ES7210 (4ch) |
| **功放** | VOP/VON | LPA2103A |
| **充电检测** | GPIO28/29 | LGS4056HDA |

### GPIO 分配

| 功能 | GPIO | 备注 |
|------|------|------|
| LED 电源 | GPIO4 | 红色 |
| LED 呼吸 | GPIO5 | 白色 |
| 音量减 | GPIO18 | SW1 |
| 音量加 | GPIO1 | SW2 |
| 配网 | GPIO2 | SW3 |
| BOOT | GPIO0 | 启动模式 |
| USB D- | GPIO19 | USB通信 |
| USB D+ | GPIO20 | USB通信 |
| UART0 TX | GPIO43 | 调试 |
| UART0 RX | GPIO44 | 调试 |

## 下一步开发计划

### 阶段 1: 基础外设 (1-2周)
- [ ] GPIO 控制 (LED, 按键)
- [ ] I2S 音频采集 (ES7210)
- [ ] I2S 音频播放 (ES8311)
- [ ] 充电状态检测

### 阶段 2: 网络通信 (2-3周)
- [ ] WiFi 连接
- [ ] HTTP 客户端
- [ ] MQTT 客户端
- [ ] OTA 升级

### 阶段 3: 语音处理 (3-4周)
- [ ] 语音识别 (ASR)
- [ ] 语音合成 (TTS)
- [ ] 回声消除
- [ ] 降噪处理

### 阶段 4: 智能对话 (4-6周)
- [ ] LLM 对话
- [ ] 上下文管理
- [ ] 多轮对话
- [ ] 技能系统

## 参考资源

- ESP32-S3 技术参考手册
- esp-hal 文档: https://docs.espressif.com/projects/rust/book/
- esp-hal 示例: https://github.com/esp-rs/esp-hal/tree/main/examples
- ESP32-S3-AI-2 硬件文档: ../HARDWARE.md
