# 真机在环测试 (HIL) 方案

本仓库的 host 测试覆盖 `xiaozhi-core`（217 个用例），但音频时序、DMA、
WiFi 与 OTA 只能在真机验证。本文描述把"烧录后手测"变成可重复 HIL 的方案。

## 现状

- CI 只做编译、静态检查与 core host 测试；固件行为靠人工按
  [`ROADMAP.md`](ROADMAP.md) 的清单验证。
- 诊断 feature（`debug-diag`、`*-selftest`）已能输出结构化 defmt 日志，
  可直接作为 HIL 的断言来源。

## 总体思路

不做一个大 E2E，而是三层递进：

1. **目标侧 `embedded-test`**：驱动/资源级用例（PSRAM、I2C codec、
   I2S 时钟、看门狗、otadata），快且稳定；
2. **主机侧 HIL runner**：烧录 → 注入激励 → 收集事件 → 断言 → 归档；
3. **CI self-hosted runner**：label/定时触发与 artifact 归档。

被测控制通道必须与业务链路隔离：不能复用 MCP/WS 当控制面，
否则链路本身故障时无法区分原因。

## 1. 设备侧

新增 `hil` feature 与独立 `src/bin/hil-agent.rs`（不复用主固件流程）：

```text
PING / VERSION / GET_COUNTERS / RESET_COUNTERS
SET_VOLUME 60 / PLAY_TONE 1000 3000 / CAPTURE 2000
WAKE / BARGE_IN / SIM_LOW_BATTERY 15 / REBOOT
```

参数范围：`SET_VOLUME` / `SIM_LOW_BATTERY` 为 0~100；
`PLAY_TONE` 频率 20~20000 Hz、时长 1~60000 ms；`CAPTURE` 1~60000 ms。

命令解析与事件格式化已落在 `xiaozhi-core::hil`（`parse_command` /
`format_event`，host 测试覆盖）。`hil-agent` bin 只负责串口/USB 收发与
执行动作，控制协议不进入固件主循环。

当前实现（`cargo build --release --features hil --bin hil-agent`）：

- 控制通道：**USB Serial/JTAG**（独立于 UART0 日志）；
- 已支持：`PING` / `VERSION` / `GET_COUNTERS` / `RESET_COUNTERS` /
  `SET_VOLUME` / `PLAY_TONE <20-20000> <ms>` / `SIM_LOW_BATTERY` / `REBOOT`；
- 暂返回 `HIL error reason=not_implemented`：`CAPTURE` / `WAKE` / `BARGE_IN`
  （需要与主音频/会话链路合流后再实现）；
- `PLAY_TONE` 由 `xiaozhi-core::audio::tone::ToneGen` 生成任意频率；
  主固件的提示音 `beep` 仍固定 250Hz。

- 走第二路 UART 或 USB Serial/JTAG，与音频/网络链路物理隔离；
- `CAPTURE` 回传 PCM 统计（RMS/峰值/ZCR/削顶数），必要时才回传原始数据；
- 开启 `hil` feature 后只输出稳定 `key=value`：

```text
HIL boot version=1.2.3 sha=ab12cd34 psram=8388608 reset=POWER_ON
HIL dma underruns=0 late=0 overflow=0
HIL wake latency_ms=820 source=vad
HIL session state=Listening frames=42
HIL tts bytes=184320 duration_ms=3000 drop=0
HIL heap internal_used=81234 internal_total=131072
```

自由文本只作为失败附件。每个用例开始前固定 NVS/`FACTORY_RESET`，
结束时记录 `reset_reason`；设备看门狗保持开启，主机再加超时，双层兜底。

## 2. 硬件夹具

- **电气回环（首选）**：DAC → π 衰减/分压 → ADC，继电器在
  「回环/真实麦克风」间切换。必须衰减，ES8311 直连 ES7210 会削顶。
- **声学回环（夜间长稳）**：隔音箱 + USB 声卡；不要用作 PR 门禁。
- **按键/电源**：USB 继电器或光耦模拟 BOOT/音量键；可编程电源做
  低电/掉电/挂死重启；INA219/PPK2 采电流。
- **网络**：专用 AP（固定信道、关漫游、隔离外网）+ 本地测试服务器，
  ASR/LLM/TTS 返回固定可预期内容（固定时长音频），便于断言。

## 3. 主机侧 runner

独立 `hil/` crate（不污染固件 crate 与 CI 缓存）：

```text
hil/
├── src/main.rs      # 用例编排
├── src/device.rs    # espflash/probe-rs 烧录、串口、事件解析、复位
├── src/server.rs    # 固定响应的 WS/OTA/MQTT 服务
├── src/fixture.rs   # 继电器/电源/音频注入
└── src/cases/       # 每个用例一个模块
```

单用例生命周期：

```text
构建(hil feature) → 烧录 → 读 HIL boot/SHA → FACTORY_RESET →
注入激励 → 收集事件(超时) → 断言 → 保存 artifacts → 硬复位
```

断言用容差，不逐样本比对：

- DMA `underrun/late/overflow == 0`；
- 播放时长误差 < ±5%，`drop == 0`；
- 唤醒延迟、关键词确认率、固定噪声下误唤醒数；
- 状态迁移序列（hello → listen start → tts start → tts stop）；
- 堆水位峰值、栈余量、OTA 回滚复位原因。

失败分类：断言失败 = 真回归；USB 枚举/WiFi 未连 = 基础设施失败，
重试一次；挂死 = 断电重启重跑一次，仍失败保留现场。

## 4. 目标侧 embedded-test

用 `embedded-test` + `probe-rs run` 跑驱动级用例：PSRAM/堆分配、
I2C codec ACK、I2S 时钟/静音回环、看门狗复位、otadata 读写。
作为 HIL 的第一道门禁，失败时无需进入完整链路。

## 5. CI 闭环

GitHub 托管 runner 连不了真实设备，需 self-hosted：

```yaml
hil:
  runs-on: [self-hosted, esp32s3, audio-fixture]
  if: workflow_dispatch || contains(github.event.pull_request.labels.*.name, 'hil')
  concurrency: hil-device
  timeout-minutes: 30
  steps:
    - uses: actions/checkout@v7
      with: { submodules: recursive }
    - run: just image
    - run: cargo run --manifest-path hil/Cargo.toml -- --all --junit hil.xml
    - uses: actions/upload-artifact@v7
      if: always()
      with: { name: hil-artifacts, path: hil/artifacts/ }
```

- `concurrency` 保证同一设备串行；
- **fork PR 不直接进自托管 runner**（安全），用 label 或合并后
  `workflow_run` 触发；
- PR 只跑 5 分钟冒烟（启动/音频回环/会话自测），main 每晚全量 + 长稳。

## 6. 复用现有资产

`audio-loopback`、`session-selftest`、`watchdog-selftest`、
`ota-selftest-crash`、`idle-selftest`、`just image` 与本文用例清单
可直接转成第一批 HIL case。HIL 构建关闭 `debug-diag`，避免高频日志
破坏实时性。

## 7. 落地顺序

1. 第 1 周：`hil-agent` + 结构化事件 + 烧录/读版本/复位脚本；
2. 第 2 周：电气音频回环夹具，跑通启动、codec、I2S 回环、会话自测；
3. 第 3 周：self-hosted runner + JUnit/artifact + label 触发；
4. 第 4 周：OTA 回滚、看门狗、低电、MQTT 半开重连；
5. 之后：夜间长稳、声学回环、多板矩阵、功耗曲线。

## 8. 关键坑

- 音频电平失配（必须衰减）；
- USB 串口枚举/占用；WiFi 环境不可控；
- OTA 变砖——保留继电器模拟 BOOT+RST 的 USB 恢复路径；
- 日志影响实时性（HIL 构建关闭高频日志）；
- 断言过严（用容差）；
- fork PR 在带设备 runner 上执行（安全风险）。

> 当前 CI 不包含 HIL：GitHub 托管 runner 无法连接真实设备。
> 若后续接入 self-hosted runner，可把启动/音频回环/看门狗作为 PR 门禁。
