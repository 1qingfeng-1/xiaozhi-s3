<!-- markdownlint-disable-file MD013 -->
# vendor/esp-sr — WakeNet 关键词唤醒 (esp-sr v1.9.5)

esp-hal (no_std) 下的 esp-sr 集成, feature `wakenet` 门控。
**链接与调用链均已闭环** (`cargo build --release --features wakenet`)；
检测准确率/误唤醒仍需真机录音标定。

## 状态 (2026-10-31 修正)

此前记录的「预编译库与 esp-hal MMU block 1 不兼容」是**误判**。

真机 `InstrProhibited (PC=0x1FFE2000)` 的根因是手写 FFI 结构体错位：

- `include/esp_wn_iface.h` 的 `esp_wn_iface_t` 只有 **14 项 (56 字节)**，
  `detect` 位于 `get_vol_gain` 与 `clean` 之间（偏移 **0x2c**）。
- 旧 Rust `WnIface` 多写了一个头文件里不存在的 `reset_det_threshold`（15 项），
  把 `detect` 排到偏移 **0x38**。
- 最终 ELF 中 `esp_sr_wakenet9_quantized` 表尾紧邻的 rodata 常量恰好是
  `00 20 fe 1f` → **0x1FFE2000**。所谓「跳到 MMU block 1」就是读了这个常量。
- 证据：`target/.../xiaozhi-s3` 里 `esp_sr_wakenet9_quantized+0x2c` =
  `model_detect`，`+0x38` = `0x1FFE2000`。

修正：Rust 结构体与头文件逐位一致，并用
`offset_of!(detect) == 44` / `offset_of!(destroy) == 52` / `size_of == 56`
三条编译期断言锁死。**esp-hal 的 PSRAM/MMU 映射不需要任何改动。**

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
经 `srmodel_load` 语义原地解析 —— **无需模型分区与 SPIFFS**。

## 架构

```text
Rust (src/audio/wake_net.rs)          C (src/model_path_shim.c)
  WakeNetSource (WakeWordSource trait)   esp_srmodel_init: 解析内嵌 srmodels.bin
    └─ esp_wn_handle_from_name             srmodel_get_model_coeff / get_model_base_path
    └─ iface.create / detect / clean       fopen/fread 兜底 (hufzip 文件路径)
libwakenet.a (闭源) → libdl_lib.a → libhufzip.a → libc_speech_features.a
                                    → esp-dsp 子集 (compat 桥接 ANSI) → cJSON
```

## 垫片清单 (model_path_shim.c)

- RAM 加载: `esp_srmodel_init/filter/deinit/exists/get_wake_words`、
  `srmodel_get_model_coeff`、`get_static_srmodels`、`srmodel_load`
- `srmodel_list_t` 布局与 `model_path.h` 的 **ESP_PLATFORM 分支**一致
  (`model_name, model_info, partition, mmap_handle, num, model_data`)；
  `_MODEL_INFO_` 会解析成 `model_info`，唤醒词按官方 `i % 4 == 3` 规则提取
- 文件 IO: `fopen/fread/fseek/ftell/fclose/feof/fwrite/fflush` (内存文件)
- 堆: `heap_caps_malloc/calloc` 的 `MALLOC_CAP_SPIRAM` 请求经 Rust
  `shim_heap_caps_alloc`/`shim_psram_malloc` 落 PSRAM (4 字节头与 esp-alloc 兼容，C 侧 `free` 可回收)；
  其余 caps 走内部堆
- 日志/断言: `esp_log_*` 静默; `__assert_func` 由固件提供 (勿重复定义)
- reent/malloc: `__getreent/_malloc_r/_calloc_r/_realloc_r/_free_r/_sbrk_r`
  → 全局堆 (esp-alloc compat); `memalign`
- FreeRTOS: 队列/任务**空转桩** (单线程语义)。wn9 的 MFCC 走
  `esp_fbank_f32` 直通函数表，不依赖 `mfcc_runner_task`，因此该桩不影响 wn9
- Cache/ROM: `Cache_Start_DCache_Preload` 等空实现; `esp_aes_*` 透传
- 兜底解析: `strtof/strtod/strtok/strdup/_ctype_` (避开 newlib ssp/stdio)

## 已知风险 (真机阶段逐项验证)

1. **检测准确率/误唤醒**: 模型阈值 `_MODEL_INFO_` 为 0.585/0.595；
   安静/远场/回声环境需用真机录音标定，必要时 `set_det_threshold` 上调。
2. **CPU/内存**: wakenet 推理权重落 PSRAM，推理状态在内部堆；每 32ms 一帧
   512 样本。与 WiFi/opus 共存时的实时性需实测。
3. **DET_MODE**: 当前用 `DET_MODE_95`（单通道，最激进）。若改用 2CH/3CH，
   必须提供对应通道数（`chunk × channels`）的输入，否则模型读到越界样本。
4. **FreeRTOS 任务桩**: wn9 不需要；若以后换 wn5/wn7/wn8 或引入 AFE，
   需实现真实任务桥接（见 `docs/ROADMAP.md`）。

## 启用方法

```bash
cargo build --release --features wakenet
```

`src/audio/wake_source.rs` 在启动时优先创建 WakeNet（失败自动回退能量 VAD）；
命中即视为本地关键词确认，跳过服务端 ASR 门控。
`--features wakenet,debug-diag` 会在开机时跑一次 create/detect/destroy 自检，
并打印解析出的唤醒词。
