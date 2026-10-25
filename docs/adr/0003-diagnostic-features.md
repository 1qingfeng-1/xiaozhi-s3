# ADR-0003: 诊断开关保留为 Cargo feature 的约束

- 状态: 已接受
- 日期: 2026-10-25
- 决策者: 维护者

## 背景

`aec-off`、`pa-off`、`volume-off`、`ps-adaptive` 与若干 `*-selftest`
是行为开关，不是可叠加能力。Cargo feature 的语义是“增加能力”，
`--all-features` 会把这些互斥开关同时打开，产生无意义的固件组合。

## 决策

短期继续使用 Cargo feature（改动面小、文档与 CI 已围绕它建立），但：

1. 所有诊断 feature 在 `Cargo.toml` 中注明“禁止用于发布 / 互斥”；
2. 行为开关（`aec-off` / `pa-off` / `volume-off` / `ps-adaptive`）改用
   `cfg!()` 求值而非 `#[cfg]` 遮蔽，保证任何组合都能无警告编译；
   `*-selftest` 仍在函数级 `#[cfg]` 块中，并用 `cfg_attr` 接受不可达代码告警；
3. CI 的 feature 矩阵逐个显式组合编译，不用 `--all-features` 代表运行配置；
4. `--all-features` 仅用于静态检查。

长期若诊断项继续增加，迁移到 `esp-config` 风格的构建期配置或运行时配置。

## 后果

### 正面

- 保留现有使用习惯与脚本，`--all-features` 可保持零警告。

### 负面 / 代价

- 仍然违反 Cargo feature 可加性；下游若复用此 crate 需自行避免组合。
- 新增诊断开关时必须手动加入 CI feature 矩阵。

## 备选方案

立即改为 `build.rs` + `--cfg` 环境变量：语义更正确，但会改变所有诊断构建命令，
并需要同步修改文档与历史脚本。
