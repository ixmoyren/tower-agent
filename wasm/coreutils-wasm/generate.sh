#!/usr/bin/env bash
#
# 重新生成 codemode 的命令层组件（uutils/coreutils multicall）。
#
# 与 monty-sandbox 不同，这里**不需要** wasi-sdk / wasm-opt / wasm-tools：
# `wasm32-wasip2` 目标由 rustc 直出组件（代价是 wasm-opt 处理不了组件，体积靠 profile 压）。
#
# 产物：crates/tower-agent-codemode/assets/coreutils.wasm（约 7.4 MB，**入库**——
# 宿主 crate 用 include_bytes! 内嵌，形状与 monty-sandbox 一致）。
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
target_dir="${CARGO_TARGET_DIR:-$here/target}"
out="$here/../../crates/tower-agent-codemode/assets/coreutils.wasm"

if ! rustup target list --installed | grep -qx wasm32-wasip2; then
    echo "缺少 wasm32-wasip2 target：rustup target add wasm32-wasip2" >&2
    exit 1
fi

cd "$here"

# 额外参数透传给 cargo（例如 `./generate.sh --offline`）。首次解析依赖需要联网，
# 之后 Cargo.lock 固定版本，就能纯离线重建。
echo "[1/2] cargo build --release --target wasm32-wasip2 $*"
# 显式指定 target dir：开发机的 ~/.cargo/config.toml 可能把 target-dir 指到别处。
CARGO_TARGET_DIR="$target_dir" cargo build --release --target wasm32-wasip2 "$@"

echo "[2/2] 落到宿主 crate 的 assets/"
mkdir -p "$(dirname "$out")"
cp "$target_dir/wasm32-wasip2/release/coreutils.wasm" "$out"
echo "wrote $out ($(du -h "$out" | cut -f1))"
