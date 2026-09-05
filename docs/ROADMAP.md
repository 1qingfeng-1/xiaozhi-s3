<!-- markdownlint-disable-file MD013 -->
# ROADMAP — 智能音箱开发计划

> 起草于 M4.2 完成后（opus 全双工上机验证通过）。当前进度与已完成能力见
> 本文「现状盘点」；分层规则与硬件约束见 `AGENTS.md`。

## 总体路线

> **状态 (2026-09-04)**：M1–M4.2 已上机验证；M5/M6/M7 代码全部完成
> （host 单测 77/77，全 bin 编译/clippy/fmt 全绿，release 可烧录）。
> wakenet 特性链接闭环但真机运行时失败（MMU block 1），VAD 声音唤醒为默认路径。
> **当前阶段 = M8 真机验证与稳定化**（把 M5/M6/M7 在设备上逐项验收）。
> 之后 M9 补齐 MCP/OTA/AEC，M10 产品化并发布 v1.0。

```text
M1 音频通路 ── M2 WiFi/配网 ── M4.1 WS 协议 ── M4.2 opus 全双工
                                                        │
                        ┌───────────────────────────────┘
                        ▼
                 M5 语音唤醒 (VAD 默认 / WakeNet feature)
                        │
                        ▼
                 M6 整机集成 (状态机/主固件/灯效/打断)
                        │
                        ▼
                 M7 产品化 (MCP/OTA 检查/显示/电池, AEC 评估)
                        │
                        ▼
                 M8 真机验证与稳定化 (M5/M6/M7 上机逐项验收)   ← 当前
                        │
                        ▼
                 M9 功能补齐 (MCP 接线 / OTA 下载烧写 / AEC barge-in)
                        │
                        ▼
                 M10 产品化收尾 (深睡/打磨/加固) ── 发布 v1.0
```

## M8 真机验证与稳定化（进行中）

**目标**：M5/M6/M7 代码已全部就绪，本阶段在真机上逐项验收，把「编译/链接/
单测全绿」提升到「设备上稳定可用」。全部验收通过后进入 M9。

**验收项**（烧录 `xiaozhi-s3` 主固件后逐项确认）：

| # | 项 | 预期现象 | 类型 |
|---|---|---|---|
| 1 | 开机自启 | 上电 → 读 NVS 凭据 → STA 连 WiFi → 进入待机监听 (Idle)，**不自动连服务器**（WS 会话在唤醒时才建，per-conversation）；VAD 在原始电平域 + 1s 预热期，开机无假唤醒 | 自动 |
| 2 | 声音唤醒 (VAD) | 待机说话 → beep + 快呼吸灯 → 自动建连 → 进入 Listening | 需说话 |
| 3 | 对话闭环 | Listening 上行 opus → WaitingReply → TTS 下行播报 (Speaking) → 回 Idle；无应答 10s 超时回 Idle | 需说话 |
| 4 | 打断 (barge-in) | Speaking 中按 BOOT → `tts abort` → 回 Listening；验证 Manual 模式标记 (post-transition 读) | 需按键+说话 |
| 5 | 显示 | 状态行随阶段刷新 IDLE/CONN/LISTEN/WAIT/TTS；ST7789 反显/偏移正常 | 观察 |
| 6 | 配网/凭据 | 首次 SoftAP 表单 → 存 NVS → 重启 STA 自动连；BOOT 开机长按清凭重回配网 | 需操作 |
| 7 | 断网/重连 | 断网/改 SSID → 自动重连；socket 跨重连复用不耗尽堆；无重启循环 | 需操作 |
| 8 | OTA 检查 | 启动日志打印服务器固件版本 + 本地版本比较结果（已是最新 / 发现新版） | 自动 |
| 9 | 电池 | Idle 每 10s 读电量、充电态 GPIO 实时；**确认不再崩 `pm_on_beacon_rx`** | 自动 |
| 10 | 回环回归 | `audio-loopback` bin：采集 → 增益 → VAD → 回放 (35% 防啸叫) | 需说话 |
| 11 | 无人值守 | 挂机过夜不崩不僵（看门狗/panic 自恢复）；无 `esp_wifi_internal_tx` 连刷 | 观察 |

**执行协议**：

- 烧录主固件：`espflash flash --monitor --chip esp32s3 -p /dev/ttyACM0 \
  target/xtensa-esp32s3-none-elf/release/xiaozhi-s3`（defmt 日志经串口回读）。
- 自动类（1/5/8/9）直接看启动日志判定；说话/按键/操作类（2/3/4/6/7/10）需人工
  配合，每步记录现象 → 异常定位 → 小步修复提交 → 复验。
- 每修一个问题保持「编译 + clippy + 77 单测全绿」；真机复验通过再勾销上表。
- 11 项全过 + 无人值守过夜 OK → M8 完成。

> **关键词唤醒 (wakenet) 不在 M8 范围**：真机运行时失败（wn9 经函数指针跳
> MMU block 1，见 AGENTS.md 与 `vendor/esp-sr/README.md`），VAD 为默认路径；
> wakenet 作为可选增强留待 M9 评估（写 MMU 页表 / 换 esp-sr 2.x+esp-dl）。

---

## M5 语音唤醒

**目标**：待机状态下「说话即唤醒」，自动进入对话；关键词「你好小智」作为可选增强。

**Spike 结论（esp-sr 链接验证，已完成）**：

- 选 v1.9.5（纯 C 闭包，无 esp-dl C++ 框架依赖；master/2.5.1 的 wakenet 依赖
  esp-dl 3.x C++ 运行时，闭包过重）。
- 链接闭包 = 4 个预编译库（wakenet/dl_lib/hufzip/c_speech_features）+
  esp-dsp 源码子集（fft2r/fft4r/bitrev/cplx2real/dotprod）+ esp-dl 的
  kiss_fft/vvadd + cjson + shim（log/heap_caps/文件 IO/assert/printf）+
  newlib(libc/libm/libgcc)。
- 模型加载：`model_path.c` 是源码可整体替换（绕开 SPIFFS）；hufzip（闭源）
  经 POSIX `fopen/fread` 读模型文件 → 以内存文件表 shim。
- **深水区**：wakenet 内部以 FreeRTOS 任务跑 MFCC（`mfcc_runner_task`，闭源，
  任务语义不可见），且模型解压/内存布局等运行时行为在无真机条件下不可验证。

**交付分层**：

| 层 | 内容 | 验证 |
|---|---|---|
| 声音唤醒（默认，上机即用） | 能量 VAD 持续人声检测 + 唤醒策略（去抖/冷却/抑制），纯逻辑进 core | host 单测全覆盖 |
| 关键词唤醒（feature `wakenet`） | esp-sr v1.9.5 vendoring + shim + 模型分区 + 烧录脚本 | 链接级验证；运行时留待真机阶段 |

两层共用同一检测器 trait 与唤醒流程，切换只改实现。

**任务分解**：

1. `xiaozhi-core::audio::wake`：检测器 trait 与唤醒策略状态机（去抖/冷却），
   含 VAD 实现，host 单测。
2. 固件接入：时钟全程常开（待机也维持监听）；Idle 循环喂检测器；
   触发 → 提示音 + LED → 连接 WS → listen start（对齐 C++ 语义）。
3. feature `wakenet`：vendor esp-sr v1.9.5 库 + shim 层（链接级），
   模型分区与烧录脚本，运行时验证留待真机阶段。

**验收**（真机）：安静环境说话 1s 内唤醒；夜间无人值守误唤醒可控
（冷却与阈值参数化）；唤醒 → 可说话 < 1.5s。

## M6 整机集成

**目标**：`xiaozhi-s3` 主 bin 成为完整产品固件，上电即用。

1. **对话状态机进 xiaozhi-core**（host 单测）：`Idle / Connecting / Listening /
   Speaking` + 事件（唤醒、按键、tts start/stop/abort、断线）。
2. **会话代码模块化**：WS/协议/音频会话从 `xiaozhi-cloud.rs` 抽成
   `src/session.rs` + `src/protocol.rs`，调试 bin 与主固件共用。
3. **打断与终止**：Speaking 中按 BOOT → 发 `tts abort` 回 Listening；
   auto 模式说完自动回 Idle。
4. **按键与灯效**：VOL+/VOL- 音量；LED 状态映射（待机呼吸/对讲常亮/播报闪烁）。
5. **健壮性**：WiFi/WS 断线重连、看门狗、panic 自恢复。

**验收**（真机）：断电上电零操作，唤醒→对话→播报→打断→再唤醒闭环，
无人值守一夜不崩不僵。

## M7 产品化增强（各项独立，可穿插）

| 项 | 内容 | 备注 |
|---|---|---|
| MCP 设备控制 | hello 宣告 `mcp:true`，实现 JSON-RPC over WS：音量/状态查询 | 消息编解码进 core 单测 |
| OTA 升级 | HTTP 检版本→下载→写 ota_1→切 otadata；需双 app 分区（M5 布分区时一次布好） | 对齐 C++ `ota.cc`；版本比较/解析进 core 单测 |
| 显示 ST7789 | SPI 驱动 + 状态图标/对话文本 | framebuffer/字库进 core 单测；BSP 引脚已就位 |
| 电源管理 | 电池电量上报（MCP getDeviceStatus 返回）、可选深睡 | 组件已有 |
| AEC 全双工 | ESP-SR AFE AEC + ES7210 TDM 参考通道，支持说话打断 (barge-in) | 依赖 AFE（FreeRTOS 任务语义），工作量/风险最大，视 M5 spike 结果定案；无真机环境下仅做到「接口与配置就绪」 |

## M9 功能补齐（M7 遗留 / 未接线项）

**目标**：把 M7 中「代码就绪但未接线 / 未真机验证」的能力补齐到端到端可用。
各项独立，可按风险/价值取舍，不阻塞 M8 稳定化。

1. **MCP 设备控制**（hello 宣告 `mcp:true` + JSON-RPC over WS）：接通
   `app::mcp_handle`。已知阻塞（commit 27359e0）：宣告 `mcp:true` 后服务器等
   `initialize` 应答期间停读 TCP 流 → WiFi TX 池耗尽（`esp_wifi_internal_tx
   NO_MEM(257)`）。**对策**：会话任务内异步构造并下发 `initialize`/`tools/list`
   应答（不阻塞 TCP 读循环）；对齐 C++ `mcp_server.cc`。验收：语音「把音量调到
   50」→ 设备 set_volume 生效。
2. **OTA 下载/烧写**（现在只有版本检查）：双 app 分区（ota_0/ota_1）+ otadata
   改造；下载固件 → 校验 → 写 ota_1 → 切 otadata → 重启。分区表一次性布好。
   对齐 C++ `ota.cc`。验收：服务器发新版 → 设备自动下载重启到新版。
3. **AEC 全双工 barge-in（风险最高，需 spike 前置）**：ESP-SR AFE（AEC/NS/AGC）
   与 ES7210 TDM 参考通道，支持**说话打断 TTS**（真正的 barge-in，非按钮）。
   风险同 wakenet（AFE 经 FreeRTOS 任务/函数指针，可能撞 MMU block 1）。
   **spike 先验**：最小 AFE 链路能否在 esp-hal 下跑通。失败则 v1.0 保留按钮
   打断 + VAD 唤醒，AEC 归入 v1.1。

## M10 产品化收尾与 v1.0 发布

**目标**：把可稳定对话的设备打磨成可交付产品并发布首个正式版本。

1. **电源管理**：待机深睡（deep-sleep）降功耗，评估唤醒源（RTC 定时 / 触摸 / 光感）；
   唤醒后快速恢复 WiFi + 会话。
2. **体验打磨**：启动流程（LED/显示状态）、错误提示（配网失败/服务器不可达/电量低）、
   音量与凭据持久化、首次引导。
3. **稳定性加固**：看门狗全覆盖、内存/堆水位监控、关键路径日志降级（量产关 debug）。
4. **发布 v1.0**：`v1.0.0` tag 触发 `release.yml`（构建 + git-cliff changelog +
   GitHub Release）；补 README 用户文档（配网指南/引脚表/故障排查）。

**最终形态（v1.0）**：断电上电零操作 → 自动连网 → 语音唤醒 → 对话/播报 →
按钮打断 → 显示状态 → 电量上报 →（可选）MCP 设备控制 → OTA 自升级；
无人值守过夜稳定。

## 现状盘点（M7 代码完成时）

| 里程碑 | 能力 | 状态 |
|---|---|---|
| M1 | I2S 全双工音频通路（ES8311 播放 + ES7210 采集，回环验证） | ✅ 上机验证 |
| M2 | WiFi 配网（SoftAP + 表单页）、Flash 凭据存储 | ✅ 上机验证 |
| M4.1 | WebSocket 协议层（握手/hello/listen/重连） | ✅ 上机验证 |
| M4.2 | opus 全双工（mic→opus 上行、TTS opus→播放），上机对话验证通过 | ✅ 上机验证 |
| M5 | 声音唤醒（VAD 默认 / wakenet feature 链接闭环） | 代码完成，待 M8 上机 |
| M6 | 对话状态机 / 主固件 / 灯效 / 打断 | 代码完成，待 M8 上机 |
| M7 | MCP（未接线）/ OTA 检查 / ST7789 显示 / 电池上报 | 代码完成，MCP/OTA 下载待 M9 |
| M8 | 真机验证与稳定化（11 项验收） | 进行中 |
| M9 | MCP 接线 / OTA 下载烧写 / AEC barge-in | 待办（AEC 需 spike） |
| M10 | 深睡 / 打磨 / 加固 / v1.0 发布 | 待办 |

**测试策略**：固件 crate 无 host 测试（xtensa target），一切可计算逻辑
（状态机/消息编解码/去抖/帧缓冲/版本比较）下沉 `xiaozhi-core` host 单测；
固件侧以「编译 + 链接 + clippy 全绿」为门禁，真机验证由人工在阶段边界执行。
