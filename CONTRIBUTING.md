# 贡献指南

感谢参与！本项目是 ESP32-S3 上的 Rust `no_std` 固件，改动会直接影响真机行为，
请先阅读本指南与 [`AGENTS.md`](AGENTS.md)。

## 开发环境

```bash
# 1. 安装 ESP 工具链 (espup 创建 rust-toolchain.toml 指定的 esp channel)
cargo install espup --locked
espup install
source ~/.espup/export-esp.sh

# 2. 安装常用工具
cargo install cargo-nextest cargo-deny typos-cli espflash
pip install pre-commit && pre-commit install

# 3. 拉取子模块
git submodule update --init --recursive

# 4. 门禁 (与 CI 完全一致)
./scripts/check.sh --with-docs
```

host 侧命令（core 测试）必须显式使用 `+stable`：

```bash
cd xiaozhi-core && cargo +stable nextest run
```

## 架构边界

- `xiaozhi-core/` 只放纯逻辑：**禁止引入 `esp-hal` 或任何硬件依赖**，
  保持 `#![deny(unsafe_code)]` 与 host 可测试性。
- 固件侧只做 I2S/I2C、DMA、Flash、网络等硬件操作；对话/协议决策在 core。
- BSP 引脚只在 `src/bsp/gezipai.rs` 定义，应用代码禁止硬编码 GPIO/I2C 地址。
- 新增板子按 [`docs/PORTING.md`](docs/PORTING.md) 四步法。

## 子模块工作流（重要）

`xiaozhi-core` 是独立 git 仓库（submodule）。修改 core 时：

1. 在 `xiaozhi-core/` 内提交并推送到其远端；
2. 回到父仓库，提交更新后的子模块指针；
3. 父仓库 PR 中说明对应的 core commit。

只改父仓库、不更新指针的 PR 不会被合并。

## 提交与 PR

- Conventional Commits，commit message 用中文（示例：`fix(audio): 修复 VAD 阈值`）。
- 一个 PR 只做一件事；附带真机验证记录（日志片段、复现步骤、板型）。
- 提交前必须通过 `./scripts/check.sh --with-docs`。
- 涉及行为/时序的改动，请在 PR 描述中写明资源影响：栈、堆、DMA、通道容量。
- 诊断/自测 feature（`*-off`、`*-selftest`、`ps-adaptive`）不是可叠加能力，
  不要在生产构建中启用，`--all-features` 只用于编译检查。

## 文档

- 新功能更新 `AGENTS.md` 对应章节与 `docs/ROADMAP.md`。
- 架构取舍记录在 [`docs/adr/`](docs/adr/)。
- Markdown 需通过 markdownlint；历史过程文档统一放 `docs/archive/`。

## 安全

发现安全问题时不要开公开 issue，见 [`SECURITY.md`](SECURITY.md)。
