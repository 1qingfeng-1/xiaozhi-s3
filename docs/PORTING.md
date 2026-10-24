<!-- markdownlint-disable-file MD013 -->
# 多板与跨芯片移植指南

本项目把**可计算逻辑**放在 `xiaozhi-core`（host 单测），把**硬件操作**留给
`xiaozhi-s3` 固件 crate。移植分两类：同芯片换板（已完成路径，可直接照做）
与跨芯片移植（C3/C6/P4，需要新建目标与固件 crate，当前未验证）。

## 1. 同芯片换板（ESP32-S3）

BSP 采用「组合优先于继承」：`BoardConfig` 是纯数据契约，`create_board()`
按配置组装运行时组件。

新增一块板四步：

1. 复制 `src/bsp/gezipai.rs` 为 `src/bsp/<board>.rs`，修改 `BOARD` 常量与
   工厂里的 I2C 地址/Codec 初始化；
2. `src/bsp/mod.rs` 增加 `mod <board>;` 与 feature 门控的 `pub use`；
3. `Cargo.toml` 增加 `board-<board> = []`，并视情况调整 `default`；
4. 若同时启用多块板，补全 `src/bsp/mod.rs` 末尾的互斥 `compile_error!`。

约束：

- 应用/驱动只读 `bsp::active::BOARD` 与 `Board` 组件 API，**禁止硬编码
  GPIO/I2C 地址**；硬接线断言只允许出现在板子工厂内。
- Codec 差异只在板级工厂里体现（选择驱动实现与寄存器序列），公共契约
  只暴露「播放通道/录音通道」功能角色。
- 音频 DMA 缓冲必须在内部 RAM；PSRAM 分配器在 `create_board()` 末尾注册。

编译验证（无硬件也可做）：

```bash
cargo check --no-default-features --features board-<board>
```

## 2. 跨芯片移植（C3 / C6 / P4）

这些芯片与 S3 的差异不是「换引脚」级别，单个 `xiaozhi-s3` crate 无法同时
编译（target、外设、内存映射都不同）。推荐做法：

| 层 | 复用方式 |
| --- | --- |
| `xiaozhi-core` | 原样复用（纯 no_std 逻辑，无芯片依赖） |
| 协议/传输 | `session.rs`、`mqtt_session.rs` 逻辑可复制，依赖仅 embassy-net |
| BSP 契约 | 复制 `board.rs` 契约，按芯片能力裁剪组件 |
| 固件 crate | 新建 `xiaozhi-<chip>` crate + 对应 rust target |

按芯片的已知差异：

- **ESP32-C3**（RISC-V，160MHz，400KB SRAM，无 PSRAM）：
  - target `riscv32imc-unknown-none-elf`，需另建 `.cargo/config.toml`；
  - 无 PSRAM：PCM 队列/预录必须缩小或改流式，内存预算需重新评估；
  - I2S 外设与 S3 不同（esp-hal 的 `i2s` 泛型可复用，但引脚矩阵/DMA 数量少）；
  - WiFi 只有 2.4G 单频，缺少 S3 的 PSRAM 缓冲余量。
- **ESP32-C6**（RISC-V，WiFi 6 + Thread/Zigbee）：
  - target `riscv32imac-unknown-none-elf`；
  - 若要做 Matter/Thread 需要额外协议栈，当前固件不含；
  - 内存与 C3 同级别，音频链路需精简。
- **ESP32-P4**（RISC-V 双核，无内置 WiFi，需外挂 C6 做无线）：
  - 需要双芯片通信（SDIO/SPI）与独立网络协处理器固件；
  - 音频外设丰富、内存大，适合高音质/显示增强，但工程量最大。

落地步骤（建议按此顺序）：

1. 新建 crate，先跑通 `esp-hal` 的 hello + WiFi + 时间基（embassy）；
2. 接入 `xiaozhi-core`，用 host 单测保证状态机/协议不变；
3. 打通 I2S 采集/播放（先用回环 bin），再接入 opus；
4. 最后接 `session.rs`/`mqtt_session.rs`，跑一遍 hello → TTS 回放。

**现状（2026-10-24）**：同芯片换板路径已具备并被本板验证；C3/C6/P4 尚未
移植（无硬件、无对应 target crate），本文件仅给出工程路径与差异清单。
