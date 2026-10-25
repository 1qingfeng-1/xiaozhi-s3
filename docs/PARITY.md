<!-- markdownlint-disable-file MD013 -->
# 功能对齐清单 — C++ 参考工程 vs 本固件

> 对照对象：`/home/mrpeng/Documents/xiaozhi-esp32`（官方 xiaozhi-esp32，ESP-IDF/C++，
> MIT），以及同板 `main/boards/gezipai`。盘点日期 2026-10-25。
>
> 目标不是逐行移植，而是**同一块 gezipai 硬件上用户可感知的功能对齐**。
> 语音链路受 esp-hal MMU block 1 限制（WakeNet/AFE 推理跑不起来），见
> `AGENTS.md` 与 `vendor/esp-sr/README.md`，相关项单独标注。

## 状态图例

- ✅ 已对齐（行为等价）
- 🟡 部分对齐 / 有替代实现
- ⬜ 未实现
- ⛔ 当前硬件/工具链受限（需先解决 MMU 或没有对应外设）

## 协议与会话

| 能力 | 状态 | 说明 |
|---|---|---|
| WS v1/v2/v3、MQTT+UDP | ✅ | 与 C++ 相同的三种二进制帧与 UDP AES-128-CTR |
| `hello` / `listen start/stop` / `tts start/stop` | ✅ | 与 C++ 一致 |
| `abort` 打断 | ✅ | 2026-10-25 修正：必须是 `{"type":"abort"}`，`tts/state=abort` 服务端会忽略 |
| `stt` 显示 | ✅ | 解析并缓存文本（屏幕 UI 批次渲染） |
| `llm.emotion` | ✅ | 缓存情绪；当前以 `EMO <name>` 通知显示 |
| `tts.sentence_start` | ✅ | 解析并缓存朗读文本 |
| `alert` | ✅ | 显示 `ALERT` 通知 + 播放提醒音（播报中只显示不插音） |
| `system.reboot` | ✅ | 与 MCP reboot 共用延迟复位路径 |
| `error` | ✅ | 触发错误提示音并结束会话 |
| `goodbye`（MQTT，按 session_id 过滤） | ✅ | 建连期/过期通知忽略 |
| `custom` | 🟡 | 仅记录日志（C++ 默认也不显示） |
| 设备主动 `goodbye`（MQTT 关会话） | ✅ | 复用 `GOODBYE` 常量 |
| `listen detect`（唤醒词音频上传） | ⛔ | 依赖 WakeNet；当前 VAD 唤醒 + 本地「我在」 |
| realtime 聆听（播报中持续上行 + 服务端 AEC） | ✅ | `server_aec` 为真时 Speaking 持续上行，服务端 VAD 收尾 |
| 激活码 / 激活挑战（HMAC） | ⬜ | 接官方 xiaozhi.me 时需要 |
| OTA 下发 WebSocket 配置 | ⬜ | 当前只消费 `mqtt` 对象 |
| OTA `firmware.force` | ⬜ | 当前按版本比较 |
| OTA 校时 `server_time` | ⬜ | 待机时钟的前置 |
| OTA 后确认 / 自动回滚确认 | 🟡 | 启动确认已置 VALID，未实现 PENDING_VERIFY 回滚窗口 |
| 资源（assets）OTA | ⬜ | 依赖屏幕 UI 资源分区 |
| MQTT TLS（8883） | ⬜ | 当前仅明文 1883，受信内网使用 |

## MCP 设备控制

| 能力 | 状态 | 说明 |
|---|---|---|
| `initialize` / `tools/list` / `tools/call` | ✅ | JSON-RPC 2.0，`2024-11-05` |
| `self.get_device_status` | 🟡 | 音量/电量/充电；缺 SSID/RSSI/亮度/芯片温度 |
| `self.audio_speaker.set_volume` | ✅ | 0–100，NVS 持久化 |
| `self.get_system_info` / `self.reboot` | ✅ | |
| `self.screen.set_brightness` | ⬜ | 需先实现背光 PWM |
| `self.upgrade_firmware(url)` | ⬜ | 有安全风险，接官方服务器时再评估 |
| 灯效 / 重新配网 / press-to-talk 等板级工具 | ⬜ | gezipai 参考板未注册这些工具，低优先级 |

## 音频

| 能力 | 状态 | 说明 |
|---|---|---|
| Opus 16k/60ms 全双工、下行重采样 | ✅ | 与 C++ 相同参数 |
| 丢包隐藏（PLC） | ✅ | 本固件增强 |
| 能量 VAD 唤醒 / 本地断句 | 🟡 | C++ 用 WakeNet + 服务端 VAD；本机 VAD 为替代方案 |
| AFE（AEC/NS/AGC） | ⛔ | esp-sr 推理撞 MMU block 1 |
| WakeNet 关键词唤醒 | ⛔ | 同上；任意人声即唤醒是当前限制 |
| 提示音（空闲/唤醒/低电/错误/alert/成功） | ✅ | 2026-10-25 增加 4 段（`tools/convert_prompts.sh` 转码） |
| 配置模式音效（wificonfig）/ 升级音（upgrade） | ⬜ | 配网阶段音频未初始化 |
| 音频自测（配网下按键录放） | ⬜ | 调试 bin 有回环，未接产品流程 |
| 空闲 15s 关 codec | 🟡 | 本固件保持采集以支持 VAD 唤醒，仅关功放 |

## 显示与交互

> 本机（gezipai 实机）**没有屏幕**，显示类功能整批跳过；状态机/通知行走的是
> 无屏兜底路径，不影响语音链路。若后续换带屏样机，再按本表补齐。

| 能力 | 状态 | 说明 |
|---|---|---|
| 状态文字 | ✅ | 3×5 ASCII 点阵 |
| 通知行（音量/低电/错误/情绪/alert） | ✅ | 2026-10-25 增加，3 秒/5 秒超时 |
| 中文点阵字库 | ⬜ | 屏幕 UI 批次 |
| 表情（21 种） | ⬜ | 屏幕 UI 批次 |
| 状态栏（WiFi/电池/静音） | ⬜ | 屏幕 UI 批次 |
| 聊天气泡（stt/tts 文本） | ⬜ | 文本已缓存，待 UI |
| 待机时钟 | ⬜ | 依赖校时 |
| 亮度 PWM / 息屏 | ⬜ | 当前背光常亮 |
| 多语言（22 种，编译期） | ⬜ | 当前提示音固定中文 |
| 音量单击 ±10 / 长按最大/静音 | ✅ | 2026-10-25 对齐 |
| BOOT 按住对讲 / 开机长按 3s 清凭据 | ✅ | 本固件用长按 3s 防下载模式误清 |
| 低电提醒（<20% 放电，一次） | ✅ | 提示音 + 通知；不自动关机 |
| 网络错误提示音 | ✅ | 会话异常事件 |
| 自动关机 / 深睡 | ⛔ | S3 的 BOOT/VOL 键非 RTC-capable，无法按键唤醒 |
| 空闲低功耗（功放关/LED 灭/WiFi Minimum） | ✅ | 本固件自研；C++ gezipai 板无 PowerSaveTimer |

## 其他

| 能力 | 状态 | 说明 |
|---|---|---|
| 设备 UUID（Client-Id） | 🟡 | 当前用 MAC；C++ 首次随机生成并持久化 |
| 序列号 / 完整系统信息 JSON | ⬜ | 激活流程需要 |
| 跨芯片 C3/C6/P4 | ⬜ | 独立任务，需要硬件 |
| 多板（BSP 抽象） | ✅ | 已有 BoardConfig/组件分层 |
| 声纹识别 | ⛔ | C++ 设备端也没有，依赖服务器 |

## 批次一（2026-10-25，已完成）

- `xiaozhi-core/src/server_msg.rs`：下行消息分类 + 顶层字段扫描（转义引号安全），6 个 host 单测；
- `src/session.rs`：打断报文改为 `{"type":"abort"}`；
- `src/app.rs` / `src/mqtt_session.rs`：接入统一解析，新增 `UiMessage` 通道、
  `SessionEvent::Error`、`system.reboot`、stt/llm/tts sentence/alert 处理；
- `src/prompt.rs`：多片段播放器（空闲/唤醒/低电/错误/alert/成功）；
- `assets/*.raw`：低电、错误、提醒、成功音（C++ 参考工程 OGG 转码）；
- `src/bsp/components/display.rs`：通知行绘制；
- 音量长按、低电提醒、错误提示、显示通知。

### 真机记录（2026-10-25）

- 启动/WiFi/OTA/MQTT/MCP tools、60s 空闲提示、提示后功放关闭均通过；
- 修正：低功耗待机时功放已断电，低电/alert/error 提示音前需先恢复 PA，
  播完仍在待机则重新关闭；
- 修正：VOL 100% 破音 —— ES8311 REG32 旧线性映射 100% = +32dB，满幅 PCM
  会硬削顶。新映射 0% = 静音，1~100% = -50dB~-3dB（REG 91~185），
  60% ≈ -22dB，与旧映射的常用音量接近；`AGENTS.md` 已同步新映射；
- 修正：短回复播放卡顿 —— 根因是 UDP 下行突发丢包（日志短句 35 包丢 5 包，
  本地无欠载）。UDP 收缓冲 32KB→64KB（元数据 64→128）、每轮排空 32→64、
  下行通道 8→16、PCM 环 512KB；复测「卡顿消失，播放正常」；
- 修正：服务端 AEC 时间戳大量为 0 —— `PlayoutTimestamps`/`AecClock` 只有
  64 条（≈3.8s），而 PCM 环已扩到 8s，溢出后时间戳被并进最旧条目；两条
  队列扩到 192 条（≈11.5s），并在 TTS 结束后静音上行 400ms，避免尾音被
  服务端 ASR 识别成用户说话（真机日志可见「我听到啦」自问自答）；
- 语音唤醒、BOOT 打断、音量长按已人工验证通过；alert 声音尚未实测。

### 硬件发现：功放使能导致 WiFi 下行持续丢包（2026-10-25）

长句播放中后段卡顿/杂音，做了三组对照（同一长 TTS、网关 1.0x pacing、无重复包）：

| 条件 | 下行丢包 |
|---|---|
| 功放关闭（`pa-off`） | **0**（502 包全收） |
| 功放使能、codec 音量 0（`volume-off`） | ≈19%（378/502） |
| 功放使能、正常音量 | ≈18%（389/502） |

结论：不是播放缓冲、不是上行干扰、也不是扬声器声音电流，而是**功放使能本身**
（PA 电路/供电/EMI）让 WiFi 下行持续丢包。软件无法根治：

- 试过网关 17ms / 20+40ms 重复发包，无改善甚至更差（持续丢包，非突发）；
- 建议硬件侧：接充满的电池或换更强 5V 供电、PA 电源就近加大电容/独立 LDO、
  检查 PA 输入是否自激、天线/屏蔽与走线（PCB 上避开扬声器线），或换 WiFi 信道
  避开干扰谐波；
- 诊断构建保留 `pa-off` / `volume-off` / `aec-off` 三个 feature，便于复测。

## 下一批建议

1. **语音链路**：先真机验证 realtime 模式的服务端 AEC 打断质量，再评估 WakeNet
   MMU block 1 修复（高风险）。
2. **服务端兼容**：激活/挑战、资源 OTA、MQTT TLS、校时（接官方服务器时优先）。
3. **网关 pacing**：已在本机网关实现（1.4x 实时、tts stop 排在音频后），
   待真机复测固化；带屏样机的 UI 批次暂缓（本机无屏）。
