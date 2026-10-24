<!-- markdownlint-disable-file MD013 -->
# ROADMAP — 智能音箱开发计划

> 起草于 M4.2 完成后（opus 全双工上机验证通过）。当前进度与已完成能力见
> 本文「现状盘点」；分层规则与硬件约束见 `AGENTS.md`。

## 总体路线

> **状态 (2026-10-23)**：M1–M4.2 已上机验证；M5–M10 代码完成，其中
> M8/M10 的真机验收清单尚待逐项勾销。
> wakenet 特性链接闭环但真机运行时失败（MMU block 1），VAD 声音唤醒为默认路径。
> AEC barge-in 推迟 v1.1。host 单测 103/103，全 bin 编译/clippy/fmt 全绿；代码已推送 origin。
> **当前阶段 = M10 真机验证 + v1.0 发布**。

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
                 M8 真机验证与稳定化 (M5/M6/M7 上机逐项验收)
                        │
                        ▼
                 M9 功能补齐 (MCP 接线 / OTA 下载校验 / AEC→v1.1)
                        │
                        ▼
                 M10 产品化收尾 (OTA 烧写/电源/打磨/加固) ── 发布 v1.0  ← 当前
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
- 每修一个问题保持「编译 + clippy + 95 单测全绿」；真机复验通过再勾销上表。
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

**现状（2026-09-06 真机验证）**：MCP 接通并验证；OTA「查询 + 下载 + e9 头校验」
接通并验证（服务器发新版 → 设备取回并校验通过，880,176B 无损）；AEC 经评估
**推迟 v1.1**。OTA「写 flash + 切 otadata」需双 app 分区表改造，作为高风险后续
步骤（M10 稳定性/产品化一并做），未并入本里程碑。

1. **MCP 设备控制（已接通并验证 ✅）**：hello 宣告 `mcp:true`，会话任务内
   分发 `initialize`/`tools/list`/`tools/call` 到 `app::mcp_handle` 并即时回包。
   原 NO_MEM 阻塞（commit 27359e0）由「会话内异步应答」规避，且当前 xiaozhi-server
   用非阻塞 `create_task` 读循环，实测不再复现。真机：服务器日志确认 `mcp=true` +
   工具数 2（get_device_status / set_volume）。
2. **OTA 下载/烧写（下载+校验已接通并验证 ✅；写 flash+切 otadata 待分区表）**：
   `check` 改为 **POST**（`Device-Id`/`Client-Id` 头 + JSON body `board.type`
   板型 + `application.version` 当前版本；GET 只回健康串拿不到清单）→ 清单解析 →
   版本比较。有新版时 `download_validate` 流式 GET 固件 url（前 128B 进
   `validate_image_header` 校验 e9 magic + app_desc magic → 提取版本，其余只计数
   不落 RAM）。真机：服务器 `data/bin/gezipai_0.2.0.bin` → 设备日志「有新版本
   0.2.0 → 下载校验通过 880,176B」。写 ota_1 + 切 otadata + 重启需双 app 分区
   （esp-hal 无 `esp_ota` 助手，需手动 erase/write + otadata），砖片风险、串口可
   恢复，列 M10。
3. **AEC 全双工 barge-in（推迟 v1.1）**：需 ESP-SR AFE（AEC/NS/AGC）+ ES7210 TDM
   参考通道。评估结论：① 当前 `vendor/esp-sr` 子集只有 wakenet，无 AFE；② wakenet
   spike 已证 ESP-SR NN 推理路径在 esp-hal 下必撞 MMU block 1（`InstrProhibited @
   0x1FFE2000`，esp-hal 只映射 block 0），AFE 同款推理路径预期同样失败。故 v1.0
   保留按钮 barge-in + VAD 唤醒，AEC 归 v1.1（需先解 MMU block 1 再引入 AFE）。

## M10 产品化收尾与 v1.0 发布

**目标**：把可稳定对话的设备打磨成可交付产品并发布首个正式版本。

1. **OTA 烧写 + 切槽** (✅)：双 app 分区 (ota_0/ota_1, 各 4MB) + otadata 乒乓切换;
   开机检查 → 下载 → 4KB 扇区流式擦写 → 写 otadata → 重启; 无 factory 自愈。
   防护: HTTP 200 / Content-Length 完整性 / 分区上界 0x3F0000 / 45s 超时 /
   同版本失败≥3 次退避 (NVS 0xC000)。
2. **电源管理** (✅)：WiFi `PowerSaveMode::Minimum` (beacon 间 modem sleep);
   Idle 主循环 10ms 等待 (降 CPU 唤醒); deep-sleep 推迟 v1.1 (GPIO 1/18/2 非 RTC-capable)。
3. **体验打磨** (✅)：音量持久化 (NVS 0xD000, 开机 `set_volume` 落 ES8311 硬件);
   10s 堆水位诊断 (>90% 告警); 电量/充电态 10s 限频诊断日志。
4. **稳定性加固** (✅)：panic 回溯 + 软复位; 采集故障直线路径重建;
   堆水位监控。
5. **发布 v1.0** (进行中)：双 crate 0.1.0→1.0.0 已提交并推送 origin；
   推 tag 触发 `release.yml` **须真机验收通过后、且用户确认再推**。

**真机验收清单（v1.0 打 tag 前必须全过）**：

| # | 项 | 预期 | 风险关联 |
|---|---|---|---|
| 1 | 首刷新分区表 | `cargo run` 走 espflash.toml → ota_0; bootloader 自愈 seq=1; 正常进 Idle | 分区表 |
| 2 | 正常 OTA 升级 | 服务器放新版 → 日志 100% → 重启进另一槽 → 新版本号 | #1#2 |
| 3 | 截断下载 | 断网/限速中断 → 拒绝切槽, 仍跑旧版; 失败计数+1 | #2#6 |
| 4 | 超大镜像 | Content-Length>4MB → 拒绝下载 | #1 |
| 5 | 非 200 响应 | 404/500 → 不烧写 | #2 |
| 6 | 连续失败退避 | 同版本失败 3 次后第 4 次开机跳过 | #6 |
| 7 | 音量断电恢复 | 设 35% → 断电重启 → 硬件仍 35% (非 60%) | #3 |
| 8 | 音量键活跃期 | Listening 中调音量 → 无 Late/崩溃 | #7 |
| 9 | 服务器不可达 | OTA check 45s 内放弃, 正常进主循环 | #5 |
| 10 | 过夜稳定 | >8h Idle: 无重启, 堆水位告警不出现 | 加固 |
| 11 | 空闲功耗 | Idle 电流 vs 改前 (modem sleep 生效) | 电源 |
| 12 | 回归 M8 清单 | 唤醒/对话/打断/MCP/电量仍正常 | 全量 |

**已知限制（v1.0 文档须声明）**：无固件签名校验（仅 e9 头）; 无自动回滚
（VALID 直写, 坏镜像需串口恢复）; 仅受信内网 OTA。

**最终形态（v1.0）**：断电上电零操作 → 自动连网 → 语音唤醒 → 对话/播报 →
按钮打断 → 显示状态 → 电量上报 →（可选）MCP 设备控制 → OTA 自升级;
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
| M9 | MCP 接线 / OTA 下载校验 / AEC barge-in | ✅ MCP+OTA 校验（AEC 推迟 v1.1） |
| M10 | OTA 烧写 / 电源 / 打磨 / 加固 / v1.0 | 代码完成，待真机验证 |

**测试策略**：固件 crate 无 host 测试（xtensa target），一切可计算逻辑
（状态机/消息编解码/去抖/帧缓冲/版本比较）下沉 `xiaozhi-core` host 单测；
固件侧以「编译 + 链接 + clippy 全绿」为门禁，真机验证由人工在阶段边界执行。

## 真机验收记录（2026-10-24，gezipai）

### 通过

| 项 | 结果 |
| --- | --- |
| 开机自启 / 分区 / PSRAM | ota_0 启动；PCM 环 192KB 落 PSRAM；内部堆无 >90% 告警 |
| WiFi / OTA 检查 | 一次连上；OTA「已是最新 1.0.0」 |
| 语音唤醒 → 对话 | 「你好小智」→ 正常回答；开口无提示音、无杂音 |
| 多轮（安静环境，间隔 1~2s） | 同会话连续追问可正常回答 |
| 长句播放 | 4 句约 10s 连续播放 3 次，无卡顿/杂音/漏字 |
| BOOT 对讲 / 按键打断 | 按住正常回答；播报中 BOOT 立即静音并转自动聆听 |
| 音量持久化 | VOL+ 至 90% → 复位 → 开机日志「音量恢复: 90%」 |
| 服务器不可达启动 | TCP 失败 → 跳过升级 → 正常进主循环 |
| 稳定性 | 测试期间启动次数 1、无 panic/堆告警 |

### 修复要点（对应真机根因）

- I2S TX 环形常流 + 软件 PCM 队列（192KB PSRAM）+ 1s 预缓冲 + TCP 反压，
  消除逐字重复/杂音/漏字；TtsStop 排空后再回 Listening。
- PSRAM `alloc_caps` 必须清零（未初始化静音 = 白噪声）。
- 语音唤醒预录只补触发点之后的音频；BOOT 建连窗口同样补发。
- 移除运行期提示音（会漏进麦克风干扰 ASR），以呼吸灯提示（快闪=建连/等回复，中速=Listening）。
- 开机音量软启动 50%（400ms 后恢复 NVS 值），避免高音量瞬时电流掉压复位。
- 自适应 VAD（1.5×噪声+100，上限 650，4 帧去抖）+ 聆听按 60ms 子块累计。

### 已知限制 / 未测

- 立即追问无 AEC：音频可送达并识别，但设备会拾取扬声器/环境声形成短循环；
  安静环境间隔 1~2s 可稳定多轮（AEC 归 v1.1）。
- 高音量（>80%）下仍有掉压风险，硬件供电待加强。
- 过夜稳定、WiFi 断线重连、OTA 正常升级（需 1.0.1 镜像）本次未测。

### v1.1 进展（2026-10-24）

| 项 | 状态 | 验证 |
| --- | --- | --- |
| 高频诊断日志开关化 (`debug-diag`) | ✅ | 默认编译期移除 RAW/寄存器/1Hz/[T]/TX 速率 |
| OTA 正常升级 | ✅ | 1.0.1 下载 875KB → 写 ota_1 → otadata 切槽 → 从 0x410000 启动 |
| OTA 回滚保护 (PENDING_VERIFY) | ✅ | 新镜像启动 + WiFi 连通后确认 VALID（日志 `已确认运行镜像 seq=2`）|
| WiFi 断线重连 | ✅ | AP 关闭 15s：3s 退避重试 → 恢复后自动重连 → 新会话 253ms |
| 过夜稳定 | ⏭ 跳过 | 按用户决定延后 |
| 坏镜像实际回滚 | ❌ | 实测崩溃镜像会反复重启：当前 `esp-bootloader-esp-idf` 未启用 `APP_ROLLBACK_ENABLE`，PENDING 不会被引导器回滚；PENDING+启动确认已实现，崩溃镜像需串口恢复 |

**本轮修复**：OTA URL 缓冲 64→160（原 URL 截断成空导致 `GET /` 404）、
下载字节计数从 0 起算（原多算 128B 拒绝切槽）、otadata erase 的 `to` 参数、
确认时按 4KB 间距读取两条目、开机音量软启动、运行期提示音移除。

**发布注意**：快速改版本号构建可能复用缓存导致镜像版本未更新（真机出现重复升级）；
发布前必须 clean build 并核对 app_desc 版本。

### v1.1 P1/P2 进展（2026-10-24 晚）

| 项 | 状态 | 说明 |
| --- | --- | --- |
| 二进制协议 v2/v3 编解码 | ✅ | core `protocol`：v2 16B 头（含时间戳）/ v3 4B 头 + `AecClock`，host 单测 |
| WS 协议版本可配 | ✅ | NVS 0xB000（默认 1）；v2/v3 需服务端支持 |
| MQTT + UDP 传输 | ✅ | 真机验证：MQTT 控制 + UDP 音频（AES-128-CTR）、服务端 AEC 时间戳、TTS 下行播放 |
| MCP 工具扩展 | ✅ | `self.get_system_info` / `self.reboot`，工具列表 4 个 |
| 播放时间戳对齐 | ✅ | `PlayoutTimestamps`：按实际播出字节推进，一帧播完才进 AEC 时钟 |
| 过期 goodbye 过滤 | ✅ | 按 `session_id` 忽略旧 bridge 通知；本地 20s 超时主动 goodbye |
| 内部堆优化 | ✅ | MQTT 大缓冲迁 PSRAM，水位 98% → <90% |
| 主循环看门狗 | ✅ | TIMG0 MWDT 8s，真机 `rst:0x7 (TG0WDT_SYS_RST)` 验证 |
| OTA 低电保护 | ✅ | 电量 <20% 且未充电跳过升级（防中途掉电） |
| WS 路径回归 | ✅ | 擦除 NVS 0xA000 → 服务器不下发 mqtt → 走 WS v1，79ms 建连、状态机正常 |
| MQTT 配置自愈 | ✅ | 擦除后重新开机，OTA 重新下发并写入 NVS，当次启动即选择 MQTT+UDP |
| 网络半开保护 | ✅ | 读写 10s / 握手 15s 超时 + TCP 状态检测；网关重启后 3s 退避重连成功 |
| 多板（同芯片） | ✅ 路径就绪 | BSP 四步法 + `docs/PORTING.md`；另一块 S3 板可直接照做 |
| 多板（C3/C6/P4） | ⏳ 未移植 | 需独立 crate/target 与硬件；差异清单见 `docs/PORTING.md` |
| 立即追问（无 AEC） | ⚠️ 依赖 MQTT 网关 | 直连 WS 服务端不解析 16B 头；MQTT+UDP 下服务端 AEC 已生效 |

**MQTT+UDP 部署与验证细节**：见 [`docs/MQTT_UDP.md`](MQTT_UDP.md)。

**本轮新增 feature**（均默认关，仅用于无人值守自测）：

- `session-selftest`：开机 5s 自动触发一次唤醒，验证传输/播报链路；
- `watchdog-selftest`：启用 RWDT 后阻塞主循环 12s，验证 8s 看门狗复位。

**已知限制**：

- MQTT 为明文 1883，仅建议受信内网；设备侧 TLS 待后续引入；
- 设备激活 HMAC / OTA 签名：自托管服务器不下发签名清单、也无激活服务端，
  设备侧当前只记录 `activation.code`，待接入对应服务后实现；
- 无真人说话时不能自动进入会话（自测 feature 可绕过）；
- 高音量（>80%）掉压、过夜稳定仍未测（见上表）。
