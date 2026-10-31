# 开发快捷命令 (安装: cargo install just)
# 用法: just / just check / just fuzz parse_ws 20000
set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

default: check

# 与 CI 同序的门禁
check:
    ./scripts/check.sh

# 额外跑 typos / markdownlint / cargo-deny
check-docs:
    ./scripts/check.sh --with-docs

# 格式化父仓库与 core
fmt:
    cargo fmt --all
    cd xiaozhi-core && cargo +stable fmt --all

# core host 测试
core-test:
    cd xiaozhi-core && cargo +stable nextest run --locked

# core host clippy
core-clippy:
    cd xiaozhi-core && cargo +stable clippy --locked --all-targets -- -D warnings

# 变异测试 (耗时, 本地按需)
mutants:
    cd xiaozhi-core && cargo mutants --test-tool nextest

# 诊断/自测 feature 组合编译检查
features:
    ./scripts/check_features.sh

# core 分层门禁 (禁止硬件依赖)
layers:
    ./scripts/check_layering.sh

# Markdown 相对链接检查 (需 cargo install lychee)
links:
    lychee --offline --no-progress --exclude-path docs/archive --exclude-path vendor '**/*.md'

# release 构建
build:
    cargo build --release --locked

# 生成应用镜像与整片镜像到 dist/
image:
    cargo build --release --locked
    mkdir -p dist
    espflash save-image --chip esp32s3 --flash-size 16mb \
        --bootloader assets/bootloader-esp32s3-rollback.bin \
        target/xtensa-esp32s3-none-elf/release/xiaozhi-s3 dist/xiaozhi-s3.bin
    espflash save-image --chip esp32s3 --flash-size 16mb --merge --skip-padding \
        --bootloader assets/bootloader-esp32s3-rollback.bin \
        target/xtensa-esp32s3-none-elf/release/xiaozhi-s3 dist/xiaozhi-s3-merged.bin

# 烧录 + defmt 监视
flash:
    cargo run --release --bin xiaozhi-s3

# 主任务栈预算
stack:
    ./scripts/check_stack.sh target/xtensa-esp32s3-none-elf/release/xiaozhi-s3

# 本地 fuzz (从仓库外启动, 避开 build-std 配置)
fuzz target="parse_ws" runs="10000":
    cd /tmp && cargo +nightly fuzz run {{target}} \
        --fuzz-dir {{justfile_directory()}}/xiaozhi-core/fuzz -- -runs={{runs}}
