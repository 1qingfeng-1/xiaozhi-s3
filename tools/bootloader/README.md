# 自定义二级引导器（启用 OTA 自动回滚）

`esp-bootloader-esp-idf` 只是应用侧支持库，是否回滚由二级引导器决定。
本工程用本机 ESP-IDF 构建一个启用
`CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE` 的 bootloader，产物放到
`assets/bootloader-esp32s3-rollback.bin`，由 `espflash.toml` 注入。

## 构建

```bash
# 需要 ESP-IDF >= 5.4 (本机 /home/mrpeng/esp/v5.5.2/esp-idf)
./scripts/build_bootloader.sh
```

脚本会：

1. `source $IDF_PATH/export.sh`
2. `idf.py -C tools/bootloader -B <tmp> set-target esp32s3 bootloader`
3. 复制 `bootloader.bin` → `assets/bootloader-esp32s3-rollback.bin`
4. 生成 `assets/bootloader-esp32s3-rollback.bin.sha256`

## 与应用状态的配合

- 应用写新镜像 otadata 为 `NEW(0)`（`src/ota_check.rs`）；
- 引导器首次启动把 `NEW` 改为 `PENDING_VERIFY`；
- 应用联网稳定后写 `VALID`；
- 若应用在确认前崩溃/重启，引导器把 `PENDING_VERIFY` 标为 `ABORTED`
  并回退到旧槽。

⚠️ 不要把新镜像直接写 `PENDING_VERIFY`：引导器启动时会先把所有
`PENDING_VERIFY` 标为 `ABORTED`，导致 OTA 一升级就回退。
