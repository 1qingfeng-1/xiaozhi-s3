<!-- markdownlint-disable-file MD013 -->
# AGENTS.md — xiaozhi-s3 工作区指南

ESP32-S3 智能音箱固件（Rust `no_std` + esp-hal v1.1）。开发板 gezipai：ES8311 (DAC) + ES7210 (麦克风 ADC) + ST7789 (显示)，芯片 ESP32-S3R8 / 16MB Flash。

## 常用命令

`rust-toolchain.toml` 指定 `esp` 工具链（espup 安装的 xtensa 工具链），在本仓库直接 `cargo` 即用 esp 工具链。

```bash
cargo build --release                 # 构建固件（默认 target xtensa-esp32s3-none-elf）
cargo run --release --bin xiaozhi-s3  # 烧录+监视（espflash, defmt 日志）
cargo fmt --all -- --check            # 格式检查
cargo clippy --all-features -- -D warnings --cap-lints allow   # 与 CI 一致
cargo check --all
```

单元测试（只有 xiaozhi-core 有测试，且只能在 host 上跑）：

```bash
cd xiaozhi-core && cargo +stable nextest run   # 或 cargo +stable test
```

测试铁律：

- **不要在仓库根目录跑 `cargo test`**（作用于 xtensa target，必失败）；
- esp 工具链没有 host 的 std/test，host 侧命令必须显式 `+stable`；
- xiaozhi-core 是独立 workspace 根（空 `[workspace]`），不继承父目录 `.cargo/config.toml` 的 esp 配置。

## 架构与分层

```text
xiaozhi-s3/          固件 crate (no_std, esp-hal 薄封装)
├── src/bin/         main.rs 主入口 + gpio-test / i2c-scan 硬件调试工具
├── src/audio/       opus 编解码 (libopus FFI 安全封装, M4.2)
├── src/bsp/         板级支持包（编译时选板，cargo feature）
│   ├── board.rs     BoardConfig 契约（纯数据常量，按功能细分）
│   ├── components/  Board 运行时组件 (leds/buttons/power/battery/audio)
│   └── gezipai.rs   gezipai 板: BOARD 常量 + create_board() 工厂
├── vendor/libopus/  xiph/opus v1.6.1 裁剪副本 (fixed-point 构建所需 125 个 .c)
xiaozhi-core/        git submodule，纯逻辑层（音量映射/I2S 时钟/Codec 寄存器序列/状态机），host 可单测
```

分层规则（改代码前必读）：

- **xiaozhi-core 保持纯逻辑**：禁止引入 esp-hal 等硬件依赖；I2S/I2C 寄存器读写、DMA 等硬件代码放固件 crate。可分类/可计算的逻辑放 core 以便 host 单测。
- **禁止硬编码 GPIO/I2C 地址**：驱动与应用只能读 `bsp::active::BOARD` 配置和 `Board` 组件 API。引脚唯一事实来源：`src/bsp/gezipai.rs`。
- **组合优先于继承**：`BoardConfig` 是编译期常量；Codec 地址按角色命名（`dac_addr`/`adc_addr`），与芯片型号解耦。
- **新增板子**：复制任一 `src/bsp/<board>.rs` → 填 `BOARD` + 实现 `create_board()`（硬接线编译期断言放这里）→ `src/bsp/mod.rs` 注册两行 → `Cargo.toml [features]` 加 `board-<name> = []`。选板守卫（`compile_error!`）在 `src/bsp/mod.rs`，勿删。

## 坑与约束

- `.cargo/config.toml` 的 `-nostartfiles` 链接参数是 **per-target**（只在 `[target.xtensa-esp32s3-none-elf]` 下）。移到全局 `[build]` 会污染 host target，导致 xiaozhi-core 的 host 测试链接失败。
- xiaozhi-core 是 git submodule：克隆需 `--recurse-submodules`；core 有改动后要在父仓库提交 submodule 指针。
- 仓库有多个 bin，`cargo run` 需 `--bin` 指定；固件主程序为 `xiaozhi-s3`。
- 引脚曾误判过：充电检测是 GPIO41(CHARGING)/GPIO42(DONE)，**GPIO12/13 是灯效串口**（非充电脚）。一切以 `src/bsp/gezipai.rs` 为准。
- 日志用 defmt（`DEFMT_LOG=info`），打印走 esp-println。
- edition 2024，rust-version 1.88；`profile.dev` 也开 `opt-level = "s"`（默认 debug 对嵌入式太慢太大，勿改回）。
- CI（`.github/workflows/ci.yml`）用 `esp-rs/xtensa-toolchain` action 装 esp 工具链；core-test job 用 stable + cargo-nextest。不要在 CI 里手动 `source ~/.espup/export-esp.sh`（runner 上不存在该路径）。
- 板载 8MB PSRAM 实际为 **Octal SPI**（esp-hal 自动探测）。PSRAM 注册（`psram_allocator!`）必须在音频 DMA 缓冲分配**之后**（`create_board` 末尾）：S3 GDMA 访问不了 PSRAM，先注册会让 I2S 缓冲落外部 RAM 报 `DmaError(UnsupportedMemoryRegion)`。
- 音频 DMA 缓冲必须在**内部 RAM**；内部堆（160KB）在 `create_board` 第一行注册（各 bin 共用，先于任何分配）。
- I2S 全双工（esp-hal 1.1）：TX 主机经 `signal_loopback` 把时钟共享给 RX（等价 IDF `i2s_ll_share_bck_ws`）。**循环采集的描述符数量必须与缓冲精确等分**（`available()` 从链尾描述符回溯，多余空描述符 → 空指针崩溃）；**RX 从机依赖 TX 时钟**——TX 不跑 ES7210 不出数据（回环用"采集不足一个描述符就补播静音窗"维持时钟）；一次性 TX 重启约有 ~28B 采集损耗；`available()` 按完整描述符（4092B）计数。
- 音量注意：ES8311 REG32 是 **dB 线性映射**（0%= -95.5dB → 100%= +32dB），35% ≈ -51dB 基本不可闻；麦克风数字电平仅 1~5% 满幅，回放前需软件增益（回环/上行均用 800%）。

## opus 集成（M4.2）

- **vendor/libopus 是 xiph/opus v1.6.1 的裁剪副本**（仅 fixed-point 构建所需源码，无
  float/dnn/x86/ARM），由 `build.rs` 用 `cc` 交叉编译。更新它时以官方 `*_sources.mk`
  清单为准重新裁剪。许可证 BSD-3（`vendor/libopus/COPYING`）。
- **xtensa GCC 探测顺序**（`build.rs::find_xtensa_gcc`）：环境变量
  `CC_xtensa_esp32s3_none_elf` / `CC` → PATH 中的 `xtensa-esp32s3-elf-gcc` →
  espup 默认目录 `~/.espressif/tools/xtensa-esp-elf/*/xtensa-esp-elf/bin`。
  CI 由 `esp-rs/xtensa-toolchain` action（espup）自带 GCC，无需额外安装。
- **C 堆分配走 esp-alloc 的 `compat` feature**（malloc/free/calloc/realloc）：
  libopus 状态经 `opus_alloc` → malloc 分配（编码器 ~25KB / 解码器 ~5KB，落内部堆）。
  compat 的 4 字节对齐**已够用**——FIXED_POINT 构建下 opus 状态无 64 位字段
  （`ec_window` 默认 32 位、无 float），若未来开浮点构建需重估对齐。
- **解码器建在本地 16kHz**：libopus 解码时内部重采样，服务器 24kHz 的 TTS 流
  直接输出 16kHz，无需外部重采样器（C++ 参考用的是 SILK 重采样器，两法皆可）。
- **协议参数**（对齐 C++ `websocket_protocol.cc`）：hello `version:1` → 二进制帧
  为**裸 opus 载荷**（无帧头）；上行 16kHz/60ms/单声道 VoIP、complexity 0、DTX 开；
  服务器 hello 的 `sample_rate`（默认 24000）只影响其下行流，本地解码不受影响，
  `frame_duration`（默认 60ms）决定解码缓冲大小。
- **跨 await 借用坑**：async 会话循环里不可对 `board.capture` 做"释放句柄再重启"
  （生成器借用区域粗于 NLL，`E0499`）。采集**常驻启动**，重建只能在 main 的
  直线路径上做（会话结束时 drop→start）；板子组件按字段拆分借用传参
  （`&mut board.audio` / `&mut board.buttons` 与采集句柄 disjoint）。
- **I2S RX 环形缓冲饱和 = `DmaError::Late`**：esp-hal 的 `available()` 从链尾
  描述符回溯计数，整条描述符链都被 CPU 持有（0.5s 没排空）时绕链一圈报 Late，
  且内部 available 计数已被重复累加**损坏**——唯一恢复手段是丢弃句柄重建采集。
  因此**I2S 时钟运行期间必须每轮循环排空采集**（TTS 播放、对讲静音窗都在驱动
  时钟），不是只有对讲时才需要处理 RX。

## C++ 参考实现（本机）

`/home/mrpeng/Documents/xiaozhi-esp32` — 官方 xiaozhi-esp32 成品固件（ESP-IDF/C++，MIT），功能全部正常，移植/排查硬件行为时的第一对照物。关键位置：

- `main/boards/<board>/config.h` — 板级引脚定义；`esp32s3-korvo2-v3` 与 gezipai 同为 ES8311+ES7210，最接近
- `main/audio/audio_service.cc` — 音频数据通路（I2S 收发、opus 编解码调度）
- `main/audio/audio_codec.cc` — 经 esp_codec_dev 控制 ES8311/ES7210；`wake_words/` 唤醒词
- `main/protocols/` — MQTT 与 WebSocket 协议实现（`protocol.cc` 为公共抽象）
- `main/application.cc` 设备状态机、`main/display/` 显示、`main/led/` 灯效
- `ota.cc` 升级、`mcp_server.cc` MCP 设备控制

注意：它是 ESP-IDF + esp_codec_dev 栈；移植到 esp-hal 时，Codec 寄存器层
以 `xiaozhi-core` 序列为准（勿直接照抄调用方式，且勿漏上电段，
见 codec.rs 注释与单测）。

## 约定与文档

- 注释、文档、commit message 均为中文；commit 遵循 Conventional Commits
  （feat/fix/docs/chore…），release changelog 用 git-cliff（`cliff.toml`）。
- 格式化约束在 `rustfmt.toml`（max_width 100）；clippy 阈值在 `clippy.toml`（栈阈值 1024B，嵌入式从严）。
- 开发流程：`docs/DEVELOPMENT.md`、`docs/QUICK_START.md`；硬件引脚表：README「引脚映射」一节。
