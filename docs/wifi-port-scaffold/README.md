<!-- markdownlint-disable-file MD013 -->
# WiFi 集成配对研究（esp-hal 1.1.2 稳定线）

本目录是 esp-wifi/esp-radio 集成的**脚手架与研究成果**（不参与编译）。
`wifi-test.rs.old-api`、`mod.rs`、`provision.rs`、`net.rs`、`store.rs` 是按
**旧 esp-wifi 0.13 API** 写的实现草稿，逻辑可复用，API 需移植到 esp-radio。

## 结论：依赖配对矩阵（逐版本核实过 crates.io 索引）

| esp-wifi 版本 | 要求的 esp-hal | 结论 |
|---|---|---|
| 0.12.0 | ^0.23.0 | 仅配旧 0.x 线 |
| 0.13.0/0.14.x/0.15.x | ^1.0.0-beta.x 起 | **源码级不兼容 esp-hal 1.1.2**（引用 `RadioClockController`/`sync::Locked`/`RADIO_CLK` 等 1.1.2 已移除的 API；xtensa-lx-rt 版本错位 0.18/0.19/0.20 vs 1.1.2 的 0.22 → 构建恐慌） |

**正确路径：esp-wifi 在 1.x 时代已改名 [`esp-radio`](https://crates.io/crates/esp-radio)**：

| esp-radio | esp-hal | 说明 |
|---|---|---|
| 0.17.0 | ^1.0.0 + unstable | |
| 0.18.0 | ~1.1.0-rc.0 | |
| **1.0.0-beta.0** | **~1.1.0** | **与本仓库 esp-hal 1.1.2 配对 ✓（已验证 cargo 解析通过）** |

⚠️ crates.io 的 esp-wifi 0.13~0.15 全部无法与 esp-hal 1.1.2 稳定线组合——勿再尝试。

## esp-radio 1.0.0-beta.0 集成要点（源码已核实）

依赖三元组：

```toml
esp-radio = { version = "1.0.0-beta.0", features = ["esp32s3", "wifi", "esp-alloc", "defmt"] }
esp-rtos  = { version = "0.3.0", features = ["esp32s3", "esp-radio", "esp-alloc"] }
# esp-alloc 已有
```

官方 quick start（esp-radio src/lib.rs 顶部文档）：

```rust
use esp_hal::interrupt::software::SoftwareInterruptControl;
let timg0 = TimerGroup::new(peripherals.TIMG0);
let sw_interrupt = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
// 必须在 radio init 之前启动调度器:
esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);
let controller = esp_radio::wifi::WifiController::new(peripherals.WIFI, Default::default())?;
```

- `esp-radio/src/wifi/mod.rs`：`WifiController::new(device, ControllerConfig)`、
  `is_connected()`（阻塞）、`Interface::station()` / `access_point()`（构造
  embassy-net-driver-02 的网络设备）、`mac_address()`
- 网络栈：`embassy-net 0.7`（配 embassy-net-driver 0.2）+ `embassy-executor`
  （Runner 需驱动）；参考 esp-hal 仓库 examples（wifi_dhcp 等，与 1.1 同代）
- 内存：WiFi 从全局堆分配；官方示例内部堆 ~100KB 起步（我们现有 160KB 内部堆
  需与音频缓冲 50KB 共存，必要时调 esp_radio 的 ControllerConfig 缓冲参数）
- 仿真优化：`[profile.dev.package.esp-radio] opt-level = 3`

## 已写好待移植的逻辑（本目录）

- `store.rs`：配网凭据 Flash 持久化（NVS 分区末扇区 0xE000，TLV 格式）— 与网络栈无关，可直接用（esp-storage 0.8.1）
- `provision.rs`：SoftAP `xiaozhi-gezipai` + 手写 DHCP 服务器（固定租约 192.168.4.2）+ 表单页 `<http://192.168.4.1/>` — 需移植到 esp-radio AP 模式 + embassy-net UDP/TCP
- `net.rs`：STA 网络栈（DHCP 客户端 + 阻塞 TCP GET）— 需改为 embassy-net Stack::poll 驱动
- `wifi-test.rs.old-api`：整体流程编排（读凭据→配网→STA→DHCP→探测 OTA `:8003/xiaozhi/ota/`）

## 本地服务器（已部署）

- 目录：`/home/mrpeng/xiaozhi-server/`（docker compose，容器 xiaozhi-esp32-server 9.0.6）
- WebSocket：`ws://192.168.123.14:8000/xiaozhi/v1/`
- OTA 探测：`http://192.168.123.14:8003/xiaozhi/ota/`（已验证可达）
- ASR 本地 SenseVoiceSmall ✓；TTS EdgeTTS ✓；**LLM 需用户提供智谱 API key**（`.config.yaml` 的 ChatGLMLLM api_key）

## 下一步（M4 续）

1. 按 esp-radio API 移植 wifi 模块（init/connect）
2. embassy-net 0.7 + executor 集成（阻塞主循环改 embassy 或 hybrid）
3. 配网模块移植（SoftAP + DHCP server + 表单）
4. 协议握手（对照 C++ `main/protocols/`）与 opus 选型
