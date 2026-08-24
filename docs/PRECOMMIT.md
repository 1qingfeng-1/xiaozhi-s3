# Git 钩子配置
# 确保 pre-commit 正确安装和使用

# 安装 pre-commit
# pip install pre-commit
# pre-commit install

# 运行所有钩子
# pre-commit run --all-files

# 只运行特定的钩子
# pre-commit run cargo-fmt
# pre-commit run cargo-clippy
# pre-commit run cargo-check

# 跳过钩子 (紧急情况)
# git commit --no-verify

# 更新钩子
# pre-commit autoupdate

# 卸载钩子
# pre-commit uninstall

# 嵌入式项目特定的钩子说明
# ================================

# 1. cargo-fmt
# - 确保代码格式一致
# - 使用 rustfmt.toml 中的配置
# - 快速检查，不会编译代码

# 2. cargo-clippy
# - 静态分析，发现潜在问题
# - 针对嵌入式优化 (栈大小, 内存占用)
# - 需要编译依赖，较慢

# 3. cargo-check
# - 快速类型检查
# - 不生成二进制文件
# - 验证代码可以编译

# 4. cargo-deny
# - 检查依赖安全性
# - 检查许可证兼容性
# - 使用 deny.toml 配置

# 5. typos
# - 拼写检查
# - 使用 _typos.toml 配置
# - 快速检查

# 6. check-binaries
# - 防止意外提交大型二进制文件
# - 固件文件 (.bin, .elf, .hex) 需要特别检查
# - 确保这些文件是有意提交的

# 最佳实践
# ========

# 1. 提交前运行所有检查
# pre-commit run --all-files

# 2. 如果某个钩子失败，修复后重新提交
# git add .
# git commit -m "fix: address pre-commit issues"

# 3. 不要跳过钩子，除非是紧急情况
# 如果必须跳过，事后应该运行检查并修复

# 4. 定期更新 pre-commit 配置
# pre-commit autoupdate

# 5. 在 CI/CD 中运行相同的检查
# 确保本地和 CI 的行为一致

# 嵌入式项目注意事项
# ==================

# 1. 栈大小
# - ESP32-S3 的栈大小有限 (默认 8KB)
# - 避免递归和大的栈上数组
# - 使用堆分配或静态缓冲区

# 2. 内存占用
# - ESP32-S3 有 512KB SRAM
# - 避免不必要的堆分配
# - 使用 esp-alloc 管理内存

# 3. 依赖管理
# - 尽量使用 no_std 兼容的 crate
# - 避免引入大型依赖库
# - 定期更新依赖，修复安全漏洞

# 4. 固件大小
# - ESP32-S3 有 16MB Flash
# - 但固件越小越好 (OTA 升级更快)
# - 避免引入不必要的功能

# 5. 性能
# - 嵌入式系统性能敏感
# - 避免不必要的计算
# - 使用硬件加速器 (I2S, DMA 等)
