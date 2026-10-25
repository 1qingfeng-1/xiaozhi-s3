# ADR-0005: 端口/适配器与主循环可测试化（规划）

- 状态: 提议
- 日期: 2026-10-25
- 决策者: 维护者

## 背景

`xiaozhi-core` 覆盖了所有纯逻辑并有 188 个 host 测试，但固件的编排层
（`src/app.rs` 的 `run()`）直接依赖 `esp-hal` 类型，无法在 host 上测试。
主循环里还有较多布尔标志（`keyword_gate`、`preroll_flush`、`mic_mute_until_ms`、
`volume_dirty` 等），隐式状态机导致 `clippy::excessive_nesting` 集中出现。

## 决策（分阶段）

1. **显式阶段枚举**：把布尔组合收敛为 `LoopPhase`
   （`Gated / Idle / Connecting / Listening / WaitingReply / Speaking`），
   首先用于 LED/显示与日志，不改变音频路径。
2. **端口 trait**：只为会变化/会失败的三类边界定义 trait：
   - `AudioCapture` / `AudioPlayback`（I2S + codec）
   - `Transport`（WS / MQTT 已事实二分）
   - `Storage`（NVS / 未来 sequential-storage）
   BSP 提供实现；host 测试用 mock 验证"唤醒 → 建连 → 关键词门控 → 收尾"时序。
3. **纯函数 tick**：`tick(now, inputs) -> Actions` 放 core，
   固件只负责执行副作用。与现有 `DialogSm` 的 Action 模式一致。
4. 不为 GPIO/时钟等"不会变化"的 API 建 trait。

## 后果

### 正面

- 编排层可在 host 回归，减少真机调试成本。
- 嵌套告警可通过结构消除，而非 `#[allow]`。
- 多板/多传输复用边界清晰。

### 负面 / 代价

- 需要一次较大的 `app.rs` 重构，必须在真机上做完整回归
  （音频时序、DMA、唤醒、OTA 都不能退化）。
- trait 对象/泛型可能增加代码体积与调用开销，需测量 Flash 与实时性。

## 备选方案

保持现状，仅对 `run()` 加 `#[allow(clippy::excessive_nesting)]`。
代价是可测试性长期缺失，且告警掩盖真实复杂度增长。

## 当前状态

- 步骤 1 **已落地**：`dialog::LoopPhase`（门控优先、低功耗只影响 Idle）
  提供 host 测试；固件灯语改为单一 `match phase`，删除布尔组合。
- `stt_has_keyword` 已从固件移到 core，并补了误识别变体测试。
- 聆听回合的「能量 + ZCR」统计迁入 `listen::voiced_ms`，固件不再保留
  重复判定循环，该规则现在有 host 测试覆盖。
- 上行前处理（增益 + 限幅）统一为 `capture::condition_for_uplink`，
  聆听 / 播报 AEC / 预录补发三条路径共用同一顺序。
- 定长帧组装迁入 `encode::FrameQueue`；`encode_uplink` 不再需要
  `excessive_nesting` allow，跨轮余量/多帧行为有 host 测试。
- 已消除可安全修复的 lint；`run()` / `encode_uplink()` 仍保留带说明的
  `#[allow(clippy::excessive_nesting)]`。
- 步骤 2 **部分落地**：`core::port` 提供 `PcmSource` / `PcmSink` /
  `CaptureConfig` / `PortError` 与 `pump_capture`，并用 mock 覆盖增益、
  限幅、截断、空读与错误传播。**尚未接入 BSP 与 `app::run`**，
  因此现有音频路径不变。
- 剩余工作：BSP 适配器实现、`run()` 切换到 `pump_capture`/端口、
  步骤 3（纯函数 tick）与真机音频回归。
