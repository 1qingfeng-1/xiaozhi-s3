#!/usr/bin/env bash
# 统一门禁: 与 CI / pre-commit 保持同一顺序
#
# 用法: scripts/check.sh [--with-docs]
#   --with-docs  额外跑 cargo-deny / typos / markdownlint (需本地已安装)
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

echo "== 固件 fmt =="
cargo fmt --all -- --check

echo "== 固件 clippy =="
cargo clippy --all-features -- -D warnings

echo "== 固件 check =="
cargo check --all

echo "== core fmt/clippy/tests =="
(
    cd xiaozhi-core
    cargo +stable fmt --all -- --check
    cargo +stable clippy --all-targets -- -D warnings
    if command -v cargo-nextest >/dev/null 2>&1; then
        cargo +stable nextest run
    else
        cargo +stable test
    fi
)

if [ "${1:-}" = "--with-docs" ]; then
    echo "== 依赖审计 =="
    if command -v cargo-deny >/dev/null 2>&1; then
        cargo deny check advisories licenses
    else
        echo "跳过: 未安装 cargo-deny" >&2
    fi
    echo "== 拼写 =="
    if command -v typos >/dev/null 2>&1; then
        typos
    else
        echo "跳过: 未安装 typos" >&2
    fi
    echo "== Markdown =="
    if command -v markdownlint-cli2 >/dev/null 2>&1; then
        markdownlint-cli2 "**/*.md" "#target" "#vendor" "#CHANGELOG.md" "#docs/archive"
    else
        echo "跳过: 未安装 markdownlint-cli2" >&2
    fi
fi

echo "全部门禁通过。"
