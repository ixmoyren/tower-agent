# rquickjs-sandbox (wasm component)

codemode 的 **JS 沙箱**：宿主调 `run(code, opts)`，脚本在 rquickjs 里跑；脚本要用命令时，组件把
请求 `exec` 回宿主，宿主再去起 [`wasm/coreutils-wasm`](../coreutils-wasm/README.md)。设计与决策见
[`docs/codemode.md`](../../docs/codemode.md)。

这个 **确实是沙箱**（跑不受信脚本、有内存/时间/能力边界）；被它调用的 `coreutils-wasm` 才是「工具」。

## 接口

`wit/codemode.wit`（`tower-agent:codemode@0.1.0`）：

```wit
world codemode {
  import host;      // exec(name, args, opts) -> result<exec-result, string>
  export sandbox;   // run(code, opts) -> result<run-result, string>
}
```

- `exec` 用 `name`（裸命令名，如 `cat`）分发；stdin/stdout/stderr 都是字节列，避免把宿主类型塞进沙箱。
- `run-result` 带回：脚本 `return` 值的 JSON、`console.log` 收集到的 stdout、命令流水（审计）、
  是否截断。

## 脚本可见的 API（M1 版）

```js
const r = await exec("sort", ["/data/f.txt"]);   // → { code, stdout, stderr }
const txt = fs.readFile("/data/f.txt");
fs.writeFile("/out.txt", txt.split("\n").reverse().join("\n"));
console.log("done", fs.readdir("/data"));
return {lines: txt.split("\n").length};        // 返回值会以 JSON 带回给宿主
```

`exec` 是 **async 形状、同步实现**（首版 host import 是同步的）——将来换真正的异步 import，脚本不用改。
命令函数（`cat()`、`sort()` 那种 thenable 链式）是 M3 的事，M1 只有 `exec` 逃生口 + `fs` 子集。

脚本被包成 **async 函数**（`new AsyncFunction(src)()`），所以既能用顶层 `await`、也能 `return`；
组件内部手工驱动 microtask 队列把它结算掉（原生函数是同步的，通常一两轮就好）。

## 资源上限（`src/lib.rs` 顶部常量，见 docs §7.1）

| 项            | 值         | 备注                                                                                                                                                  |
|---------------|------------|-------------------------------------------------------------------------------------------------------------------------------------------------------|
| JS 栈         | **64 KiB** | **不要**改成上游 README 建议的 256 KiB。独立跑时 64 KiB 能抛可捕获的 `RangeError`，但**在组件里跑时深递归仍是硬 trap**（宿主降级成脚本失败）——见 §3.1 |
| JS 堆         | 64 MiB     | `Runtime::set_memory_limit`                                                                                                                           |
| 脚本总超时    | 30 s       | `set_interrupt_handler`，`opts.timeout-ms` 可覆盖；纯计算死循环也管                                                                                   |
| 命令流水      | 100 条     | 审计用，超出不再记录                                                                                                                                  |
| `console.log` | 1 MiB      | 超出截断并置 `truncated`                                                                                                                              |

## 重新生成

```sh
WASI_SDK=~/.cache/wasi-sdk-34.0-x86_64-linux ./generate.sh
```

依赖：`cargo-component`、`wasm32-wasip2` target、`WASI_SDK`（rquickjs 要编 quickjs 的 C 源码）。

**不需要 quickjs-ng 子模块，也不需要 vendor**：crates.io 上的 `rquickjs-sys` 0.14 **自带**那 4 个
C 文件（`quickjs.c` / `libregexp.c` / `libunicode.c` / `dtoa.c`），实测直接编得过、跑得起来。

产物：`dist/codemode.wasm`（ **796 KB**；`dist/`、`src/bindings.rs`、`target/` 都不入库）。

## 下一步

宿主侧 `crates/tower-agent-codemode`：`bindgen!` 绑定 + 实现 `exec`（起 coreutils 组件、内存 stdio、
设 `argv[0]`）+ 会话 preopen + 冒烟测试。trap 要当成「脚本失败」并 **丢弃实例**（trap 后不可复用）。
