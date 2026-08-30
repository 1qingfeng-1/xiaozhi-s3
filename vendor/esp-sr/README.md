<!-- markdownlint-disable-file MD013 -->
# vendor/esp-sr — WakeNet 关键词唤醒 (esp-sr v1.9.5)

esp-hal (no_std) 下的 esp-sr 集成, feature `wakenet` 门控。
**链接层已验证闭环** (`cargo build --release --features wakenet` 通过);
运行时行为 (模型解压/MFCC/检测) 待真机阶段验证后才能默认启用。

## 资产

| 路径 | 内容 | 来源 |
|---|---|---|
| `lib/` | libwakenet / libdl_lib / libhufzip / libc_speech_features (esp32s3 预编译) | espressif/esp-sr v1.9.5 |
| `include/` | 公开头文件 (esp_wn_iface / model_path / dl_lib*) | 同上 |
| `model/srmodels.bin` | 「你好小智」(wn9_nihaoxiaozhi) 模型包, hufzip 压缩 ~284KB | `pack_model.py` 打包 |
| `src/model_path_shim.c` | esp-hal 环境垫片 (本项目编写) | — |
| `../esp-dsp/` | esp-dsp 1.5.2 源码子集 (FFT/点积, ANSI 路径) + compat 桥接 | espressif/esp-dsp |
| `../cjson/` | cJSON 1.7.18 | DaveGamble/cJSON |

模型以 `include_bytes!` 嵌入固件 flash rodata (`src/audio/wake_net.rs`),
经官方 `srmodel_load` 语义原地解析 —— **无需模型分区与 SPIFFS**。

## 架构

```text
Rust (src/audio/wake_net.rs)          C (src/model_path_shim.c)
  WakeNetSource (WakeWordSource trait)   esp_srmodel_init: 解析内嵌 srmodels.bin
    └─ esp_wn_handle_from_name             srmodel_get_model_coeff / get_model_base_path
    └─ iface.create / detect / destroy     fopen/fread 兜底 (hufzip 文件路径)
libwakenet.a (闭源) → libdl_lib.a → libhufzip.a → libc_speech_features.a
                                    → esp-dsp 子集 (compat 桥接 ANSI) → cJSON
```

## 垫片清单 (model_path_shim.c)

- RAM 加载: `esp_srmodel_init/filter/deinit/exists/get_wake_words`、
  `srmodel_get_model_coeff`、`get_static_srmodels`、`srmodel_load`
- 文件 IO: `fopen/fread/fseek/ftell/fclose/feof/fwrite/fflush` (内存文件)
- 日志/断言: `esp_log_*` 静默; `__assert_func` 由固件提供 (勿重复定义)
- reent/malloc: `__getreent/_malloc_r/_calloc_r/_realloc_r/_free_r/_sbrk_r`
  → 全局堆 (esp-alloc compat); `memalign`
- FreeRTOS: 队列/任务**空转桩** (单线程语义, 见已知风险)
- Cache/ROM: `Cache_Start_DCache_Preload` 等空实现; `esp_aes_*` 透传
- 兜底解析: `strtof/strtod/strtok/strdup/_ctype_` (避开 newlib ssp/stdio)

## 真机验证结论 (2026-08-30): 预编译库与 esp-hal 内存映射不兼容

自检插桩定位: create 成功 (chunk=512/rate=16000/模型状态在内部堆),
**首次 detect 即 InstrProhibited (PC=0x1FFE2000)**。

反汇编确认: `model_detect` (0x420dd604) 内 `callx8` 经
`model->[160] -> [+8]` 的函数指针跳到 0x1FFE2000 — 该地址落在
**ESP32-S3 MMU cache block 1** (数据 0x1FC00000 / 取指 0x5FC00000,
栈上返回地址 0x5FFE1FFD 与崩溃 PC 0x1FFE2000 恰为同一物理页的两个
窗口别名, 差 0x40000000)。

结论: esp-sr v1.9.5 的 wn9 模型加载/推理路径假设 IDF 已将 PSRAM 映射
进 **cache block 1** (IDF S3 的 PSRAM 专用窗口), 而 esp-hal 只映射了
block 0 (数据 0x3C090000+, 无 block 1)。这不是垫片可修的符号问题。

可行出路 (均为独立工作量):
1. 在 esp-hal PSRAM 初始化后自行写 S3 MMU block 1 页表, 把 PSRAM
   物理页映射到 0x1FC00000/0x5FC00000 窗口 — 需确认库内期望的布局;
2. 改用 esp-sr 2.x + esp-dl (源码开放, 无预编译假设, 但集成量大);
3. 持续使用 VAD 声音唤醒 (默认路径, 已验证)。

## 已知风险 (真机阶段逐项验证)

1. **mfcc 任务**: `xTaskCreatePinnedToCore` 为空转桩 —— 若 wakenet create
   依赖其任务产出, 检测将无结果。验证方法: create 后喂「你好小智」录音,
   无检出则需实现真实任务桥接 (esp-rtos 双核或查询式虚拟任务)。
2. **vvadd_shift_1_int16**: 语义按 esp-dl 惯例推定, 未对照源码。
3. **模型阈值**: `_MODEL_INFO_` 显示 det threshold 0.585/0.595, 如误唤醒
   频繁可通过 `set_det_threshold` 上调。
4. **CPU/内存**: wakenet 推理 ~80KB 内部堆 + MFCC 每 32ms 一帧; 与
   WiFi/opus 共存时的实时性需实测。

## 启用方法

```bash
cargo build --release --features wakenet   # 链接级已验证
```

启用后需将 `src/app.rs` 的唤醒源从 `VadWakeDetector` 换为
`WakeNetSource` (同一 `WakeWordSource` trait, 约 5 行改动)。
