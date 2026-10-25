<!-- markdownlint-disable-file MD013 -->
# AGENTS.md — xiaozhi-s3 工作区指南

ESP32-S3 智能音箱固件（Rust `no_std` + esp-hal v1.1 + esp-rtos 0.3/embassy-net）。
开发板 gezipai：ES8311 (DAC) + ES7210 (麦克风) + ST7789 (显示)，ESP32-S3R8 / 16MB Flash / 8MB Octal PSRAM。
全语音链路已接通：VAD 唤醒 → WebSocket 会话（xiaozhi 协议 v1，二进制帧 = 裸 opus）→ TTS 播放/打断，
另含配网、MCP、OTA 下载烧写（双 app 乒乓切槽）、显示、电池。

## 常用命令

`rust-toolchain.toml` 指定 `esp` 工具链（espup 安装），本仓库直接 `cargo` 即用之（含 `build-std = ["alloc","core"]`）；
**host 侧命令必须显式 `+stable`**。

```bash
cargo build --release                        # 固件 (target xtensa-esp32s3-none-elf)
cargo run --release --bin xiaozhi-s3         # 烧录+监视 (espflash, defmt)
cargo run --release --bin audio-loopback     # 其他调试 bin: wifi-test / gpio-test / i2c-scan
cargo fmt --all -- --check
cargo clippy --all-features -- -D warnings --cap-lints allow   # 与 CI/pre-commit 一致
cargo check --all

# 核心层 168 个 host 单测 — 必须在 xiaozhi-core/ 目录内跑
cd xiaozhi-core && cargo +stable nextest run
# 单测过滤: cargo +stable nextest run ota_crc
# 或 cargo +stable test
```

**为何必须 `cd xiaozhi-core`**：父仓库 `.cargo/config.toml` 把 `[build] target` 锁到
xtensa；从根目录跑 `cargo +stable nextest run -p xiaozhi-core` 会拿父配置编 host 依赖
（E0463: can't find crate for `core`）。CI 同样 `working-directory: xiaozhi-core` + `cargo nextest run`（不带 `-p`）。

- 仓库共 **5 个 bin**：`xiaozhi-s3`（产品固件）与 `wifi-test` 在 Cargo.toml 声明，
  `gpio-test`/`i2c-scan`/`audio-loopback` 靠 `src/bin/` 自动发现；`cargo run` 必须 `--bin` 指定。
- panic 打印回溯后软复位；符号化：
  `xtensa-esp32s3-elf-addr2line -pfiaC -e target/xtensa-esp32s3-none-elf/release/xiaozhi-s3 <bt 地址...>`

测试铁律：

- **不要在仓库根目录跑 `cargo test`**（作用于 xtensa target，必失败）；
- xiaozhi-core 是独立 workspace 根（空 `[workspace]` + 自带 `.cargo/config.toml` 覆盖回 host target），
  不继承父目录的 esp target/-nostartfiles；
- 只有 xiaozhi-core 有单测，且只能在 host 上跑（168 个，当前全绿）。

## 架构与分层

```text
xiaozhi-s3/          固件 crate (no_std, esp-rtos 双任务)
├── src/bin/         main.rs 产品固件 + wifi-test / gpio-test / i2c-scan / audio-loopback 调试 bin
├── src/app.rs       双任务编排: 音频主循环 (非阻塞, 每轮 Idle 10ms/活跃 1ms)
│                    + 会话任务 (WS 长阻塞 await 集中于此)
├── src/session.rs   WS 传输层 (embassy-net TCP 上写手帧编解码, :8000 /xiaozhi/v1/, 含 ping/pong)
├── src/mqtt_session.rs MQTT+UDP 传输 (MQTT 控制通道 + UDP/AES-128-CTR 音频, 网关配置由 OTA 下发)
├── src/wdt.rs       主循环看门狗 (TIMG0 MWDT 8s, 每轮 feed)
├── src/ota_check.rs OTA: boot_ota_task 独立任务 POST :8003 检查 → download_and_flash
│                    (HTTP200/完整性/分区上界/超时/失败退避 → 4KB 擦写 → otadata → 复位)
├── src/wifi/        STA 连接 + SoftAP 配网 + Flash 凭据/音量存取 (esp-storage)
├── src/audio/       opus FFI 安全封装 + wake_net (esp-sr, feature `wakenet`)
├── src/bsp/         板级支持 (board.rs 契约 / components/ 组件 / gezipai.rs 工厂)
├── src/macros.rs    mk_static! (StaticCell 常驻分配, 各 bin 共用)
├── partitions.csv   双 app 分区表 (见下); espflash.toml 声明其路径
├── vendor/          libopus (v1.6.1 裁剪) / esp-sr / esp-dsp 子集 / cjson (均 build.rs 编译或链接)
xiaozhi-core/        git submodule, 纯逻辑层: audio (音量映射/时钟/codec 序列/唤醒 VAD/播放流程) +
                     dialog (对话状态机) + mcp (JSON-RPC 解析) + ota (清单/版本/e9 头/otadata 条目) +
                     display + board 规则 + protocol/aes/mqtt/udp 编解码; host 可单测 (168 个), #![deny(unsafe_code)]
```

分层规则（改代码前必读）：

- **xiaozhi-core 保持纯逻辑**：禁止引入 esp-hal 等硬件依赖；I2S/I2C 寄存器读写、DMA、
  Flash 读写等硬件代码放固件 crate。可分类/可计算的逻辑（状态机、协议解析、版本比较、
  otadata 编解码/CRC）放 core 以便 host 单测。
- **禁止硬编码 GPIO/I2C 地址**：驱动与应用只能读 `bsp::active::BOARD` 与 `Board` 组件 API；
  引脚唯一事实来源 `src/bsp/gezipai.rs`；硬件接线 `const _: () = assert!` 只允许在板子工厂内。
- **组合优先于继承**：`BoardConfig` 编译期常量，Codec 地址按角色命名（`dac_addr`/`adc_addr`）。
- **新增板子**：复制 `src/bsp/<board>.rs` → 填 `BOARD` + 实现 `create_board()` →
  `src/bsp/mod.rs` 注册两行 → Cargo.toml 加 `board-<name>` feature（选板 `compile_error!` 守卫在 mod.rs，勿删）。

## 分区表与 OTA（M10）

- `partitions.csv`（espflash 从 cwd 发现 `espflash.toml` → 读该表；改表后 `cargo run` 即生效）：

  | 分区 | offset | size | 备注 |
  | --- | --- | --- | --- |
  | nvs | 0x9000 | 0x6000 | WiFi 凭据末扇区 0xE000；音量 0xD000 |
  | phy_init | 0xF000 | 0x1000 | |
  | otadata | 0x10000 | 0x2000 | 两扇区: entry0@+0 / entry1@+0x1000 |
  | ota_0 | 0x20000 | 0x3F0000 | 首刷目标 (`target_app_partition`) |
  | ota_1 | 0x410000 | 0x3F0000 | |

- **无 factory 分区**：otadata 全 0xFF 时二级 bootloader 自愈 → 启 ota_0 并写 seq=1。
  新 seq = active+1，目标槽 = `(seq-1)%2`；烧写顺序 = 镜像 → otadata 条目（VALID 直写，
  不走 PENDING_VERIFY）→ `software_reset()`。
- **otadata 两条各占一扇区（4KB 间距），不是相邻 32B**——对齐 IDF `bootloader_common_read_otadata`；
  CRC = reflected CRC-32（init 0, xorout 0xFFFFFFFF）对 `ota_seq` 4B LE；
  纯逻辑在 `xiaozhi_core::ota`（`read_otadata`/`next_write`/`otadata_entry_offset`）。
- **OTA 由 `boot_ota_task` 独立任务执行**（main await done；采集未 start、会话任务未 spawn）：避免 flash 擦写停顿期间
  I2S RX DMA 溢出（`DmaError::Late`）。勿把 flash 写塞进 `app::run` 循环或 `session_task`。
- Flash 读写走 `esp-storage` `FlashStorage`（ROM 驱动；`features` 含 `defmt` 才能 `{:?}` 打印
  `FlashStorageError`）。同一 `FlashStorage` 实例跨 NVS/OTA 共用（`mk_static!` 单例）。

## 坑与约束

- `.cargo/config.toml` 的 `-nostartfiles` 是 **per-target**（只在 `[target.xtensa-esp32s3-none-elf]`），
  移到全局 `[build]` 会污染 host target，core 测试链接失败。
- xiaozhi-core 是 git submodule：克隆需 `--recurse-submodules`；core 改动后要在父仓库提交 submodule 指针
  （core 是独立仓库，改动先提交/推送 core 本身）。
- 日志用 defmt（`.cargo/config.toml` `[env] DEFMT_LOG=info`），打印走 esp-println。
- edition 2024，rust-version 1.88；`profile.dev` 也开 `opt-level = "s"`（勿改回）。
- 内部堆 **128KB**（`create_board` 第一行注册，先于任何分配，各 bin 共用）；
  主任务栈 = RWDATA 剩余（Channels 迁 PSRAM 后实测 **102,604B ≈100KB**，CI 守卫 `scripts/check_stack.sh` 下限 96KiB）。**opus 编码 60ms 帧需 ~87KB 栈**（SILK VLA 随帧长线性，
  `VAR_ARRAYS` 走任务栈）——缩堆或加长帧前必须重估，否则栈溢出。
  主循环 Idle+10s 读 `esp_alloc::HEAP.stats()`，`current_usage/size > 90%` 打警告。
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
- WiFi 默认常开 `PowerSaveMode::None`（长句 TTS 音质优先）；自适应「空闲 Minimum / 会话 None」放在 feature `ps-adaptive` 后（实测切换瞬间会丢 1~3 包）；deep-sleep 唤醒
  **v1.0 不做**：BOOT/VOL± 是 GPIO1/18/2，S3 上非 RTC-capable，无法做按键唤醒。
- embassy-net `StackResources` 容量按**每张表**计（`TcpSocket::new` 超容 panic → 重启循环）：
  STA 栈 `StackResources<6>`（wifi/mod.rs，DHCP+OTA+会话 TCP+UDP 音频）；SoftAP 配网表
  `StackResources<8>`（provision.rs）；wifi-test `StackResources<3>`。
  socket 跨重连复用（`session_task` 内持有），勿每次 connect 新建（泄漏耗尽内部堆）。
- `session_task` 是 embassy 任务：参数必须 `'static`（Channels 经 `mk_static!` 常驻），
  返回的 SpawnToken 必须 `spawner.spawn`（丢弃即 panic）。
- BOOT 键**开机按住** = 清除 WiFi 凭据重回配网；配网 = SoftAP `xiaozhi-gezipai`
  （WPA2，密码 `xiaozhi-setup`，启动日志会打印）+ 表单页 `http://192.168.4.1:8080/`；
  凭据+服务器 IP 存 NVS `0xE000` 扇区（magic `ZOIX`），
  音量存同分区 `0xD000` 扇区（4B 追加记录，首字节 0~100，写满 1024 条才擦除一次，避免主循环擦扇区；与凭据分离勿混写）。
- 对话状态机（`xiaozhi_core::dialog`）：`WaitingReply` 20s 无服务器消息 → 结束会话回待机。
- **MCP 已接通并真机验证**（M9，2026-09-06）：hello 发 `features:{"mcp":true}`，
  会话任务内分发 `initialize`/`tools/list`/`tools/call` 到 `app::mcp_handle` 并即时回包
  （工具：`get_device_status`/`set_volume`/`get_system_info`/`reboot`；WS 与 MQTT 共用
  `mcp_handle`，音量直接写 `PENDING_VOLUME` 待办）。原 NO_MEM 阻塞（commit 27359e0，宣告
  `mcp:true` 后服务器等 `initialize` 期间停读 TCP 流 → `esp_wifi_internal_tx
  NO_MEM(257)` 连刷）由「会话内异步应答」规避，且当前 xiaozhi-server 用非阻塞
  `create_task` 读循环，实测不再复现。
- OTA 检查是 **POST** `:8003/xiaozhi/ota/`（Device-Id 头 + JSON body）；GET 只回健康串拿不到清单（实测）。
- OTA 清单可选 `firmware.sha256` / `hash`：存在时下载过程流式 SHA-256，
  不匹配拒绝切槽；当前服务端不下发，仅做 Content-Length 校验。
  固件 URL 主机支持 IPv4 字面量或域名（DHCP DNS，5s 超时回退配置 server）。
- WS Bearer token 默认 `xiaozhi-test`，构建期 `XIAOZHI_WS_TOKEN=...` 可覆盖。
- 音量：ES8311 REG32 是 **dB 线性映射**（步进 0.5dB）。0% = REG0（-95.5dB 静音）；
  1~100% 映射到 REG91~185（-50dB ~ **-3dB**），100% 刻意不进 +dB 区——满幅 PCM
  再叠加 +32dB 会硬削顶（真机 VOL 100% 破音）。60% ≈ REG147 ≈ -22dB。
  麦克风数字电平仅 1~5% 满幅，回放/上行前软件增益 600%（+15.6dB，`GAIN_PERCENT`；
  过高会削顶毁 ASR）。`audio-loopback` 回放仍压 35% 防啸叫。
- 引脚曾误判：充电检测是 GPIO41(CHARGING)/GPIO42(DONE)；**GPIO12/13 是灯效串口**（非充电脚）。
  一切以 `src/bsp/gezipai.rs` 为准。

## opus / wakenet 集成

- **wakenet（esp-sr v1.9.5，feature 门控）真机已验证失败**（2026-08-30）：链接闭环，
  但首次 `detect` 即 InstrProhibited（PC=0x1FFE2000）——wn9 推理路径经函数指针跳到
  **MMU cache block 1**（IDF 专有的 PSRAM 窗口 0x1FC00000/0x5FC00000），esp-hal 只映射
  block 0，非垫片可修。出路（写 MMU block 1 页表 / 换 esp-sr 2.x+esp-dl / 继续 VAD）
  见 `vendor/esp-sr/README.md`。**VAD 声音唤醒（`VadWakeDetector` + `WakePolicy`）
  是默认且已验证的路径**，两层共用 `WakeWordSource` trait，切换约 5 行。
- **AEC 全双工 barge-in 推迟 v1.1**：需 ESP-SR AFE（AEC/NS/AGC）+ ES7210 TDM 参考
  通道，但 ① `vendor/esp-sr` 子集只有 wakenet、无 AFE；② 同款 NN 推理路径预期撞
  同款 MMU block 1（见上 wakenet 条目）。故 v1.0 保留按钮打断 + VAD 唤醒；做 AEC
  前先解 MMU block 1（写 block 1 页表 / 换 esp-sr 2.x+esp-dl）再引入 AFE。
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
- `ota.cc`、`mcp_server.cc`、`partitions/v2/*.csv`（双 ota 无 factory 的参考布局）

注意：它是 ESP-IDF + esp_codec_dev 栈；移植到 esp-hal 时 Codec 寄存器层以
`xiaozhi-core` 序列为准（勿照抄调用方式，勿漏上电段，见 codec.rs 注释与单测）。

## 约定与文档

- **对话回复一律用中文**（用户要求，重要：向用户输出的解释、总结、提问均用中文）。
- 注释、文档、commit message 均为中文；Conventional Commits；changelog 用 git-cliff（`cliff.toml`）。
- **Release 流程**：推 `v[0-9]+.*` tag 触发 `release.yml`（构建 + git-cliff changelog + GitHub Release）。
- pre-commit（`.pre-commit-config.yaml`）：fmt / clippy / check / cargo-deny / typos / markdownlint；
  `vendor/` 整体排除（三方资产保持上游原样）。
- 格式化 `rustfmt.toml`（max_width 100）；clippy 阈值 `clippy.toml`（栈 1024B）。
- 门禁顺序：`fmt --check` → `clippy -D warnings` → `check --all` → core 单测（cd 进子目录）。
- 开发流程 `docs/DEVELOPMENT.md`、`docs/QUICK_START.md`；阶段计划与**真机验证清单**
  `docs/ROADMAP.md`（下一步上机验证项以此为准）；引脚表见 README「引脚映射」。
- MQTT 网关部署与 UDP 加密/时间戳：`docs/MQTT_UDP.md`；换板/跨芯片移植：`docs/PORTING.md`。
- 默认关闭的**自测 feature**：`debug-diag`（高频日志）、`ota-selftest-crash`（确认前 panic）、
  `session-selftest`（开机 5s 自动唤醒）、`watchdog-selftest`（阻塞主循环验证复位）、
  `tts-selftest`（长句 TTS）、`idle-selftest`（空闲窗口缩到 5s）、
  `ps-adaptive`（WiFi 省电自适应 A/B，默认 None）。
- 门禁可用 `scripts/check.sh`（含 core fmt/clippy/nextest）；栈预算用 `scripts/check_stack.sh`。
