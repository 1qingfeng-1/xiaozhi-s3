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
└── src/bsp/         板级支持包（编译时选板，cargo feature）
    ├── board.rs     BoardConfig 契约（纯数据常量，按功能细分）
    ├── components/  Board 运行时组件 (leds/buttons/power/battery/audio)
    └── gezipai.rs   gezipai 板: BOARD 常量 + create_board() 工厂
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
