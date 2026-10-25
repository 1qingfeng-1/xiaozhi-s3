# Pull Request

## 改动说明

<!-- 这个 PR 解决什么问题？为什么这样改？ -->

## 影响范围

- [ ] `xiaozhi-core`（纯逻辑层，需先在子仓库提交并推送）
- [ ] 固件硬件层（I2S / I2C / DMA / Flash / 网络）
- [ ] BSP / 新板
- [ ] CI / 文档 / 构建

## 资源影响

<!-- 栈、堆、DMA、Flash、通道容量；无影响请写“无” -->

## 验证方式

- [ ] `./scripts/check.sh --with-docs`
- [ ] `cd xiaozhi-core && cargo +stable nextest run`
- [ ] 真机验证（板型 / 步骤 / 关键日志）

日志片段：

```text

```

## 关联 issue

<!-- Closes #... -->
