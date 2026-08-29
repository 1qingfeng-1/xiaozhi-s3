<!-- markdownlint-disable-file MD013 -->
# ROADMAP — 智能音箱开发计划

> 起草于 M4.2 完成后（opus 全双工上机验证通过）。当前进度与已完成能力见
> 本文末尾「现状盘点」；分层规则与硬件约束见 `AGENTS.md`。

## 总体路线

> **状态 (2026-08-30)**: M5/M6/M7 全部代码完成, host 单测 77/77 通过,
> 全 bin 编译/链接/clippy 全绿。wakenet 特性链接级闭环, 运行时验证
> 与 AEC 留待真机阶段。待办清单见文末「真机验证清单」。

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
```

## 真机验证清单 (下次上机)

1. **声音唤醒**: 待机说话 → 提示音 + 快呼吸灯 → 自动建连 → auto 对话;
   调 `app.rs::VAD_THRESHOLD` 与 `WakePolicyConfig` 平衡灵敏度/误唤醒。
2. **关键词唤醒**: `--features wakenet` 烧录 → 看启动日志 `wakenet 自检通过`
   → 说「你好小智」验证检出; 失败场景与对策见 `vendor/esp-sr/README.md`
   (mfcc 任务桥接/阈值上调)。
3. **MCP**: 服务器端语音指令 "把音量调到 50" → 观察 set_volume 调用日志。
4. **OTA 检查**: 启动日志应打印服务器版本与「已是最新」; 下载烧写待分区表改造。
5. **显示**: 状态行随对话阶段刷新 (IDLE/CONN/LISTEN/WAIT/TTS);
   ST7789 初始化参数 (反显/偏移) 如异常按面板规格调整 `components/display.rs`。
6. **回归**: audio-loopback 回环、BOOT 长按清配网、配网页表单、断网重连。

执行原则：每个阶段保持「编译 + host 单测全绿」的小步提交；技术风险最高的
部分（esp-sr 链接）用 spike 前置验证；真机验证集中在阶段边界进行。

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
| MCP 设备控制 | hello 已广播 `mcp:true`，实现 JSON-RPC over WS：音量/状态查询 | 消息编解码进 core 单测 |
| OTA 升级 | HTTP 检版本→下载→写 ota_1→切 otadata；需双 app 分区（M5 布分区时一次布好） | 对齐 C++ `ota.cc`；版本比较/解析进 core 单测 |
| 显示 ST7789 | SPI 驱动 + 状态图标/对话文本 | framebuffer/字库进 core 单测；BSP 引脚已就位 |
| 电源管理 | 电池电量上报（MCP getDeviceStatus 返回）、可选深睡 | 组件已有 |
| AEC 全双工 | ESP-SR AFE AEC + ES7210 TDM 参考通道，支持说话打断 (barge-in) | 依赖 AFE（FreeRTOS 任务语义），工作量/风险最大，视 M5 spike 结果定案；无真机环境下仅做到「接口与配置就绪」 |

## 现状盘点（M4.2 完成时）

| 里程碑 | 能力 |
|---|---|
| M1 | I2S 全双工音频通路（ES8311 播放 + ES7210 采集，回环验证） |
| M2 | WiFi 配网（SoftAP + Vue 页面）、Flash 凭据存储 |
| M4.1 | WebSocket 协议层（握手/hello/listen/重连） |
| M4.2 | opus 全双工（mic→opus 上行、TTS opus→播放），上机对话验证通过 |

**测试策略**：固件 crate 无 host 测试（xtensa target），一切可计算逻辑
（状态机/消息编解码/去抖/帧缓冲/版本比较）下沉 `xiaozhi-core` host 单测；
固件侧以「编译 + 链接 + clippy 全绿」为门禁，真机验证由人工在阶段边界执行。
