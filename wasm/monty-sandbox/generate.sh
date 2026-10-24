#!/usr/bin/env bash
#
# 重新生成内嵌到 `tower-agent-plugin-monty` 的 wasm 沙箱组件。
#
# 流程：生成绑定 -> 编译 wasm32-wasip2 组件 -> 落到 assets。
# wasip2 目标由 rustc 直出 component；体积由 Cargo release profile 优化，
# 不再经过 preview1 adapter 或 wasm-tools/wasm-opt。
#
# 依赖：cargo-component、wasm32-wasip2 target。
# 产物：crates/tower-agent-plugin-monty/assets/monty-sandbox.wasm
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
target_dir="${CARGO_TARGET_DIR:-$here/target}"
out="$root/crates/tower-agent-plugin-monty/assets/monty-sandbox.wasm"
component="$target_dir/wasm32-wasip2/release/monty_sandbox.wasm"

if ! rustup target list --installed | grep -qx wasm32-wasip2; then
  echo "缺少 wasm32-wasip2 target：rustup target add wasm32-wasip2" >&2
  exit 1
fi

cd "$here"

echo "[1/2] 生成 WIT 绑定 (cargo component bindings)"
cargo component bindings

echo "[2/2] 编译 wasm32-wasip2 组件"
cargo build --release --target wasm32-wasip2 --target-dir "$target_dir" "$@"

mkdir -p "$(dirname "$out")"
cp "$component" "$out"
echo "wrote $out ($(du -h "$out" | cut -f1))"
