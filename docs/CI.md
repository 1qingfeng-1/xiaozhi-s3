# 门禁、CI 与发布

本文是本地门禁、pre-commit、GitHub Actions 与 release 的唯一说明。
历史配置总结见 [`archive/`](archive/)。

## 1. 本地门禁（与 CI 同序）

```bash
./scripts/check.sh              # 固件 + core
./scripts/check.sh --with-docs  # 额外跑 cargo-deny / typos / markdownlint
```

等价的手工命令：

```bash
# 固件（esp 工具链由 rust-toolchain.toml 锁定）
cargo fmt --all -- --check
cargo clippy --locked --all-features -- -D warnings
cargo check --locked --all

# core（host，必须 +stable）
cd xiaozhi-core
cargo +stable fmt --all -- --check
cargo +stable clippy --all-targets -- -D warnings
cargo +stable nextest run
```

> `--locked` 只在 CI 使用，保证 Cargo.lock 与依赖一致；本地升级依赖时可省略。

### 资源门禁

```bash
cargo build --release --locked
./scripts/check_stack.sh target/xtensa-esp32s3-none-elf/release/xiaozhi-s3
```

主任务栈下限 **96KiB**（CI 硬性检查）。镜像大小上限为 OTA 槽
`0x3F0000` 的 90%（CI 用 `espflash save-image` 生成镜像后检查）。

## 2. pre-commit

```bash
pip install pre-commit
pre-commit install            # 安装 commit 钩子
pre-commit run --all-files    # 全量运行
pre-commit run cargo-fmt      # 只跑某个钩子
git commit --no-verify        # 紧急跳过（不推荐）
pre-commit autoupdate         # 升级钩子版本
```

钩子列表（`.pre-commit-config.yaml`）：

| 钩子 | 说明 |
|---|---|
| `cargo-fmt` / `cargo-clippy` / `cargo-check` | Rust 门禁，与 CI 一致 |
| `cargo-deny` | advisory / license 审计 |
| `typos` | 拼写检查 |
| `markdownlint-cli2` | Markdown 检查（`docs/archive/` 与 `vendor/` 排除） |
| `check-*` / `end-of-file-fixer` / `trailing-whitespace` | 通用文件卫生 |

### 常见问题

- **hook 找不到 cargo**：确保 `~/.cargo/bin` 在 `PATH`，重新 `pre-commit install`。
- **第一次运行很慢**：pre-commit 需要创建 node/python 环境，之后有缓存。
- **markdownlint 与本地版本不一致**：以 `.pre-commit-config.yaml` 里的
  `rev` 为准（CI 使用同版本 `npx markdownlint-cli2@0.17.2`）。
- **cargo-deny 需要联网**：无法访问 GitHub 时 advisory 数据库拉取会失败，
  可先 `cargo deny fetch` 预热缓存。

## 3. GitHub Actions

工作流：`.github/workflows/ci.yml`，在 `push main` 与 PR 时触发。

| Job | 内容 |
|---|---|
| `fmt` | `cargo fmt --check` |
| `clippy` | `cargo clippy --locked --all-features -- -D warnings`（真实门禁） |
| `check` | `cargo check --locked --all` |
| `core-test` | core fmt / clippy / nextest（host） |
| `docs-lint` | `typos` + `markdownlint-cli2@0.17.2` |
| `deny` | `cargo deny check advisories licenses` |
| `build` | feature 矩阵编译 → release 构建 → 栈预算 → 镜像生成与大小门禁 → 上传产物 |

构建产物（artifact `firmware`）：

- `xiaozhi-s3.bin`：应用镜像（用于 OTA / `write-bin 0x20000`）
- `xiaozhi-s3-merged.bin`：整片镜像（bootloader + 分区表 + app）

## 4. 发布

```bash
# 1. 确认门禁全绿
./scripts/check.sh --with-docs

# 2. 打 tag 并推送（触发 release.yml）
git tag -a v1.0.0 -m "v1.0.0"
git push origin v1.0.0
```

`release.yml` 会：

1. `cargo build --release --locked`；
2. 生成应用镜像与整片镜像，校验应用镜像 ≤ OTA 槽 90%；
3. 生成 `SHA256SUMS`；
4. 用 git-cliff 生成 CHANGELOG；
5. 创建 GitHub Release，上传两个镜像 + 校验和 + CHANGELOG。

也可在 Actions 页面手动 `Run workflow` 并填写 tag。

### 烧录

```bash
# 首次 USB 全量烧录（整片镜像）
espflash write-bin 0x0 xiaozhi-s3-v1.0.0-merged.bin

# 仅应用镜像（写入 ota_0）
espflash write-bin 0x20000 xiaozhi-s3-v1.0.0.bin

# 校验
sha256sum -c SHA256SUMS
espflash image_info --chip esp32s3 xiaozhi-s3-v1.0.0.bin
```

> OTA 当前只做 CRC 完整性校验，尚未启用签名验签，见 [`../SECURITY.md`](../SECURITY.md)。

## 5. 发布检查清单

- [ ] `scripts/check.sh --with-docs` 通过
- [ ] core 改动已在子仓库提交并推送，父仓库指针已更新
- [ ] 版本号与 `Cargo.toml` 一致
- [ ] 真机验证项（ROADMAP）已完成并记录
- [ ] release tag 符合 `v[0-9]+.*`
