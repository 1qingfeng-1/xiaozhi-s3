# GitHub Actions 快速参考

## 触发 CI

```bash
# Push to main (自动触发)
git push origin main

# Push tag (触发 release)
git tag -a v1.0.0 -m "Release v1.0.0"
git push origin v1.0.0
```

## 查看状态

```bash
# GitHub 界面
https://github.com/<owner>/<repo>/actions

# 查看具体的 workflow run
https://github.com/<owner>/<repo>/actions/runs/<run-id>

# 下载 artifact
# 在 workflow run 页面点击 "Artifacts" 标签
```

## 手动触发 Release

```bash
# 通过 GitHub Actions 界面
# 1. 点击 "Actions" 标签
# 2. 选择 "Release" workflow
# 3. 点击 "Run workflow"
# 4. 输入 tag (可选)
# 5. 点击 "Run workflow"
```

## 本地测试

```bash
# 运行 CI 检查
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo check --all
cargo test --all-features

# 构建 release
cargo build --release

# 检查固件大小
ls -lh target/xtensa-esp32s3-none-elf/release/xiaozhi-s3
```

## 创建 Release

```bash
# 1. 确保所有检查通过
cargo test

# 2. 更新版本 (可选)
# 编辑 Cargo.toml 中的 version 字段

# 3. 创建 tag
git tag -a v1.0.0 -m "Release v1.0.0"

# 4. 推送 tag (触发 release)
git push origin v1.0.0
```

## 故障排除

### CI 失败

```bash
# 1. 查看错误日志
# 在 GitHub Actions 页面点击失败的 job

# 2. 本地复现
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings

# 3. 修复问题
# 根据错误信息修复代码

# 4. 重新推送
git push origin main
```

### Release 失败

```bash
# 1. 检查 tag
git tag -l
git log --oneline

# 2. 检查权限
# 确保 workflow 有 contents: write 权限

# 3. 检查 artifact
ls -lh target/xtensa-esp32s3-none-elf/release/

# 4. 重新触发
git push origin --tags
# 或手动触发
```

## 常用命令

```bash
# 查看所有 tag
git tag -l

# 查看最近的 tag
git tag -l | sort -V | tail -5

# 删除本地 tag
git tag -d v1.0.0

# 删除远程 tag
git push origin :refs/tags/v1.0.0

# 查看 tag 详情
git show v1.0.0
```

## 文件列表

```
.github/workflows/
├── ci.yml        # CI 工作流
└── release.yml   # Release 工作流

GITHUB_ACTIONS.md       # 详细文档
```

## 文档链接

- GitHub Actions: https://docs.github.com/actions
- Rust 嵌入式: https://docs.espressif.com/projects/rust/book/
- git-cliff: https://git-cliff.org/
- Semantic Versioning: https://semver.org/
