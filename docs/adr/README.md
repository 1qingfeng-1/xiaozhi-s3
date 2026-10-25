# 架构决策记录 (ADR)

记录“为什么这样设计”，避免同类讨论反复出现。新增 ADR 时复制模板：

```markdown
# ADR-NNNN: 标题

- 状态: 提议 / 已接受 / 已废弃 / 被 ADR-XXXX 取代
- 日期: YYYY-MM-DD
- 决策者: ...

## 背景

## 决策

## 后果

### 正面

### 负面 / 代价

## 备选方案
```

| 编号 | 标题 | 状态 |
|---|---|---|
| [0001](0001-core-submodule.md) | core 使用 git submodule 而非 workspace | 已接受 |
| [0002](0002-dual-transport.md) | WS 与 MQTT+UDP 双传输并存 | 已接受 |
| [0003](0003-diagnostic-features.md) | 诊断开关保留为 Cargo feature 的约束 | 已接受 |
| [0004](0004-no-ota-signature-yet.md) | OTA 暂只做 CRC，不启用签名 | 已接受（待修订） |
| [0005](0005-port-traits-plan.md) | 端口/适配器与主循环可测试化 | 提议 |
