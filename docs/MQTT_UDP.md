<!-- markdownlint-disable-file MD013 -->
# MQTT + UDP 传输（xiaozhi 网关）

设备除 WebSocket 外，还支持 `xiaozhi-mqtt-gateway` 的 MQTT+UDP 传输：
MQTT 承载控制消息，音频走 UDP + AES-128-CTR，包头携带服务器时间戳，
可在服务端做回声消除（AEC）。传输方式完全由 OTA 清单决定，固件无需重编。

## 1. 服务器侧准备

1. 部署 [xiaozhi-mqtt-gateway](https://github.com/xinnan-tech/xiaozhi-mqtt-gateway)（Node.js）：
   - `config/mqtt.json` 的 `chat_servers` 填 `ws://<xiaozhi-server>:8000/xiaozhi/v1/?from=mqtt_gateway`
   - `.env`：`PUBLIC_IP` / `MQTT_PORT` / `UDP_PORT` / `API_PORT` /
     `MQTT_SIGNATURE_KEY`（≥8 位，含大写字母，且不能包含 `test`）
2. `xiaozhi-server` 的 `data/.config.yaml`：

   ```yaml
   server:
     mqtt_gateway: <网关 IP>:1883
     mqtt_signature_key: <与网关 .env 相同的密钥>
   ```

3. 重启服务器。之后 `POST /xiaozhi/ota/` 的响应会多出 `mqtt` 对象：

   ```json
   {
     "mqtt": {
       "endpoint": "192.168.123.14:1883",
       "client_id": "GID_gezipai@@@44_1B_F6_D9_58_B0@@@44_1B_F6_D9_58_B0",
       "username": "…base64…",
       "password": "…HMAC-SHA256…",
       "publish_topic": "device-server",
       "subscribe_topic": "devices/p2p/44_1B_F6_D9_58_B0"
     }
   }
   ```

## 2. 设备侧流程

```text
开机 → OTA 检查 → 解析 mqtt 对象 → 存 NVS 0xA000 → 选择 MQTT 任务
  ├─ MQTT(TCP) 连接/订阅 → 收 MCP initialize（应答 tools/list）
  ├─ 唤醒 → publish hello(version=3, transport=udp, aec=true)
  ├─ 收到 hello 回复 → 取 udp.server/port/key/nonce → UDP 就绪 → Ready
  ├─ 上行：opus → 16B 头 + AES-128-CTR → UDP
  ├─ 下行：UDP → 解密 → opus 解码 → PCM 环 → I2S
  └─ tts start/stop、listen、goodbye 走 MQTT publish
```

关键点：

- **16 字节包头**（大端）：`type|flags|payload_len|ssrc|timestamp|sequence`；
  服务端下发的 `nonce` 即模板，设备只改写长度/时间戳/序列号，整条头作为
  AES-CTR 初始计数块。
- **时间戳**：下行帧解码入 PCM 队列时登记，按**实际写入 DMA 的字节**推进，
  一帧播完才交给上行帧（`PlayoutTimestamps` + `AecClock`），服务端据此对齐
  回声参考帧。
- **AEC**：协议版本 ≥2 / MQTT 模式下，`server_aec=true`，播报期间持续上行
  麦克风；服务端 `client_aec=true` 时对其做回声消除，并可用 VAD 打断播报。
- **过期 goodbye**：网关关闭旧 bridge 时会补发旧 `session_id` 的 goodbye，
  设备按 id 过滤，避免新会话刚建立就被判关闭。
- **本地收尾**：等回复超时（20s）主动 publish goodbye，释放网关 bridge。

## 3. NVS 布局（NVS 分区 0x9000–0xF000）

| 偏移 | 用途 | 说明 |
| --- | --- | --- |
| 0x9000 | 存储自检 | 启动时写读一次 |
| 0xA000 | MQTT 网关配置 | magic `MQCF` + 6 个长度前缀字符串 |
| 0xB000 | WS 协议版本 | 首字节 1/2/3（默认 1） |
| 0xC000 | OTA 失败计数 | 同版本失败 3 次退避 |
| 0xD000 | 音量 | 首字节 0–100 |
| 0xE000 | WiFi 凭据 | ssid/password/server |

OTA 任务仅在配置内容变化时擦写 0xA000，避免每次开机磨损。

## 4. 真机验证（2026-10-24）

| 项 | 结果 |
| --- | --- |
| OTA 下发 MQTT 配置并落 NVS | ✅ 重启后仍走 MQTT |
| MQTT 连接/订阅/心跳 | ✅ `MQTT 已连接` + MCP 工具初始化成功 |
| hello → UDP 就绪 | ✅ `UDP 通道就绪: <ip>:8884` |
| 上行音频 | ✅ 网关日志 16.6 帧/s，序列号连续 |
| 下行 TTS | ✅ 解密/解码/播放，`TX 推送速率 ≈64KB/s`，无欠载 |
| tts stop 回聆听 | ✅ 状态机 `播报 → 聆听` |
| 内部堆水位 | ✅ 大缓冲迁 PSRAM 后 <90%（此前 98%） |
| 过期 goodbye 过滤 | ✅ 新会话不再被旧 bridge 关闭误伤 |

## 5. 已知限制

- 本机 `xiaozhi-server` 的直连 WebSocket 路径**不解析** 16B 头（只有
  `?from=mqtt_gateway` 才解析并做 AEC），因此 WS 模式默认仍为 v1；
  需要服务端 AEC 时请使用 MQTT+UDP。
- MQTT 目前为明文 TCP（1883），仅建议受信内网；公网部署需在网关侧做 TLS
  或网络隔离（设备侧 TLS 依赖后续引入 mbedTLS/esp-mbedtls）。
- 无真人说话时无法自动进入会话：真机验证用 `session-selftest` 特性
  （开机 5s 自动唤醒一次）触发，默认关闭。

### 下行加固（2026-10-25）

- UDP 收包校验来源地址/端口必须等于 hello 下发的服务器 endpoint；
  `ssrc` 目前只记录不比对（兼容网关实现）。
- 序号采用回绕比较：迟到/重复包直接丢弃，只对 `seq > max_seq` 的缺口
  补 PLC（最多 4 帧），避免乱序包污染后续判断。
- MQTT QoS1 PUBLISH 立即回 PUBACK，防止 broker 重发导致 MCP 命令重复执行；
  topic 必须等于订阅主题。
- 单报文硬上限 64KB；超长报文限时丢弃，不再可能被慢速对端拖住数分钟。
- UDP 收包缓冲当前 64KB / 128 元数据（≈50 包），每轮最多排空 64 包；
  软件 PCM 环 512KB（≈8s），downlink 通道 16，用于吸收服务端 TTS 突发。
  真机验证：长句卡顿消失（改前断序 5~40%）。
- 服务端 AEC 时间戳队列 `PlayoutTimestamps`/`AecClock` 各 192 条（≈11.5s），
  必须覆盖整个 PCM 环；容量不足时新帧时间戳会被并入最旧条目，上行时间戳
  大量为 0，服务端 AEC 等于失效。

### 网关下行 pacing（本机补丁，2026-10-25）

服务端 TTS 合成快于实时，网关原本收到 WS 二进制就立即 UDP 转发，短句可能在
几百毫秒内突发几十包，设备侧缓冲再大也有上限。本机 `xiaozhi-mqtt-gateway/app.js`
已加 per-connection 发送队列：

- 按 **frame_duration 原速 1.0x** 逐包发送（60ms 帧 → 60ms/包 ≈16.7 包/s）；
  任何 >1x 的节奏都会持续跑在设备播放前面，长 TTS 会填满 8s PCM 环后丢包
  （实测 1.4x/1.2x 长句中后段卡顿）；
- 网关队列就是突发缓冲，上限 2048 包（≈2 分钟），超出丢最旧并告警；
- `tts stop` / `goodbye` 延迟到音频队列清空后再发，避免设备提前收尾丢掉尾部音频；
- 新 `tts start` 清空上一轮残余音频，避免连续长句开头混入旧包；
- 新 `hello` / 连接关闭时清空队列，防止旧会话音频串入新会话。

网关不在本仓库，部署到其他机器时需同步此补丁。复测指标：网关
`[DIAG] UDP 下行速率` 稳定在 ≈17/s，设备 `tts stop` 的断序应为 0。

> 注意：设备功放使能时实测下行丢包 ≈19%，与 WiFi 共存问题有关
> （功放关闭时 0，音量 0 时仍丢），详见 `docs/PARITY.md` 的「硬件发现」。
> 试过 UDP 重复发包（17ms / 20+40ms）无改善，已回退。
