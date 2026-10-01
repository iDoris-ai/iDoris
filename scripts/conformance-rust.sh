#!/usr/bin/env bash
# scripts/conformance-rust.sh — 用 Rust `idoris` 二进制作被测对象跑 conformance 套件。
#
# 前提：已安装 Rust 工具链、Node 和 pnpm，并执行 pnpm install --frozen-lockfile。
#
# 用法：
#   bash scripts/conformance-rust.sh
#
# 始终测试本次构建的 release 二进制；构建或契约断言失败直接返回非零。
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

if [ ! -d conformance ]; then
  echo "[conformance-rust] 找不到 conformance/ 目录。" >&2
  echo "[conformance-rust] 当前分支缺少 conformance/（PR #49 已于 2026-10-01 合并进 main），先同步 main。" >&2
  exit 1
fi

echo "[conformance-rust] cargo build --release --locked -p idoris-router --bin idoris" >&2
cargo build --release --locked -p idoris-router --bin idoris

bin="$root/target/release/idoris"
if [ ! -x "$bin" ]; then
  echo "[conformance-rust] 构建产物不存在或不可执行：$bin" >&2
  exit 1
fi

# 二进制通过环境变量配置启动，不追加 CLI 参数。harness 优先读取 ARGV，
# 显式生成 JSON 数组，既覆盖外部命令配置，也支持路径含空格。
export IDORIS_CONFORMANCE_CMD="$bin"
export IDORIS_CONFORMANCE_ARGV
IDORIS_CONFORMANCE_ARGV="$(node -e 'process.stdout.write(JSON.stringify([process.argv[1]]))' "$bin")"

echo "[conformance-rust] IDORIS_CONFORMANCE_CMD=$IDORIS_CONFORMANCE_CMD" >&2
echo "[conformance-rust] pnpm conformance" >&2
exec pnpm conformance
