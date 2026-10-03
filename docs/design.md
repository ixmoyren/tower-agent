# tower-pi-agent 整体设计

| 项目     | 值                                                                  |
|----------|---------------------------------------------------------------------|
| 状态     | 草案 v1.0（开放问题已确认；第 9 节第 2、3 步与第 4 步的 `agent`、`tools`、`tui`、`cli` 四个阶段已落地） |
| 参考基线 | pi `a276dabe57911253350bffb93cb7d7aff6a73261`（`main`，2026-10-03） |
| 架构依赖 | tower `0.5.3`（tower-service 0.3.3 / tower-layer 0.3.3）            |
| 目标仓库 | `tower-pi-agent`（Rust，edition 2024，nightly）                     |

本文档回答一个问题： **把 pi 移植成 Rust 时，tower 的 `Service` / `Layer` 模型应该承担哪些职责，pi 的每个概念落在哪里。**

行为对齐仍然以 pi 参考实现为准，工作流见 skill `rust-pi-agent-rewrite`；本文只规定结构，不改变行为。

---

## 1. 目标与非目标

目标：

- 用 tower 的 `Service` / `Layer` 作为架构骨架，让 provider 调用、工具调用、RPC 等"请求 → 响应"边界共享同一套中间件（超时、重试、限流、并发、背压、遥测）。
- 保持 pi 的可观察行为：消息类型、事件序列、流式 chunk 语义、协议帧、默认值。
- 用 Rust 的类型系统和 trait 边界表达 pi 里靠约定维持的契约（例如 `StreamFn` 不得抛错、错误必须编码进流）。

非目标：

- 不重新设计 pi 的产品行为，不新增 pi 没有的功能。
- 不追求把 *所有*概念都塞进 tower。流式事件、订阅、会话状态不是中间件，硬套只会扭曲语义。
- 不在本阶段做性能优化和 crate 拆分优化。

---

## 2. tower 模型速览

tower 的核心只有两个 trait：

```rust
// tower-service
pub trait Service<Request> {
    type Response;
    type Error;
    type Future: Future<Output = Result<Self::Response, Self::Error>>;
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>>;
    fn call(&mut self, req: Request) -> Self::Future;
}

// tower-layer
pub trait Layer<S> {
    type Service;
    fn layer(&self, inner: S) -> Self::Service;
}
```

`Service` 就是「异步函数 `Request -> Result<Response, Error>`」，`Layer` 是「把 Service 包一层」，`ServiceBuilder` 负责组合。 **先加入的 layer 先看到请求（最外层）**。

关键特点，决定了后面的映射：

1. **tower 是协议无关的中间件库**，本身不提供 I/O。HTTP 相关的 layer 在 `tower-http`，传输层要自己接 hyper/reqwest。
2. **中间件只覆盖 `call` 返回的那个 future，不覆盖 `Response` 内部的流。** `timeout`、`retry`、`concurrency_limit` 都不会约束响应流被消费的过程。pi 的 LLM 调用恰恰是"短请求 + 长流"，这是本次移植最主要的结构性矛盾。
3. **没有 request-scoped 取消概念。** tower 里取消 = drop future；要传递 `AbortSignal` 必须把 `CancellationToken` 放进 Request 类型。

本仓库已启用的 tower feature（`cargo add tower --features ...` 已执行）：

`util`、`timeout`、`retry`、`limit`、`load-shed`、`steer`、`filter`、`buffer`

暂不启用，等阶段 2 出现多端点/动态 provider 需求再加：

`discover`、`load`、`ready-cache`、`make`、`balance`、`spawn-ready`；`hedge`、`reconnect` 目前看不到对应 pi 概念，不引入。

> 说明：API 以本地 crate 源码（`tower-0.5.3`）核对，与 docs.rs 同源，离线可查。

---

## 3. pi 架构速览（参考实现）

pi 是 TypeScript monorepo，与本次重写相关的是 `packages/` 下这些包：

| pi 包                                                                        | 职责                                                                                                                        | 与本次重写的关系      |
|------------------------------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------|-----------------------|
| `protocol`                                                                   | RPC 帧格式（hello/request/cancel/response/service_update）、CBOR/框架编解码，`PROTOCOL_VERSION = 8`                         | 直接对应              |
| `ai`                                                                         | 统一多 provider LLM API：`Models` 注册表、`Provider`、`StreamFn`、`AssistantMessageEventStream`、认证、模型目录、token/cost | 第一阶段重点          |
| `agent`（`pi-agent-core`）                                                   | `Agent` 编排循环、工具执行、状态与事件（`AgentEvent`）                                                                      | 第二阶段重点          |
| `tui`                                                                        | 差分渲染的终端 UI 库                                                                                                        | 需要，但与 tower 无关 |
| `coding-agent`                                                               | CLI、会话、内置工具（bash/read/write/edit…）、扩展、MCP、compaction                                                         | 后续                  |
| `durable` / `codemode` / `mcp` / `telemetry` / `chord` / `server` / `client` | 持久化、代码模式、MCP、遥测、应用组合运行时、服务端/客户端                                                                  | 按需                  |

需要精确对齐的三个核心契约：

```ts
// ai: provider 侧，注意"不得抛错，错误要编码进流"
type StreamFn = (
  model: Model<Api>,
  context: TranscriptContext,
  options?: SimpleStreamOptions,
) => AssistantMessageEventStream | Promise<AssistantMessageEventStream>;

// ai: 流事件协议
type AssistantMessageEvent =
  | { type: "start"; partial: AssistantMessage }
  | { type: "text_start" | "text_delta" | "text_end" /* … */ }
  | { type: "thinking_*" /* … */ }
  | { type: "toolcall_*"; /* … */ }
  | { type: "done"; reason: "stop" | "length" | "toolUse" | "deferred"; message: AssistantMessage }
  | { type: "error"; reason: "aborted" | "error"; error: AssistantMessage };

// agent: 工具执行
interface AgentTool {
  name: string;
  label: string;
  prepareArguments?: (args: unknown) => unknown;
  outputSchema?: TSchema;
  executionMode?: "sequential" | "parallel";
  replay?: "never" | "safe";

  execute(
    toolCallId: string,
    params: unknown,
    signal?: AbortSignal,
    onUpdate?: (partial: AgentToolResult) => void,
  ): Promise<AgentToolResult>;
}
```

---

## 4. 概念映射（核心）

### 4.1 总原则

| pi 概念的性质                                                                       | Rust 落点                      |
|-------------------------------------------------------------------------------------|--------------------------------|
| 一次「请求 → 拿到响应」的边界（provider 调用、工具调用、RPC、路由分发）             | `Service`                      |
| 横切关注点（认证注入、请求/响应观察、超时、重试、限流、并发、背压、遥测）           | `Layer`                        |
| 流式事件、订阅通知（`AssistantMessageEventStream`、`AgentEvent`、`service_update`） | `Stream`（不进通用中间件）     |
| 业务语义（agent loop、队列、会话、compaction、消息变换）                            | 普通函数/结构体 + 编排 Service |

### 4.2 映射表

| pi 概念                                                                               | pi 中的形态                                               | Rust / tower 落点                                                               | 说明与风险                                                                                               |
|---------------------------------------------------------------------------------------|-----------------------------------------------------------|---------------------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------|
| `StreamFn`                                                                            | `(model, context, options) => EventStream`                | `Service<StreamRequest, Response = AssistantEventStream, Error = StreamError>`  | 流作为 `Response`。pi 契约要求"不抛错、错误编码进流"，所以 `Error` 通道只承载建立请求阶段的失败          |
| `Provider`                                                                            | `stream` / `streamSimple` / `generateImages` / `classify` | 每个 provider = 一组 `Service`（按能力拆分），用 `BoxCloneService` 抹平具体类型 | pi 一个 provider 有多种能力，`Service` 只有单一 Request/Response，需要按能力分 trait 或分字段            |
| `Models` 注册表                                                                       | `setProvider` / `getModel` / `getAvailable` / `refresh`   | 路由 `Service`（按 `provider`/`api` 分发）+ 普通状态仓库（目录、凭据）          | 路由可用 `tower::steer` 或自定义 match；目录/认证是状态，不是 layer                                      |
| `ProviderRequestOptions`（`apiKey`/`headers`/`env`/`fetch`/`onPayload`/`onResponse`） | 每次请求的可选注入点                                      | `Layer` 栈：`map_request` / `map_response` / 自定义 Layer                       | 顺序必须对应 pi 的合并优先级（caller 覆盖默认、合并 provider 默认）                                      |
| `timeoutMs`                                                                           | provider 请求超时                                         | `tower::timeout::Timeout`（阶段 1，覆盖到 Response future）                     | ⚠️ 覆盖不到响应流的空闲期；流超时需自建 stream-aware layer，见 D2/Q1                                     |
| `maxRetries` / `maxRetryDelayMs`                                                      | SDK 客户端重试 + 长延迟上限                               | `tower::retry::Retry` + 自定义 `Policy`                                         | 要复刻 pi 的语义：服务器要求的长延迟超过上限则立即失败（交给上层），而不是无限等待                       |
| `signal: AbortSignal`                                                                 | 贯穿 provider 与 tool 的取消信号                          | Request 内携带 `tokio_util::sync::CancellationToken`；取消 = 完成/丢弃 future   | tower 无原生取消；需要保证 drop 时资源释放与 pi 的 abort 结果一致（`stopReason: "aborted"`）             |
| `Agent`                                                                               | 编排循环 + 队列 + `subscribe`                             | 顶层编排 `Service` + 事件广播                                                   | **不是 layer**。`isStreaming`、队列 drain 点、listener await 语义都要自己实现                            |
| agent loop 的一次模型调用                                                             | `agent-loop.ts`                                           | `Service::call`                                                                 | 一次 call = 一个 turn 的一次 LLM 请求                                                                    |
| `AgentTool.execute`                                                                   | `(id, params, signal, onUpdate) => Promise<Result>`       | `Service<ToolCall, Response = ToolResult>`                                      | `onUpdate` 两种建模：保留回调，或把 Response 改成「部分结果 + 最终结果」的 `Stream`。见 Q3               |
| 工具集合与名字分发                                                                    | `tools: AgentTool[]` + 按名查找                           | 路由 `Service`（`steer` 或 `HashMap<String, BoxCloneService<…>>`）              | 未知工具名必须产出 `isError: true` 的结果，与 pi 行为一致                                                |
| `toolExecution: "parallel"`                                                           | 同一条 assistant message 的多个 tool call                 | `join_all` / `CallAllUnordered` / `JoinSet`                                     | ⚠️ pi 默认 parallel 且未声明上限；加 `ConcurrencyLimit` 会改变并发可观察行为，默认值必须与 pi 一致（Q2） |
| `beforeToolCall` / `afterToolCall`                                                    | 可修改参数/结果的钩子                                     | `Layer`（`filter` / `and_then` / 自定义）                                       | 钩子能改写请求与结果，正是 layer 的形状                                                                  |
| `convertToLlm` / `transformContext`                                                   | 消息变换（纯函数，不得抛错）                              | 普通函数，在编排层调用                                                          | 不建模为 Service，避免污染 tower 语义                                                                    |
| `Agent.subscribe` 事件                                                                | 多订阅者 push                                             | `tokio::sync::broadcast`/`mpsc` + `Stream`                                      | 与 tower 无关；背压策略需与 pi 的"await 所有 listener"对齐（Q6）                                         |
| `steer` / `followUp` 队列                                                             | 输入消息队列                                              | 编排层内部队列（可用 `tower::buffer` 串行化注入）                               | ⚠️ 名字与 `tower::steer::Steer`（请求路由）无关，不要混淆                                                |
| RPC（`protocol`）                                                                     | request/cancel → response，另有 service_update 订阅       | `Service<RpcRequest, RpcResponse>` + 订阅 `Stream`                              | 要按 target（server / session+attachment）路由                                                           |
| 会话 / 持久化（`durable`、session-manager）                                           | 有状态存储                                                | 仓储 `Service`；写入路径可用 `tower::buffer` 做单写者串行化                     | 状态本身不是中间件                                                                                       |
| 遥测（`pi-telemetry`）                                                                | 请求级遥测上下文                                          | `tracing` + 中间件；`telemetryContext` 进 Request                               | tower 各中间件自带 tracing 语义                                                                          |
| 多端点 / 动态 provider（cache-warmer、remote catalog）                                | 端点池与健康                                              | `discover` + `load` + `ready-cache` + `balance::p2c`                            | 阶段 2；当前 feature 未启用                                                                              |
| TUI 差分渲染                                                                          | `tui` 包                                                  | 独立 crate，订阅事件流                                                          | 不使用 tower                                                                                             |

### 4.3 典型 layer 栈（provider 调用）

请求从外到内（先加入的 layer 先看到请求）：

```text
ModelsRouterService
  └─ buffer(容量)              // 可选：让路由服务可 Clone、提供背压
     └─ timeout(整体请求)      // 对应 timeoutMs 的"建立请求"部分
        └─ retry(Policy)       // 对应 maxRetries / maxRetryDelayMs
           └─ concurrency_limit / rate_limit
              └─ load_shed     // 服务未就绪时快速失败（对应 provider 不可用）
                 └─ auth layer       // 解析/刷新凭据、注入 header
                    └─ request/response observer layer (onPayload / onResponse)
                       └─ provider Service (返回 AssistantEventStream)
```

顺序不是随意定的：`retry` 必须在 `timeout` 内侧（否则重试的是被超时截断的整体），`load_shed` 必须在业务 Service 外侧，`auth` 必须在 provider 外侧且在观测 layer 的相对位置与 pi 一致。落地时按 pi 的实际调用链逐条核对。

---

## 5. 分层与代码落点

阶段 1 **直接采用 workspace**（见 Q4 决策）：按下面的模块边界拆 crate，各 crate 中相同的依赖统一提取到 `[workspace.dependencies]`，避免逐 crate 重复声明版本：

```text
protocol/   // 帧、CBOR 编解码、RPC envelope（对应 pi packages/protocol）
ai/         // provider Service、Models 路由、认证/重试/超时 Layer；统一错误/PiError/BoxError/公共 Request/Response（对应 pi packages/ai）
agent/      // Agent 编排、工具 Service、事件流（对应 pi packages/agent）
tui/        // 终端 UI（对应 pi packages/tui，不使用 tower）
cli/        // CLI 入口；bash/read/write/edit 等内置工具与扩展（对应 coding-agent）
```

阶段 1 落成 `protocol` / `ai` / `agent` / `tui` / `cli` 五个 crate，拆分依据是上面的模块边界而不是文件数量；公共 `PiError` / `BoxError` / Request/Response 类型放在 `ai`，避免额外引入第 6 个 crate。后续如需可再细分，但不改变该依赖提取策略。

当前进度：workspace 骨架已建立，`ai`（第一个移植单元）、`agent`（第 4.i 步）、`cli`（第 4.ii 步内置工具 + 第 4.iv 步会话入口）与 `tui`（第 4.iii 步终端 UI）已落地，`protocol` 在后续步骤中加入。

`agent` 已落地的内容：工具元数据 + 工具 Service（`AgentTool`、`ToolRouter`）、`AgentEvent` / `AgentMessage` 事件类型、agent 编排循环（`agent_loop::run_agent_loop` / `run_agent_loop_continue`）、顺序与并行工具执行、`beforeToolCall` / `afterToolCall` / `finishTurn` 钩子、steer / follow-up 队列，以及带逐订阅者 mpsc 背压的有状态 `Agent`。

本轮相对设计的一处实现说明：`beforeToolCall` / `afterToolCall` 目前是 loop config 上的回调（与 pi 形状一致），而不是包裹单个工具 Service 的 tower Layer——它们需要 assistant 消息与 agent 上下文，且 `block` 要产出 `isError: true` 的工具结果而非 Service 错误。围绕工具 Service 的通用中间件（超时、限流、遥测）仍按第 4.3 节以 Layer 形式叠加。

尚未移植（后续步骤）：`prepareRequest` / `prepareNextTurn` 钩子、`getApiKey`、`validateToolArguments`（离线环境缺 JSON Schema 校验器）、`onPayload` / `onResponse` 观测 Layer、`normalizeContext` 的完整实现。

`cli` 已落地的内置工具（`crates/cli/src/tools`）：pi `allToolNames` 全集 `read` / `bash` / `powershell` / `edit` / `write` / `grep` / `find` / `ls`，以及它们共享的 `truncate`（2000 行 / 50KB 双重限制）、`path_utils`（`~` 展开、Unicode 空格、macOS 变体）、`file_mutation_queue`（同文件写操作串行化）与 `edit_diff`（精确 + 模糊匹配、重叠检测、unified patch）。

与 pi 的实现差异（都是"进程内实现替代外部二进制"或环境能力所限）：

- `read` 不做图片自动缩放（离线缺图像处理能力），图片按 MIME 嗅探后原样作为附件。
- `bash` 不注入 `PI_*` 会话环境变量（`ToolCallRequest` 不携带会话上下文）、进程组杀死只在类 Unix 平台启用、输出累加器是 pi 有界版本的简化版。
- `find` 不调用外部 `fd`，改用 `ignore` + `globset` 在进程内实现相同语义（`*` 不跨分隔符、无 `/` 的模式按文件名匹配、`**/` 前缀规则、尊重 `.gitignore` 且含隐藏文件、非 git 仓库内也应用 `.gitignore`）。
- `grep` 不调用外部 `rg`，改用 `regex` + `ignore` 实现相同输出形状与截断/提示文案，并跳过二进制文件。
- `powershell` 与 pi 一样只在 Windows 上可用：找不到 `pwsh`/`powershell` 时每次调用返回错误结果。

`tui` 已落地的内容（`crates/tui`，基于 `ratatui` 0.30）：

- `model::TuiState` 是唯一状态来源，由 `AgentEvent` 归约（`MessageUpdate` 驱动流式文本，`ToolExecutionStart/Update/End` 驱动工具条目原地更新，`TurnEnd` 记录错误）。
- `render` 是纯函数：`&TuiState` → 缓冲区，三段布局（状态栏 / transcript / 提示栏），按显示宽度折行（宽字符按 2 列计算）并从底部对齐。
- `app::Tui<B: Backend>` 把状态、输入编辑器与 ratatui 后端绑定；差分渲染由 `Terminal::draw` 承担（只写变化的单元格）。后端是泛型，因此生产用 termina、测试用 `TestBackend` 走同一套渲染代码。
- `backend` 模块是**唯一**与具体后端耦合的地方：`init_terminal` / `restore_terminal` / `run_tui` / `run_agent_tui` / 全局按键。当前后端是 **termina**（`ratatui::backend::TerminaBackend`，由 manifest 的 `termina` feature 引入）；换 crossterm/termion/termwiz 只改这一个文件。
- `editor` 模块是输入编辑器：**`ratatui-textarea`**（官方 Ratatui 开发者维护的 fork，git 引入、`termina` 输入适配、`default-features = false`）+ 一层 vim 键位。**默认 vim 普通模式**，五种模式：Normal / Insert / **Visual**（`v` 逐字符、`V` 整行；`y` `d` `c` `p` 作用在选区上）/ **Replace**（`r` 替换一次、`R` 覆盖输入）/ Operator（`d`/`y`/`c` + 动作，含 `dd`/`yy`/`dw`）。Normal 键位：`i a A I o O`、`Esc`、`h j k l`/方向键、`w e b`、`0 ^ $`、`gg G`、`x D C`、`p u Ctrl+r`、`v V r R`。**默认显示行号**（`set_line_number_style`），边框标题显示当前模式。Normal 模式 `Enter` 提交、`q` 退出；Insert 模式 `Enter` 换行。
- 二进制入口 `crates/tui/src/main.rs`：先跑一段脚本化演示会话，之后输入区提交的内容会被回显成一轮问答，`cargo run -p tui` 不需要 provider 也能试用编辑器。全局按键：`Ctrl+C` 随时退出、`PageUp/PageDown` 滚动会话记录；其余按键归编辑器。

`cli` 已落地的会话入口（`crates/cli`）：

- `args`：命令行解析（提示词、`-C/--cwd`、`--print`、`--list-tools`、`--faux`、`--help`、`--version`、`--` 终止选项解析）；手写解析便于单测覆盖。
- `session::Session`：持有 `agent::Agent` + 八个内置工具 + 系统提示，暴露 `subscribe` / `ask` / `continue_run` / `abort` / `state`。
- `session::run_print` / `run_tui`：同一条事件流的两种渲染方式（纯文本 / TUI）。TUI 模式下界面在事件循环里跑，用户在输入区提交的文本经 mpsc 交给驱动任务：`Session::submit` 空闲时开新一轮、**流式期间则走 `steer`**（不会撞上 "already processing"）；用户退出时取消进行中的一轮。
- `faux`：**确定性的离线 provider**。真实 provider（anthropic/openai/…）属于 `ai` crate 的 provider 层，尚未移植；为了让会话入口今天就能运行与被测试，faux 按固定规则产生事件流（工具结果 → 总结文本；`!<工具> <JSON>` → 一次工具调用；其他 → 回显）。
- 二进制 `cli`：`--list-tools` 打印八个工具，`--print` 跑一轮并打纯文本事件，默认用 TUI 渲染；没有位置参数时从 stdin 读提示词。
- 尚未移植：真实 provider、交互式输入编辑器（TUI 目前是只读视图）、扩展/MCP/compaction。
- 不使用 tower：TUI 订阅事件流，不是"请求 → 响应"边界（第 4.1 节）。

---

## 6. 关键设计决策

| 编号 | 决策                                                                                                                        | 理由                                                                           | 风险                                                                                       |
|------|-----------------------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------|
| D1   | tower 只用于「请求 → 响应」边界；流式事件/订阅用 `Stream`                                                                   | tower 中间件不覆盖响应流，硬套会得到错误语义                                   | 需要为流单独实现超时/取消/背压                                                             |
| D2   | `Layer` 只承载横切关注点，不表达业务语义                                                                                    | 保持 layer 可组合、可测试                                                      | 业务逻辑必须显式编排，代码量略增                                                           |
| D3   | 事件订阅走 `tokio` 通道 + `Stream`                                                                                          | pi 是 push、多订阅者、非请求响应的模型                                         | 已决（Q6）：逐订阅者 `mpsc`，天然 await 与背压，规避 `broadcast` 的 `Lagged` 丢弃/报错语义 |
| D4   | 取消用「放进 Request 的 `CancellationToken` + drop future」对齐 `AbortSignal`                                               | tower 无原生取消                                                               | 需要统一约定，避免每个 Service 各自实现                                                    |
| D5   | 异构 provider / 工具用 `BoxCloneService`（`util` feature）擦除类型                                                          | pi 用运行时集合存放实现                                                        | 装箱与 `Send + 'static` 约束，流类型需要 `Pin<Box<dyn Stream + Send>>`                     |
| D6   | 运行时选 tokio                                                                                                              | `buffer`、`limit` 等中间件依赖 tokio                                           | 与 pi 的 Node 事件循环差异要在测试中覆盖                                                   |
| D7   | 错误模型：自定义 `PiError` 枚举实现 `std::error::Error`，必要时退回 `BoxError`                                              | `Service::Error` 要求 `Send + Sync + 'static`；要能区分 stop reason 与传输错误 | 需要与 pi 的 `StopReason` / `errorMessage` 语义一一对应                                    |
| D8   | provider 按能力拆 Service（chat / image / classifier），而非一个巨型 Service                                                | tower 的 `Request`/`Response` 是单一类型                                       | 路由层要处理"该 provider 不支持某能力"的情况                                               |
| D9   | 已启用 feature：`util,timeout,retry,limit,load-shed,steer,filter,buffer`；暂不启用 `discover/load/ready-cache/balance/make` | 只引入当前有对应 pi 概念的中间件                                               | 阶段 2 需要重新评估 feature 集                                                             |

---

## 7. 行为对齐清单（必须在测试中固定）

来自 pi 参考实现与文档，移植时逐条核对：

1. `AssistantMessageEventStream`：正常流 `start` 在前，`*_start → *_delta → *_end`，以 `done` 结束；请求建立阶段失败可直接以 `error` 结束（无 `start`）；`update`/`done` 不得早于 `start`。
2. `StopReason` 全集：`pending | stop | length | toolUse | error | aborted | deferred`，`done` 与 `error` 各自只允许子集。
3. `StreamFn` / `convertToLlm` 契约：不得抛错或返回 rejected promise；失败编码进流。
4. 并行工具执行：`tool_execution_end` 按 **完成顺序**发出；tool-result 消息按 **assistant 源顺序**发出；`terminate` 仅当批次内所有已定稿结果都为 `true` 时提前结束。
5. `steer` / `followUp` 的 `QueueMode`：`all` 与 `one-at-a-time` 的 drain 行为不同。
6. `Agent` 在所有被 await 的 listener 完成后才进入 idle；`agent_end` 是最后一个事件，但不是 idle 的判定点。
7. 重试语义：服务器要求的长延迟超过 `maxRetryDelayMs` 时立即失败，而不是等待或静默截断。
8. RPC 协议：`PROTOCOL_VERSION = 8`，第一条客户端帧必须是 `hello`；帧格式与 CBOR 编码要与 `protocol` 包一致。

对齐测试的两个现成抓手：

- pi 侧有 faux provider（`packages/ai/src/providers/faux.ts`）与 `coding-agent/test/suite/harness.ts`，可用来生成确定的 provider 流事件序列做金标准。
- Rust 侧已有对应骨架：`crates/ai/tests/support`（确定性 faux provider，对照 pi faux 的 `start → *_start/*_delta/*_end → done` 与 setup-failure 单 `error`）与 `crates/ai/tests/conformance.rs`（固定金标准序列 + 事件协议不变量）。
- 后续中间件顺序、`poll_ready` 与背压行为可用 `tower-test` 的 mock 补充验证。

---

## 8. 开放问题（已确认）

| 编号 | 问题                                                                                | 影响                                                           | 决策                                                                                                                                              |
|------|-------------------------------------------------------------------------------------|----------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------|
| Q1   | `timeoutMs` 在 pi 里到底覆盖"整个流"还是"连接/首字节/空闲"？                        | 决定是直接用 `tower::timeout` 还是必须自建 stream-aware 超时层 | 使用 `tower::timeout`：覆盖请求建立阶段（`call` 返回的 future）；响应流的空闲/整体超时按 D1 另建 stream-aware 层，不回退本决策。                  |
| Q2   | `toolExecution: "parallel"` 是否有隐含并发上限？Rust 侧默认是否也要"无上限"？       | 加 `ConcurrencyLimit` 会改变可观察行为，默认值必须与 pi 一致   | 使用 `ConcurrencyLimit`；默认上限必须与 pi 的可观察并发一致（pi 无隐含上限时默认取"不设限/足够大"），限流只作为可配置保护项。                     |
| Q3   | 工具的部分结果：保留 pi 的回调式 `onUpdate`，还是建模为 `Stream<AgentToolResult>`？ | 影响 `AgentTool` 的 Service 形状与类型擦除复杂度               | 建模为 `Stream<AgentToolResult>`：`Service<ToolCall, Response = Stream<AgentToolResult>>`，中间项承载 pi 的 `onUpdate` 部分结果，末项为终态结果。 |
| Q4   | 阶段 1 是否真的保持单 crate？何时拆 workspace？                                     | 影响目录结构与编译时间                                         | 直接使用 workspace：按第 5 节边界拆成多个 crate，各 crate 中相同的依赖统一提取到 `[workspace.dependencies]`。                                     |
| Q5   | HTTP 层选型（reqwest 还是 hyper + tower-http）与是否需要 `tower-http`？             | 影响传输层 Service 与超时/重定向/追踪实现                      | 以 `reqwest` 为主，必要时引入 `hyper`，选型参考 `tower-mcp`。                                                                                     |
| Q6   | 多订阅者事件的背压策略：`broadcast` 落后时丢弃、报错还是逐订阅者 `mpsc`？           | pi 会 await listener，语义等价性受影响                         | 使用逐订阅者 `mpsc`：每个 listener 一条通道，天然 await 与背压，规避 `broadcast` 的 `Lagged` 丢弃/报错语义。                                      |
| Q7   | `steer`/`followUp` 队列用 `tower::buffer` 串行化，还是纯编排层队列？                | 影响"队列 drain 点"的实现位置                                  | 使用 `tower::buffer` 串行化注入，队列 drain 点落在编排层，由 `buffer` 提供单写者串行化与背压。                                                    |

---

## 9. 下一步

1. ~~确认第 8 节的开放问题（至少 Q1、Q2、Q3）。~~ 已完成，结论见第 8 节"决策"列。
2. ~~落地第一个移植单元：`ai::StreamFn → Service`，先写类型与 trait 边界（含 `PiError`、`StreamRequest`、`AssistantEventStream`）。~~ 已完成，落在 `crates/ai`：`PiError`/`BoxError`、`StreamRequest`/`StreamOptions`/`SimpleStreamOptions`/`TranscriptContext`、`AssistantEventStream`（`futures_core::Stream`）、`AssistantMessageEvent`，以及 `StreamService`/`BoxStreamService`/`StreamFn` 边界与 `boxed_stream` 装箱助手。
3. ~~建 conformance 测试骨架：用 faux provider 固定一组事件序列作为金标准。~~ 已完成，落在 `crates/ai/tests/conformance.rs` + `crates/ai/tests/support`：覆盖文本流精确金标准、thinking+text 与 tool call 的结构金标准、setup failure、pre-abort，以及共享的事件协议不变量与 `StopReason` wire 字符串。
4. 再按 `agent` → `tools` → `tui` → `cli` 的顺序推进，每步单独跑通 `cargo fmt` / `clippy` / `test`。
   1. ~~`agent`：编排循环、事件流、工具执行（顺序/并行 + 钩子）、steer / follow-up 队列。~~ 已完成，落在 `crates/agent`（`agent_loop` / `agent` / `event_bus` / `tool` / `router` / `event`），共 30 个测试，覆盖第 7 节清单的第 4、5、6 条：并行 `tool_execution_end` 完成顺序 vs 工具结果消息源顺序、`terminate` 批次提前结束、steer 队列 `one-at-a-time` 与 `all` 的 drain 差异、只有被 await 的 listener 结算完 agent 才 idle。`cargo fmt` / `clippy --all --workspace --all-targets` / `test --all --workspace` 全绿。
   2. ~~`tools`：内置工具（bash/read/write/edit…，对应 pi `coding-agent`）。~~ 已完成，落在 `crates/cli/src/tools`（`truncate` / `path_utils` / `queue` / `edit_diff` / `bash` / `read` / `write` / `edit` / `ls` / `find` / `grep`，`powershell` 复用 shell 执行逻辑），共 36 个单元测试与 15 个端到端测试（其中一条把内置工具注册进 `Agent` 跑通完整 agent 循环）。`cargo fmt` / `clippy --all --workspace --all-targets` / `test --all --workspace` 全绿（90 个测试）。
   3. ~~`tui`：`tui` crate 的差分渲染，订阅 `AgentEvent` 流。~~ 已完成，落在 `crates/tui`（`model` / `render` / `app` / `editor` / `backend` + `main.rs` 二进制）：`ratatui` 0.30 + **termina** 后端渲染，`TuiState` 归约 agent 事件，**`ratatui-textarea`（git 引入）+ vim 键位的输入编辑器（默认普通模式、默认行号）**，`Tui<B: Backend>` 泛型宿主，`run_tui` / `run_agent_tui` 事件循环（提交经 mpsc 交给调用方）。测试：25 个 lib 单元（含 12 个编辑器用例）+ 2 个二进制单元 + 7 个 `TestBackend` 端到端渲染测试。后端说明：ratatui 0.30 把后端拆成独立 crate，termina 由 `ratatui-termina` 提供（manifest 的 `termina` feature），切换点集中在 `backend.rs`。
   4. `cli`：`cli` crate 的会话入口与扩展。
      - 会话入口已完成：~~`args` / `session` / `faux` / 二进制入口~~ 落在 `crates/cli`（`args` / `session` / `faux` + `main.rs`）。会话把 agent、八个内置工具与事件渲染拼在一起：`--print` 打纯文本事件，默认走 TUI（只读视图），无位置参数时从 stdin 读提示词。真实 provider 属于 `ai` crate 的 provider 层（尚未移植），当前用 `--faux` 的确定性离线 provider 端到端跑通会话。
      - 测试：7 个 `args` 单元测试、3 个 `faux` 单元测试、3 个二进制单元测试、5 个会话端到端测试（含"用户 → 工具调用 → 真实 `read` 工具执行 → 工具结果 → 收尾回复"的完整链路与纯文本渲染断言）、6 个**真正运行 `cli` 二进制**的端到端测试（`--help` / `--version` / `--list-tools` / `--print` / stdin 提示词 / 错误参数）。
      - 待办：扩展（extensions/MCP/compaction）与真实 provider。
