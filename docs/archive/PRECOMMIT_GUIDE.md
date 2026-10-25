# Pre-commit 配置说明

## 概述

本项目使用 pre-commit 钩子来确保代码质量。配置针对嵌入式 Rust 项目进行了优化。

## 安装

### 1. 安装 Python 和 pip

```bash
# Ubuntu/Debian
sudo apt-get install python3 python3-pip

# macOS (使用 Homebrew)
brew install python3

# Windows
# 从 https://www.python.org/downloads/ 下载安装
```

### 2. 安装 pre-commit

```bash
pip3 install pre-commit
```

### 3. 安装钩子

```bash
cd /home/dev/code/xiaozhi/xiaozhi-s3
pre-commit install
```

## 使用

### 基本命令

```bash
# 安装钩子 (只需运行一次)
pre-commit install

# 提交前自动运行所有钩子
git add .
git commit -m "feat: add new feature"

# 手动运行所有钩子 (检查所有文件)
pre-commit run --all-files

# 手动运行所有钩子 (只检查暂存的文件)
pre-commit run

# 运行特定的钩子
pre-commit run cargo-fmt
pre-commit run cargo-clippy
pre-commit run cargo-check
pre-commit run cargo-deny
pre-commit run typos

# 跳过钩子 (紧急情况，不推荐)
git commit --no-verify -m "fix: urgent fix"

# 更新所有钩子到最新版本
pre-commit autoupdate

# 卸载钩子
pre-commit uninstall
```

## 钩子说明

### 1. cargo-fmt
- **功能**: 代码格式化
- **配置**: `rustfmt.toml`
- **说明**: 确保代码风格一致
- **耗时**: 快

### 2. cargo-clippy
- **功能**: 静态分析
- **配置**: `clippy.toml`
- **说明**: 发现潜在问题，针对嵌入式优化
- **耗时**: 慢 (需要编译)

### 3. cargo-check
- **功能**: 类型检查
- **配置**: `Cargo.toml`
- **说明**: 验证代码可以编译
- **耗时**: 中等

### 4. cargo-deny
- **功能**: 依赖安全检查
- **配置**: `deny.toml`
- **说明**: 检查安全漏洞和许可证
- **耗时**: 中等

### 5. typos
- **功能**: 拼写检查
- **配置**: `_typos.toml`
- **说明**: 发现拼写错误
- **耗时**: 快

### 6. check-binaries
- **功能**: 二进制文件检查
- **配置**: `.pre-commit-config.yaml`
- **说明**: 防止意外提交大型二进制文件
- **耗时**: 快

## 嵌入式特定配置

### 栈大小
- ESP32-S3 默认栈大小: 8KB
- 避免递归和大的栈上数组
- 使用堆分配或静态缓冲区

### 内存占用
- ESP32-S3 有 512KB SRAM
- 避免不必要的堆分配
- 使用 esp-alloc 管理内存

### 依赖管理
- 尽量使用 no_std 兼容的 crate
- 避免引入大型依赖库
- 定期更新依赖，修复安全漏洞

### 固件大小
- ESP32-S3 有 16MB Flash
- 固件越小越好 (OTA 升级更快)
- 避免引入不必要的功能

## 常见问题

### Q: 钩子运行很慢
**A:**
- cargo-clippy 和 cargo-check 需要编译依赖
- 首次运行会下载和编译所有依赖
- 后续运行会快很多 (使用缓存)

### Q: 某个钩子失败了
**A:**
- 查看错误信息，修复问题
- 重新运行钩子: `pre-commit run <hook-name>`
- 如果问题复杂，可以跳过: `git commit --no-verify`
- 但事后应该修复问题

### Q: 如何添加新的钩子
**A:**
- 编辑 `.pre-commit-config.yaml`
- 添加新的钩子配置
- 参考 pre-commit 文档: https://pre-commit.com/

### Q: 如何禁用某个钩子
**A:**
- 从 `.pre-commit-config.yaml` 中删除
- 或者注释掉
- 或者在 `.pre-commit-config.yaml` 中添加 `exclude` 规则

### Q: 如何在 CI/CD 中使用
**A:**
- 安装 pre-commit: `pip3 install pre-commit`
- 运行检查: `pre-commit run --all-files`
- 或者直接使用 cargo 命令:
  - `cargo fmt --all -- --check`
  - `cargo clippy --all-targets --all-features -- -D warnings`
  - `cargo check --all`
  - `cargo deny check advisories licenses`

## 最佳实践

1. **提交前运行所有检查**
   ```bash
   pre-commit run --all-files
   ```

2. **修复所有警告**
   - 不要忽略 Clippy 警告
   - 修复拼写错误
   - 保持代码格式一致

3. **定期更新依赖**
   ```bash
   cargo update
   ```

4. **检查安全漏洞**
   ```bash
   cargo deny check advisories
   ```

5. **保持固件精简**
   - 只添加必要的功能
   - 避免引入大型依赖库
   - 定期审查依赖

## 钩子执行顺序

pre-commit 按照配置文件中定义的顺序执行钩子：

1. 通用文件检查 (BOM, 符号链接, YAML 等)
2. cargo-fmt (代码格式化)
3. cargo-clippy (静态分析)
4. cargo-check (类型检查)
5. cargo-deny (依赖安全检查)
6. typos (拼写检查)
7. check-binaries (二进制文件检查)

## 排除规则

以下文件/目录会被排除：

- `target/` - 构建输出
- `.git/` - Git 仓库
- `.github/` - GitHub 配置
- `CHANGELOG.md` - 变更日志
- `.pre-commit-config.yaml` - pre-commit 配置
- `Cargo.lock` - 依赖锁定文件

## 故障排除

### 钩子无法运行

```bash
# 检查 pre-commit 是否安装
pre-commit --version

# 重新安装钩子
pre-commit uninstall
pre-commit install

# 清除缓存
pre-commit clean

# 重新运行
pre-commit run --all-files
```

### 权限问题

```bash
# 确保钩子可执行
chmod +x .git/hooks/pre-commit
```

### 网络问题

```bash
# 检查网络连接
curl https://github.com

# 如果使用代理，设置环境变量
export https_proxy=http://proxy:port
```

## 参考资料

- pre-commit 文档: https://pre-commit.com/
- Rust 格式化: https://rust-lang.github.io/rustfmt/
- Clippy: https://rust-lang.github.io/rust-clippy/
- cargo-deny: https://github.com/EmbarkStudios/cargo-deny
- typos: https://github.com/crate-ci/typos

## 版本历史

- **v1.0** (2026-08-24) - 初始版本
  - 基础钩子配置
  - 嵌入式特定优化
  - 完整的文档说明
