#!/usr/bin/env bash
# scripts/conformance-rust.sh — 用 Rust `idoris` 二进制作被测对象跑 conformance 套件。
#
# 前提：
#   `conformance/` 目录 + 根 package.json 的 `pnpm conformance` 脚本来自
#   PR #49（2026-10-01 已合并进 main）。当前分支要包含它们，否则第一步就会报错退出。
#
# 用法：
#   bash scripts/conformance-rust.sh
#
# R6：Rust `idoris` 是生产候选实现。B1/B2/B3 已补齐 Rust 请求核心、
# 推荐器与订阅中转；CI 的 rust job 必跑本脚本，shared conformance 失败即阻止
# 默认实现切换/发版。TS 只保留为参考/PoC，不再承载新的产品能力。
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

if [ ! -d conformance ]; then
  echo "[conformance-rust] 找不到 conformance/ 目录。" >&2
  echo "[conformance-rust] 当前分支缺少 conformance/（PR #49 已于 2026-10-01 合并进 main），先同步 main。" >&2
  exit 1
fi

echo "[conformance-rust] cargo build --release -p idoris-router" >&2
cargo build --release --locked -p idoris-router

bin="${CARGO_TARGET_DIR:-$root/target}/release/idoris"
if [ ! -x "$bin" ]; then
  echo "[conformance-rust] 构建产物不存在或不可执行：$bin" >&2
  exit 1
fi

# 与 release 布局一致：默认策略从可执行文件旁读取，而非仓库 cwd。
mkdir -p "$(dirname "$bin")/config"
cp config/routing-policy.yaml "$(dirname "$bin")/config/routing-policy.yaml"
cp config/catalog.yaml "$(dirname "$bin")/config/catalog.yaml"

# 正式走 task34 的 `serve` 入口；裸启动继续由 portable_startup 锁定兼容性。
export IDORIS_CONFORMANCE_CMD="$bin serve"
export IDORIS_CONFORMANCE_ARGV
IDORIS_CONFORMANCE_ARGV="$(node -e 'process.stdout.write(JSON.stringify([process.argv[1], "serve"]))' "$bin")"
# K13/M4: a 5xx does not prove the upstream POST was not executed.
export IDORIS_CONFORMANCE_POST_RETRY=0
# Shared conformance normally targets the TS reference. A few explicitly
# approved D-B1-1 edge differences are locked per implementation instead of
# forcing one side to mimic the other.
export IDORIS_CONFORMANCE_IMPLEMENTATION=rust

echo "[conformance-rust] IDORIS_CONFORMANCE_CMD=$IDORIS_CONFORMANCE_CMD" >&2
echo "[conformance-rust] pnpm conformance" >&2
exec pnpm conformance
