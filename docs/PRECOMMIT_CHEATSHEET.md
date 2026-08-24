# Pre-commit 快速参考

## 常用命令

```bash
# 验证配置
pre-commit validate-config .pre-commit-config.yaml

# 安装钩子
pre-commit install

# 运行所有检查
pre-commit run --all-files

# 运行特定钩子
pre-commit run cargo-fmt
pre-commit run cargo-clippy
pre-commit run cargo-check
pre-commit run cargo-deny
pre-commit run typos

# 跳过钩子 (紧急情况)
git commit --no-verify -m "fix: urgent fix"

# 更新钩子
pre-commit autoupdate

# 卸载钩子
pre-commit uninstall

# 清除缓存
pre-commit clean
```

## 钩子说明

| 钩子 | 功能 | 速度 | 配置 |
|------|------|------|------|
| cargo-fmt | 代码格式化 | 快 | rustfmt.toml |
| cargo-clippy | 静态分析 | 慢 | clippy.toml |
| cargo-check | 类型检查 | 中等 | Cargo.toml |
| cargo-deny | 依赖安全检查 | 中等 | deny.toml |
| typos | 拼写检查 | 快 | _typos.toml |
| markdownlint | Markdown 检查 | 快 | .markdownlint.json |

## 嵌入式限制

```
ESP32-S3:
- 栈大小: 8KB (避免递归和大数组)
- SRAM: 512KB (避免不必要的堆分配)
- Flash: 16MB (保持固件精简)
```

## 最佳实践

1. **提交前**
   ```bash
   pre-commit run --all-files
   ```

2. **修复警告**
   - 不要忽略 Clippy 警告
   - 修复拼写错误
   - 保持代码格式一致

3. **定期更新**
   ```bash
   pre-commit autoupdate
   cargo update
   cargo deny check advisories
   ```

## 故障排除

```bash
# 检查 pre-commit 版本
pre-commit --version

# 重新安装钩子
pre-commit uninstall
pre-commit install

# 清除缓存
pre-commit clean

# 重新运行
pre-commit run --all-files
```

## 环境变量

```bash
# 设置 PATH (如果 pre-commit 不在 PATH 中)
export PATH="/home/dev/.local/share/uv/tools/pre-commit/bin:$PATH"

# 如果使用代理
export https_proxy=http://proxy:port
```

## 文件列表

```
.pre-commit-config.yaml   # pre-commit 配置
rustfmt.toml              # Rust 格式化配置
clippy.toml               # Clippy 配置
deny.toml                 # cargo-deny 配置
_typos.toml               # typos 配置
PRECOMMIT.md              # 使用说明
PRECOMMIT_GUIDE.md        # 详细指南
PRECOMMIT_SUMMARY.md      # 完成总结
```

## 文档链接

- pre-commit: https://pre-commit.com/
- rustfmt: https://rust-lang.github.io/rustfmt/
- Clippy: https://rust-lang.github.io/rust-clippy/
- cargo-deny: https://github.com/EmbarkStudios/cargo-deny
- typos: https://github.com/crate-ci/typos
