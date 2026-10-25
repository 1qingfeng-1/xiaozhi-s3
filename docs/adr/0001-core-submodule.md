# ADR-0001: core 使用 git submodule 而非 workspace

- 状态: 已接受
- 日期: 2026-10-25
- 决策者: 维护者

## 背景

`xiaozhi-core` 是纯逻辑层（无 `esp-hal`、`#![deny(unsafe_code)]`），需要在
host 上用 stable 工具链跑测试。父仓库的 `.cargo/config.toml` 把
`build.target` 锁到 `xtensa-esp32s3-none-elf` 并启用 `build-std` /
`-nostartfiles`，如果 core 作为 workspace 成员，host 测试会被父配置污染
（`E0463: can't find crate for core`，或链接失败）。

同时希望 core 保持可独立发布/复用的可能。

## 决策

将 core 作为独立 git 仓库，通过 submodule 挂载；core 自带空 `[workspace]`
根，避免向上查找父 workspace。

## 后果

### 正面

- host 测试不受父 target 配置影响，`cd xiaozhi-core && cargo +stable test` 即可。
- core 可独立打 tag、发布 crates.io、被其他项目复用。
- 分层边界清晰：core 的依赖图天然不含硬件 crate。

### 负面 / 代价

- 贡献者需要两阶段提交：先 core 仓库 PR，再更新父仓库指针。
- CI 必须 `submodules: recursive`，缓存 key 只能基于 `Cargo.toml`。
- 代码搜索/重构跨仓库时工具支持较弱。

## 备选方案

1. **单一 workspace**：需要移除父 `.cargo/config.toml` 的全局 `build.target`，
   改用 `--target`/alias；会改变所有人的构建命令，收益不足以抵消迁移成本。
2. **git subtree**：单仓库协作更简单，但 core 无法独立发布，历史拆分也更麻烦。
