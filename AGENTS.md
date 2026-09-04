<!-- markdownlint-disable-file MD013 -->
# AGENTS.md — xiaozhi-s3 工作区指南

ESP32-S3 智能音箱固件（Rust `no_std` + esp-hal v1.1 + esp-rtos 0.3/embassy-net）。
开发板 gezipai：ES8311 (DAC) + ES7210 (麦克风) + ST7789 (显示)，ESP32-S3R8 / 16MB Flash / 8MB Octal PSRAM。
全语音链路已接通：VAD 唤醒 → WebSocket 会话（xiaozhi 协议 v1，二进制帧 = 裸 opus）→ TTS 播放/打断，
另含配网、MCP、OTA 检查、显示、电池。

## 常用命令

`rust-toolchain.toml` 指定 `esp` 工具链（espup 安装），本仓库直接 `cargo` 即用之（含 `build-std = ["alloc","core"]`）；
**host 侧命令必须显式 `+stable`**。

```bash
cargo build --release                        # 固件 (target xtensa-esp32s3-none-elf)
cargo run --release --bin xiaozhi-s3         # 烧录+监视 (espflash, defmt)
cargo run --release --bin audio-loopback     # 其他调试 bin: wifi-test / gpio-test / i2c-scan
cargo fmt --all -- --check
cargo clippy --all-features -- -D warnings --cap-lints allow   # 与 CI 一致
cargo check --all

cd xiaozhi-core && cargo +stable nextest run  # 核心层 77 个 host 单测（或 cargo +stable test）
```

- 仓库共 **5 个 bin**：`xiaozhi-s3`（产品固件）与 `wifi-test` 在 Cargo.toml 声明，
  `gpio-test`/`i2c-scan`/`audio-loopback` 靠 `src/bin/` 自动发现；`cargo run` 必须 `--bin` 指定。
- panic 打印回溯后软复位；符号化：
  `xtensa-esp32s3-elf-addr2line -pfiaC -e target/xtensa-esp32s3-none-elf/release/xiaozhi-s3 <bt 地址...>`

测试铁律：

- **不要在仓库根目录跑 `cargo test`**（作用于 xtensa target，必失败）；
- xiaozhi-core 是独立 workspace 根（空 `[workspace]` + 自带 `.cargo/config.toml` 覆盖回 host target），
  不继承父目录的 esp target/-nostartfiles；
- 只有 xiaozhi-core 有单测，且只能在 host 上跑。

## 架构与分层

```text
xiaozhi-s3/          固件 crate (no_std, esp-rtos 双任务)
├── src/bin/         main.rs 产品固件 + wifi-test / gpio-test / i2c-scan / audio-loopback 调试 bin
├── src/app.rs       双任务编排: 音频主循环 (非阻塞, 每轮 ≤64ms) + 会话任务 (WS 长阻塞 await 集中于此)
├── src/session.rs   WS 传输层 (embassy-net TCP 上手写帧编解码, :8000 /xiaozhi/v1/, 含 ping/pong)
├── src/ota_check.rs OTA 检查 (GET :8003/xiaozhi/ota/ → 版本比较; 下载烧写待分区表, ROADMAP M7)
├── src/wifi/        STA 连接 + SoftAP 配网 + Flash 凭据存取 (esp-storage)
├── src/audio/       opus FFI 安全封装 + wake_net (esp-sr, feature `wakenet`)
├── src/bsp/         板级支持 (board.rs 契约 / components/ 组件 / gezipai.rs 工厂)
├── src/macros.rs    mk_static! (StaticCell 常驻分配, 各 bin 共用)
├── vendor/          libopus (v1.6.1 裁剪) / esp-sr / esp-dsp 子集 / cjson (均 build.rs 编译或链接)
xiaozhi-core/        git submodule, 纯逻辑层: audio (音量/时钟/codec 序列/状态机/唤醒 VAD) +
                     dialog (对话状态机) + mcp (JSON-RPC 解析) + ota (清单/版本) + display + board 规则;
                     host 可单测 (77 个), #![deny(unsafe_code)]
```

分层规则（改代码前必读）：

- **xiaozhi-core 保持纯逻辑**：禁止引入 esp-hal 等硬件依赖；I2S/I2C 寄存器读写、DMA 等
  硬件代码放固件 crate。可分类/可计算的逻辑（状态机、协议解析、版本比较）放 core 以便 host 单测。
- **禁止硬编码 GPIO/I2C 地址**：驱动与应用只能读 `bsp::active::BOARD` 与 `Board` 组件 API；
  引脚唯一事实来源 `src/bsp/gezipai.rs`；硬件接线 `const _: () = assert!` 只允许在板子工厂内。
- **组合优先于继承**：`BoardConfig` 编译期常量，Codec 地址按角色命名（`dac_addr`/`adc_addr`）。
- **新增板子**：复制 `src/bsp/<board>.rs` → 填 `BOARD` + 实现 `create_board()` →
  `src/bsp/mod.rs` 注册两行 → Cargo.toml 加 `board-<name>` feature（选板 `compile_error!` 守卫在 mod.rs，勿删）。

## 坑与约束

- `.cargo/config.toml` 的 `-nostartfiles` 是 **per-target**（只在 `[target.xtensa-esp32s3-none-elf]`），
  移到全局 `[build]` 会污染 host target，core 测试链接失败。
- xiaozhi-core 是 git submodule：克隆需 `--recurse-submodules`；core 改动后要在父仓库提交 submodule 指针
  （core 是独立仓库，改动先提交/推送 core 本身）。
- 日志用 defmt（`.cargo/config.toml` `[env] DEFMT_LOG=info`），打印走 esp-println。
- edition 2024，rust-version 1.88；`profile.dev` 也开 `opt-level = "s"`（勿改回）。
- 内部堆 **128KB**（`create_board` 第一行注册，先于任何分配，各 bin 共用）；
  主任务栈 = RWDATA 剩余（~107KB）。**opus 编码 60ms 帧需 ~87KB 栈**（SILK VLA 随帧长线性，
  `VAR_ARRAYS` 走任务栈）——缩堆或加长帧前必须重估，否则栈溢出。
- 音频 DMA 缓冲必须在**内部 RAM**；8MB PSRAM 为 Octal SPI，`psram_allocator!` 必须在
  DMA 缓冲分配**之后**注册（`create_board` 末尾），否则 I2S 缓冲落 PSRAM 报
  `DmaError(UnsupportedMemoryRegion)`。opus 大状态块/socket 缓冲是纯 CPU 访问，可落 PSRAM。
- I2S 全双工（esp-hal 1.1）：TX 主机经 `signal_loopback` 把时钟共享给 RX（等价 IDF
  `i2s_ll_share_bck_ws`）；**RX 从机依赖 TX 时钟**，TX 不跑 ES7210 不出数据。
  循环采集描述符必须与缓冲精确等分（8×4092B = 32KB RX 链）。
- **pop 是全量语义**：缓冲必须 ≥ 当前 accumulated（描述符粒度 4095，勿自作对齐），否则
  `BufferTooSmall`；排空缓冲即整条 RX 链（32736B），长阻塞期绕回填满时最旧数据自然流失。
- **I2S RX 环形缓冲饱和 = `DmaError::Late`**：esp-hal `available()` 从链尾回溯计数，
  整条链被 CPU 持有时绕链一圈报 Late，且内部计数已**损坏**——唯一恢复是丢弃句柄重建采集。
  因此 **I2S 时钟运行期间（TTS 播放、静音窗、待机 VAD 全程）每轮必须排空采集**；
  重建只能在 main 的直线路径做（`app::run` 返回 `CaptureFault` → drop→`capture.start()` 重进）。
- **跨 await 借用坑**：async 循环里不可对 `board.capture` 做"释放句柄再重启"
  （借用区域粗于 NLL，E0499）；板子组件按字段拆 `&mut` 传参（与采集句柄 disjoint）。
- **S3 WiFi PHY 与 SARADC 共用**：WiFi 激活期间读 ADC1（电池 GPIO3）干扰 PHY，
  曾崩 WiFi blob 信标路径（`pm_on_beacon_rx` LoadProhibited）；故电量 ADC 只在 **Idle 且
  10s 限频**时读一次（会话/播报期绝不读，见 `app.rs` 主循环），充电态用 GPIO
  （CHARGING=41/DONE=42）实时。排查 WiFi 崩溃先确认电池 ADC 未在活跃期被读。
- **embassy-net `StackResources<8>`**（DHCP 1 + TCP 7）：超出容量时 `TcpSocket::new` panic
  → 重启循环。socket 跨重连复用（`session_task` 内持有），勿每次 connect 新建（泄漏耗尽内部堆）。
- `session_task` 是 embassy 任务：参数必须 `'static`（Channels 经 `mk_static!` 常驻），
  返回的 SpawnToken 必须 `spawner.spawn`（丢弃即 panic）。
- BOOT 键**开机按住** = 清除 WiFi 凭据重回配网；配网 = SoftAP `xiaozhi-gezipai` +
  表单页 `http://192.168.4.1:8080/`，凭据+服务器 IP 存 NVS 分区末扇区（magic `ZOIX`）。
- 对话状态机（`xiaozhi_core::dialog`）：`WaitingReply` 10s 无服务器消息 → 结束会话回待机。
- **MCP 目前刻意未启用**：hello 发 `features:{}`。若宣告 `mcp:true`，服务器会等
  `initialize` 应答、期间停读 TCP 流 → WiFi TX 缓冲池耗尽（`esp_wifi_internal_tx NO_MEM(257)` 连刷，
  见 commit 27359e0）。`app::mcp_handle` 是已写好但未接线的应答构造（当前 dead code，
  故 `clippy -D warnings` 在本工作区会报）；接通前勿删，接通后需真机验证。
- 音量：ES8311 REG32 是 **dB 线性映射**（0%= -95.5dB → 100%= +32dB），35% ≈ -51dB 基本不可闻
  （audio-loopback 回放音量压 35% 防啸叫）；麦克风数字电平仅 1~5% 满幅，回放/上行前
  软件增益 800%（+18dB）。
- 引脚曾误判：充电检测是 GPIO41(CHARGING)/GPIO42(DONE)；**GPIO12/13 是灯效串口**（非充电脚）。
  一切以 `src/bsp/gezipai.rs` 为准。

## opus / wakenet 集成

- **wakenet（esp-sr v1.9.5，feature 门控）真机已验证失败**（2026-08-30）：链接闭环，
  但首次 `detect` 即 InstrProhibited（PC=0x1FFE2000）——wn9 推理路径经函数指针跳到
  **MMU cache block 1**（IDF 专有的 PSRAM 窗口 0x1FC00000/0x5FC00000），esp-hal 只映射
  block 0，非垫片可修。出路（写 MMU block 1 页表 / 换 esp-sr 2.x+esp-dl / 继续 VAD）
  见 `vendor/esp-sr/README.md`。**VAD 声音唤醒（`VadWakeDetector` + `WakePolicy`）
  是默认且已验证的路径**，两层共用 `WakeWordSource` trait，切换约 5 行。
- `vendor/libopus` 是 xiph/opus v1.6.1 裁剪副本（fixed-point，无 float/dnn/x86/ARM），
  `build.rs` 用 xtensa GCC 交叉编译（探测：`CC_xtensa_esp32s3_none_elf`/`CC` → PATH →
  espup 默认目录）；更新以官方 `*_sources.mk` 清单为准。BSD-3（`vendor/libopus/COPYING`）。
- C 堆走 esp-alloc `compat`（malloc/free/…）：编码器 ~25KB / 解码器 ~5KB 落内部堆。
  4 字节对齐够用（FIXED_POINT 无 64 位字段；若开浮点构建需重估）。
- 解码器建在**本地 16kHz**：libopus 内部重采样，服务器 24kHz TTS 流直接出 16kHz，
  无需外部重采样器。协议（对齐 C++ `websocket_protocol.cc`）：hello `version:1`；
  上行 16kHz/60ms/单声道 VoIP、complexity 0、DTX 开；服务器 `sample_rate` 只影响下行。
- 下行播放每轮 ≤3 帧（~180ms），防连播期间 RX 回积压超排空缓冲；无下行时播静音块维持时钟。
- 单 opus 包上限 1276B（RFC 6716），通道容量：uplink 12 / downlink 6 / command 4 / event 4，满则丢帧。

## C++ 参考实现（本机）

`/home/mrpeng/Documents/xiaozhi-esp32` — 官方 xiaozhi-esp32 成品固件（ESP-IDF/C++，MIT），
移植/排查硬件行为时的第一对照物：

- `main/boards/<board>/config.h`（`esp32s3-korvo2-v3` 同为 ES8311+ES7210，最接近 gezipai）
- `main/audio/audio_service.cc`（数据通路）、`main/audio/audio_codec.cc`（esp_codec_dev）
- `main/protocols/`（MQTT/WS 协议）、`main/application.cc`（状态机）、`main/display/`、`main/led/`
- `ota.cc`、`mcp_server.cc`

注意：它是 ESP-IDF + esp_codec_dev 栈；移植到 esp-hal 时 Codec 寄存器层以
`xiaozhi-core` 序列为准（勿照抄调用方式，勿漏上电段，见 codec.rs 注释与单测）。

## 约定与文档

- 注释、文档、commit message 均为中文；Conventional Commits；changelog 用 git-cliff（`cliff.toml`）。
- **Release 流程**：推 `v[0-9]+.*` tag 触发 `release.yml`（构建 + git-cliff changelog + GitHub Release）。
- pre-commit（`.pre-commit-config.yaml`）：fmt / clippy / check / cargo-deny / typos / markdownlint；
  `vendor/` 整体排除（三方资产保持上游原样）。
- 格式化 `rustfmt.toml`（max_width 100）；clippy 阈值 `clippy.toml`（栈 1024B）。
- 开发流程 `docs/DEVELOPMENT.md`、`docs/QUICK_START.md`；阶段计划与**真机验证清单**
  `docs/ROADMAP.md`（下一步上机验证项以此为准）；引脚表见 README「引脚映射」。
