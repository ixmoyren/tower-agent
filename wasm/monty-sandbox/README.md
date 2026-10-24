# monty-sandbox (wasm component)

把 [monty](https://crates.io/crates/monty) 解释器编译成一个 wasm 组件，供
`tower-agent-plugin-monty` 用 wasmtime 在沙箱里执行 AI 生成的 mods 代码。

## 接口

`wit/sandbox.wit` 定义了一个宿主驱动的 REPL 状态机（`tower-agent:monty/sandbox`）：

- `start(source)`：新建会话并喂入源码，返回首个 `progress`；
- `resume-function(call-id, result-json)` / `reject-function(call-id, message)`：
  恢复一次挂起的外部函数调用；
- `resume-name(name)`：未定义名字（回 `NameError`）；
- `resume-os(call-id, result-json)`：恢复一次 OS 调用（宿主按挂载表服务，或回
  `not_handled` 让沙箱用默认异常）。

`progress` 是 `complete` / `function-call` / `name-lookup` / `os-call` / `error`
的变体。外部函数调用与 OS 调用都以 JSON 字符串过界（`os-call` 携带序列化的
`OsFunctionCall`）。每个组件实例内部用一个 `thread_local` 保存当前挂起的 REPL
进度；宿主每次运行以 `start` 开头会重置它，因此实例可以进池复用。

资源上限（见 `src/lib.rs`：单次 feed 5s / turn 2s / 递归 200 / 挂起 10000）
在组件内强制，避免 `while True: pass` 之类占死执行。

## 重新生成

```sh
./generate.sh
```

产物写到 `crates/tower-agent-plugin-monty/assets/monty-sandbox.wasm`。管线：

1. `cargo component bindings` 生成 WIT 绑定；
2. `cargo build --release --target wasm32-wasip2` 由 rustc 直出 component。

依赖：`cargo-component`、`wasm32-wasip2` target。组件体积由 release profile 的
`opt-level = "z"`、LTO 和 strip 控制；`wasm-opt` 不支持 component，因此不参与构建。
