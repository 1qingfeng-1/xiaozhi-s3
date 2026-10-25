# GitHub Actions CI/CD 配置说明

## 概述

本项目使用 GitHub Actions 来自动化构建、测试和发布流程。配置针对嵌入式 Rust 项目进行了优化。

## 工作流文件

```
.github/workflows/
├── ci.yml        # CI 工作流 (构建、测试、检查)
└── release.yml   # Release 工作流 (发布固件)
```

## CI 工作流 (ci.yml)

### 触发条件

- **Push to main**: 每次推送到 main 分支时自动运行
- **Pull Request**: 每次创建或更新 PR 时自动运行

### 检查项目

1. **代码格式化** (cargo fmt)
   - 检查代码格式是否符合 rustfmt 配置
   - 快速检查，不编译代码

2. **Clippy 静态分析** (cargo clippy)
   - 检查代码质量和潜在问题
   - 针对嵌入式优化 (栈大小, 内存占用)
   - 需要编译依赖

3. **类型检查** (cargo check)
   - 验证代码可以编译
   - 不生成二进制文件
   - 快速检查

4. **单元测试** (cargo test)
   - 运行所有测试
   - 验证代码功能

5. **构建固件** (cargo build --release)
   - 构建 release 固件
   - 检查固件大小
   - 上传固件 artifact

### 缓存策略

启用缓存以加速构建：

- **缓存位置**:
  - `~/.cargo/registry` - crates.io 依赖
  - `~/.cargo/git` - Git 依赖
  - `target` - 构建产物

- **缓存 key**:
  ```
  ${{ runner.os }}-cargo-<job-name>-${{ hashFiles('**/Cargo.lock') }}
  ```

- **缓存恢复**:
  - 精确匹配: 完整的 key
  - 部分匹配: 按顺序尝试前缀匹配

### Artifact

- **名称**: firmware
- **路径**: target/xtensa-esp32s3-none-elf/release/xiaozhi-s3
- **保留时间**: 7 天

### 输出示例

```
=== Firmware Size Report ===
-rwxr-xr-x 1 root root 1.2M target/xtensa-esp32s3-none-elf/release/xiaozhi-s3
1.2M    target/xtensa-esp32s3-none-elf/release/xiaozhi-s3
Flash size: 16MB (16777216 bytes)
Firmware must be smaller than flash size
```

## Release 工作流 (release.yml)

### 触发条件

- **Push tag**: 推送匹配 `v[0-9]+.*` 的 tag 时自动运行
  - 示例: v1.0.0, v1.2.3, v2.0.0-beta.1
- **手动触发**: 通过 GitHub Actions 界面手动触发
  - 可以指定 tag 名称

### 发布流程

1. **构建 release 固件**
   - 使用 release 配置构建
   - 检查固件大小
   - 上传固件 artifact

2. **生成 changelog**
   - 使用 git-cliff 生成
   - 基于 conventional commits
   - 使用 cliff.toml 配置

3. **创建 GitHub Release**
   - 自动创建 release
   - 上传固件和 changelog
   - 生成 release notes

### 版本管理

使用 **Semantic Versioning** (语义化版本):

```
MAJOR.MINOR.PATCH

示例:
- v1.0.0     - 初始发布
- v1.1.0     - 新增功能
- v1.1.1     - Bug 修复
- v2.0.0     - 重大变更
- v1.0.0-beta.1 - Beta 版本
```

### Tag 命名规范

```bash
# 创建 tag
git tag -a v1.0.0 -m "Release v1.0.0"

# 推送 tag (触发 release)
git push origin v1.0.0

# 推送所有 tag
git push origin --tags
```

### Release 内容

每个 release 包含:

1. **固件文件**
   - `xiaozhi-s3-v1.0.0.bin` - 固件二进制
   - 大小: 约 1-2MB
   - 格式: ESP32-S3 bootloader 格式

2. **Changelog**
   - `CHANGELOG.md` - 变更日志
   - 格式: Markdown
   - 内容: 功能、修复、破坏性变更

3. **Release Notes**
   - 固件信息 (芯片、Flash、大小)
   - 烧录方法
   - 验证方法

### Artifact

- **名称**: firmware-release
- **路径**: target/xtensa-esp32s3-none-elf/release/xiaozhi-s3
- **保留时间**: 30 天

- **名称**: changelog
- **路径**: CHANGELOG.md
- **保留时间**: 30 天

## 权限配置

### CI 工作流

- **默认权限**: 只读
- **不需要特殊权限**

### Release 工作流

```yaml
permissions:
  contents: write  # 需要写入权限来创建 release
```

## 环境变量

```yaml
env:
  CARGO_TERM_COLOR: always      # 启用彩色输出
  RUSTUP_TOOLCHAIN: esp        # 使用 ESP 工具链
```

## 最佳实践

### 1. 提交前检查

```bash
# 本地运行 CI 检查
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo check --all
cargo test --all-features
```

### 2. 创建 release

```bash
# 1. 确保所有检查通过
cargo test

# 2. 更新版本
# 编辑 Cargo.toml 中的 version 字段

# 3. 创建 tag
git tag -a v1.0.0 -m "Release v1.0.0"

# 4. 推送 tag (触发 release)
git push origin v1.0.0
```

### 3. 监控 CI/CD

- **GitHub Actions 界面**: https://github.com/<owner>/<repo>/actions
- **查看日志**: 点击具体的 workflow run
- **下载 artifact**: 在 workflow run 页面下载
- **创建 release**: 在 Releases 页面查看

### 4. 故障排除

**CI 失败**
```bash
# 查看具体的错误日志
# 检查是哪个 job 失败
# 复制错误信息，本地复现
```

**Release 失败**
```bash
# 检查 tag 是否正确推送
git tag -l
git log --oneline

# 检查权限
# 确保 workflow 有 contents: write 权限

# 检查 artifact
# 确保固件文件存在
```

## 缓存优化

### 缓存命中

- **首次运行**: 缓存未命中，下载所有依赖
- **后续运行**: 缓存命中，跳过下载
- **依赖变更**: Cargo.lock 变化时，缓存失效

### 缓存大小

- **cargo registry**: ~100MB
- **cargo git**: ~10MB
- **target**: ~500MB (取决于项目大小)
- **总计**: ~600MB

### 缓存失效

以下情况会导致缓存失效:

- Cargo.lock 变化
- 手动清除缓存
- 缓存过期 (GitHub Actions 自动管理)

## 安全性

### 依赖安全

- **cargo-deny**: 检查安全漏洞和许可证
- **RustSec**: 使用 RustSec 数据库
- **定期更新**: `cargo update`

### 固件安全

- **签名**: 考虑添加固件签名 (未来功能)
- **校验**: 提供固件 SHA256 校验和
- **版本控制**: 使用语义化版本，避免覆盖旧版本

## 成本优化

### GitHub Actions 分钟数

- **CI 工作流**: ~5-10 分钟
  - 格式化: ~1 分钟
  - Clippy: ~3 分钟
  - Check: ~2 分钟
  - Test: ~2 分钟
  - Build: ~2 分钟

- **Release 工作流**: ~5-8 分钟
  - Build: ~3 分钟
  - Changelog: ~1 分钟
  - Release: ~1 分钟

### 优化建议

- **启用缓存**: 减少依赖下载时间
- **并行 job**: 独立的 job 并行运行
- **条件执行**: 只运行必要的检查
- **限制 artifact**: 设置合理的保留时间

## 监控和告警

### 监控指标

- **CI 成功率**: 最近 30 天的 CI 成功率
- **构建时间**: 平均构建时间趋势
- **固件大小**: 固件大小变化趋势
- **依赖安全**: 安全漏洞数量

### 告警规则

- **CI 失败**: 立即通知
- **构建时间过长**: 超过 15 分钟时告警
- **固件过大**: 超过 4MB 时告警
- **安全漏洞**: 发现高危漏洞时告警

## 参考资料

- GitHub Actions 文档: https://docs.github.com/actions
- Rust 嵌入式开发: https://docs.espressif.com/projects/rust/book/
- git-cliff: https://git-cliff.org/
- Semantic Versioning: https://semver.org/
- Conventional Commits: https://www.conventionalcommits.org/

## 版本历史

- **v1.0** (2026-08-24) - 初始版本
  - CI 工作流 (基础检查)
  - Release 工作流 (基础发布)
  - 缓存优化
  - 完整的文档说明
