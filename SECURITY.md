# 安全策略

## 支持范围

仅最新 release tag 与 `main` 分支接受安全修复。

## 报告漏洞

请使用 GitHub 的 **Private vulnerability reporting**（仓库 Security 标签页），
不要公开提交 issue。请在报告中包含：

- 影响范围（OTA / 配网 / 会话协议 / MQTT-UDP / 本地显示）
- 复现步骤或 PoC（无需包含真实 WiFi 凭据）
- 设备型号、固件版本（`git describe --tags` 或 release tag）

我们会在 7 天内确认收到，并在修复发布后致谢（除非你要求匿名）。

## 范围说明

- OTA 镜像目前仅做 CRC 校验，尚未启用签名验签；在启用前请仅在可信网络使用。
- SoftAP 配网页面无鉴权，属于设计约束：请勿在不可信环境长期开启配网模式。
- 请勿在 issue/PR 中粘贴 WiFi 密码、MQTT 密钥或服务器 token。
