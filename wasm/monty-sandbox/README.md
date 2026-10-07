# monty-sandbox (wasm component)

把 [monty](https://crates.io/crates/monty) 解释器编译成一个 wasm 组件，供
`tower-agent-monty` 用 wasmtime 在沙箱里执行 AI 生成的 mods 代码。

## 接口

`wit/sandbox.wit` 定义了一个宿主驱动的 REPL 状态机（`tower-agent:monty/sandbox`）：

- `start(source)`：新建会话并喂入源码，返回首个 `progress`；
- `resume-function(call-id, result-json)` / `reject-function(call-id, message)`：
  恢复一次挂起的外部函数调用；
- `resume-name(name)` / `resume-os(call-id)`：未定义名字 / OS 调用的默认处理。

`progress` 是 `complete` / `function-call` / `name-lookup` / `os-call` / `error`
的变体，参数与返回值都以 JSON 字符串过界。每个组件实例内部用一个 `thread_local`
保存当前挂起的 REPL 进度，因此「一个组件实例 = 一个 mod 会话」。

## 重新生成

```sh
./generate.sh
```

产物写到 `crates/tower-agent-monty/assets/monty-sandbox.wasm`。管线：

1. `cargo component bindings` 生成 WIT 绑定；
2. `cargo build --release --target wasm32-wasip1` 得到核心模块；
3. `wasm-opt -Oz --strip-debug` 优化（Binaryen 目前不支持 component，所以必须在
   组件化之前对核心模块做优化）；
4. `wasm-tools component embed` + `component new --adapt ...` 组件化。

依赖：`cargo-component`、`wasm-opt`（binaryen）、`wasm-tools`、`wasm32-wasip1` target。

`adapters/wasi_snapshot_preview1.reactor.wasm` 是 WASI preview1 → preview2 的官方
reactor adapter（Apache-2.0 WITH LLVM-exception），由各组件工具链（cargo-component、
wasm-tools、spin 等）内置分发；这里 vendored 一份以保证离线、可复现的构建。
