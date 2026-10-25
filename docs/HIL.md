# 真机在环测试 (HIL) 方案

本仓库的 host 测试覆盖 `xiaozhi-core`（188 个用例），但音频时序、DMA、
WiFi 与 OTA 只能在真机验证。本文描述把"烧录后手测"变成可重复 HIL 的最小方案。

## 现状

- CI 只做编译、静态检查与 core host 测试；固件行为靠人工按
  [`ROADMAP.md`](ROADMAP.md) 的清单验证。
- 诊断 feature（`debug-diag`、`*-selftest`）已能输出结构化 defmt 日志，
  可直接作为 HIL 的断言来源。

## 目标

1. 一台常连 USB 的 ESP32-S3 + 一个测试服务器；
2. `cargo test` 风格的用例：烧录 → 等日志/状态 → 断言 → 复位；
3. 失败时保留串口日志与复位原因；
4. 先本地手动跑，稳定后再考虑 self-hosted runner。

## 推荐工具

| 工具 | 用途 |
|---|---|
| `espflash` | 烧录、`--monitor` 输出 defmt |
| `probe-rs` | 备选调试/烧录通道（USB-JTAG） |
| `embedded-test` 或 `defmt-test` | 目标侧测试框架（在设备上跑 `#[test]`） |
| `defmt-test` + `probe-rs run` | 无人值守执行并回收结果 |

## 用例清单（按收益排序）

1. **启动自检**：复位原因、PSRAM 大小、内部堆水位、codec I2C 应答；
2. **音频环回**：`audio-loopback` bin 播放正弦，检查 TX 速率与欠载计数；
3. **唤醒门控**（`session-selftest`）：注入唤醒 → 检查关键词确认/静默挂断；
4. **会话链路**：hello → listen start → TTS 下行 → tts stop，断言状态迁移；
5. **看门狗**（`watchdog-selftest`）：阻塞主循环，断言复位原因为 TG0WDT；
6. **OTA 回滚**（`ota-selftest-crash`）：确认前 panic，断言回滚到旧镜像；
7. **低电保护**：模拟电量 <20%，断言跳过升级。

## 落地步骤

1. 在仓库外建 `hil/` 测试工程（避免污染固件 crate 与 CI 缓存）；
2. 用 `espflash save-image` 产出镜像，测试脚本负责烧录与日志匹配；
3. 断言只依赖 defmt 的稳定字段，不解析自由文本；
4. 每个用例复位后重新烧录，避免状态串扰；
5. 真机回归记录写回 `ROADMAP.md`（保留日期、板型、固件版本）。

> 当前 CI 不包含 HIL：GitHub 托管 runner 无法连接真实设备。
> 若后续接入 self-hosted runner，可把本文用例 1/3/5 作为 PR 门禁。
