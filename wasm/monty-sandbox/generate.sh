#!/usr/bin/env bash
#
# 重新生成内嵌到 `tower-agent-monty` 的 wasm 沙箱组件。
#
# 流程：生成绑定 -> 编译 wasm32-wasip1 核心模块 -> wasm-opt 优化 ->
# 组件化（embed + component new，使用 preview1 reactor adapter）-> 落到 assets。
#
# 依赖：cargo-component、wasm-opt（binaryen）、wasm-tools、wasm32-wasip1 target。
# 产物：crates/tower-agent-monty/assets/monty-sandbox.wasm
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
target_dir="${CARGO_TARGET_DIR:-$here/target}"
adapter="$here/adapters/wasi_snapshot_preview1.reactor.wasm"
out="$root/crates/tower-agent-monty/assets/monty-sandbox.wasm"

core="$target_dir/wasm32-wasip1/release/monty_sandbox.wasm"
optimized="$target_dir/monty_sandbox.opt.wasm"
embedded="$target_dir/monty_sandbox.embedded.wasm"
component="$target_dir/monty_sandbox.component.wasm"

cd "$here"

echo "[1/4] 生成 WIT 绑定 (cargo component bindings)"
cargo component bindings

echo "[2/4] 编译核心模块 (wasm32-wasip1)"
cargo build --release --target wasm32-wasip1

echo "[3/4] wasm-opt -Oz --strip-debug"
wasm-opt -Oz --strip-debug "$core" -o "$optimized"

echo "[4/4] 组件化 (component embed + new)"
wasm-tools component embed "$here/wit" --world monty-sandbox "$optimized" -o "$embedded"
wasm-tools component new "$embedded" \
  --adapt "wasi_snapshot_preview1=$adapter" \
  -o "$component"

mkdir -p "$(dirname "$out")"
cp "$component" "$out"
echo "wrote $out ($(du -h "$out" | cut -f1))"
