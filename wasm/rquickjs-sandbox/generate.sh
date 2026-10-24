#!/usr/bin/env bash
#
# 重新生成 codemode 的 JS 沙箱组件（rquickjs）。
#
# 依赖：cargo-component、`wasm32-wasip2` target，以及 **WASI_SDK**：rquickjs 要编 quickjs 的
# C 源码（crates.io 上的 rquickjs-sys 自带这 4 个 .c 文件，所以**不需要子模块或 vendor**）。
#
# 产物：crates/tower-agent-plugin-rquickjs/assets/codemode.wasm（**入库**，被宿主 include_bytes! 内嵌；
# src/bindings.rs 与 target/ 不入库）
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
target_dir="${CARGO_TARGET_DIR:-$here/target}"
out="$here/../../crates/tower-agent-plugin-rquickjs/assets/codemode.wasm"

if [ -z "${WASI_SDK:-}" ]; then
    echo "需要 WASI_SDK=<wasi-sdk 根目录>（rquickjs 编 quickjs C 源码用；本机在 ~/.cache/wasi-sdk-*）" >&2
    exit 1
fi
if ! rustup target list --installed | grep -qx wasm32-wasip2; then
    echo "缺少 wasm32-wasip2 target：rustup target add wasm32-wasip2" >&2
    exit 1
fi

cd "$here"

echo "[1/3] cargo component bindings"
cargo component bindings

echo "[2/3] cargo component build --release --target wasm32-wasip2 $*"
CARGO_TARGET_DIR="$target_dir" cargo component build --release --target wasm32-wasip2 "$@"

echo "[3/3] 落到宿主 crate 的 assets/"
mkdir -p "$(dirname "$out")"
cp "$target_dir/wasm32-wasip2/release/rquickjs_sandbox.wasm" "$out"
echo "wrote $out ($(du -h "$out" | cut -f1))"
