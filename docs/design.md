# tower-agent 整体设计

| 项目     | 值                                                                                                                                                                                                  |
|----------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 状态     | 设计基线 v1.6（核心 runtime、Provider、插件、codemode、Wasmtime 沙箱与 UI 分层均已落地） |
| 能力参考 | pi-agent `a276dabe57911253350bffb93cb7d7aff6a73261`（`main`，2026-10-03） |
| 架构依赖 | tower `0.5.3`（tower-service 0.3.3 / tower-layer 0.3.3）                                                                                                                                            |
| 目标仓库 | `tower-agent`（Rust，edition 2024，nightly）                                                                                                                                                        |

本文档回答一个问题：**在构建 tower-native agent 时，`Service` / `Layer`、流、会话与插件分别承担哪些职责。**

pi-agent 是 agent 能力设计的重要参考，但不是 `tower-agent` 的兼容性目标。文中的“pi 概念映射”用于对照能力边界与交互语义，具体 API、模块结构和实现以 Rust / tower 架构为准。文档后部保留的实现进度记录可能使用早期“移植”措辞，不代表当前项目定位。

> 子设计文档：[`plugins.md`](./plugins.md)（插件 / 扩展体系：静态 tower Layer、stabby 动态插件、JIT 脚本插件）、[`codemode.md`](./codemode.md)（**agent 可以编程**：§0 是"mods/codemode 统一 + 解释器可选"的 v2 提案，§1–§11 是 JS 引擎细节）、[`sdk.md`](./sdk.md)（扩展点设计，落在 `tower-agent-plugin-sdk`）、[`roadmap.md`](./roadmap.md)（未完成事项的单一入口）。

---

## 1. 目标与非目标

目标：

- 用 tower 的 `Service` / `Layer` 作为架构骨架，让 provider 调用、工具调用、RPC 等"请求 → 响应"边界共享同一套中间件（超时、重试、限流、并发、背压、遥测）。
- 在关键 agent 行为上参考 pi-agent 的成熟经验，同时允许根据 tower 生态与 Rust 契约调整。
- 用 Rust 的类型系统和 trait 边界表达流式调用、错误编码、插件能力与生命周期契约。
- 通过插件、Wasmtime 沙箱与 codemode，让宿主和 LLM 能在受控边界内扩展或调整 agent。

非目标：

- 不把 `tower-agent` 做成 pi-agent 的逐项移植、兼容实现或产品行为克隆。
- 不承诺与 pi-agent 的 API、模块结构、事件名称和默认值逐项一致。
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

| pi 概念                                                                               | pi 中的形态                                               | Rust / tower 落点                                                                        | 说明与风险                                                                                                                     |
|---------------------------------------------------------------------------------------|-----------------------------------------------------------|------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------|
| `StreamFn`                                                                            | `(model, context, options) => EventStream`                | `Service<StreamRequest, Response = AssistantEventStream, Error = StreamError>`           | 流作为 `Response`。pi 契约要求"不抛错、错误编码进流"，所以 `Error` 通道只承载建立请求阶段的失败                                |
| `Provider`                                                                            | `stream` / `streamSimple` / `generateImages` / `classify` | 每个 provider = 一组 `Service`（按能力拆分），用 `BoxCloneService` 抹平具体类型          | pi 一个 provider 有多种能力，`Service` 只有单一 Request/Response，需要按能力分 trait 或分字段                                  |
| `Models` 注册表                                                                       | `setProvider` / `getModel` / `getAvailable` / `refresh`   | 路由 `Service`（按 `provider`/`api` 分发）+ 普通状态仓库（目录、凭据）                   | 路由可用 `tower::steer` 或自定义 match；目录/认证是状态，不是 layer                                                            |
| `ProviderRequestOptions`（`apiKey`/`headers`/`env`/`fetch`/`onPayload`/`onResponse`） | 每次请求的可选注入点                                      | `Layer` 栈：`map_request` / `map_response` / 自定义 Layer                                | 顺序必须对应 pi 的合并优先级（caller 覆盖默认、合并 provider 默认）                                                            |
| `timeoutMs`                                                                           | provider 请求超时                                         | `tower::timeout::Timeout`（阶段 1，覆盖到 Response future）                              | ⚠️ 覆盖不到响应流的空闲期；流超时需自建 stream-aware layer，见 D2/Q1                                                           |
| `maxRetries` / `maxRetryDelayMs`                                                      | SDK 客户端重试 + 长延迟上限                               | `tower::retry::Retry` + 自定义 `Policy`                                                  | 要复刻 pi 的语义：服务器要求的长延迟超过上限则立即失败（交给上层），而不是无限等待                                             |
| `signal: AbortSignal`                                                                 | 贯穿 provider 与 tool 的取消信号                          | Request 内携带 `tokio_util::sync::CancellationToken`；取消 = 完成/丢弃 future            | tower 无原生取消；需要保证 drop 时资源释放与 pi 的 abort 结果一致（`stopReason: "aborted"`）                                   |
| `Agent`                                                                               | 编排循环 + 队列 + `subscribe`                             | `tower::Service<AgentRequest, Response = AgentResponse>`（`BoxAgentService` 可擦除类型） | 现在是 tower 生态的一部分（可与 axum 组合，定位类似 `tower-mcp`）；`isStreaming`、队列 drain 点、listener await 仍在编排层实现 |
| agent loop 的一次模型调用                                                             | `agent-loop.ts`                                           | `Service::call`                                                                          | 一次 call = 一个 turn 的一次 LLM 请求                                                                                          |
| `AgentTool.execute`                                                                   | `(id, params, signal, onUpdate) => Promise<Result>`       | `Service<ToolCall, Response = ToolResult>`                                               | `onUpdate` 两种建模：保留回调，或把 Response 改成「部分结果 + 最终结果」的 `Stream`。见 Q3                                     |
| 工具集合与名字分发                                                                    | `tools: AgentTool[]` + 按名查找                           | 路由 `Service`（`steer` 或 `HashMap<String, BoxCloneService<…>>`）                       | 未知工具名必须产出 `isError: true` 的结果，与 pi 行为一致                                                                      |
| `toolExecution: "parallel"`                                                           | 同一条 assistant message 的多个 tool call                 | `join_all` / `CallAllUnordered` / `JoinSet`                                              | ⚠️ pi 默认 parallel 且未声明上限；加 `ConcurrencyLimit` 会改变并发可观察行为，默认值必须与 pi 一致（Q2）                       |
| `beforeToolCall` / `afterToolCall`                                                    | 可修改参数/结果的钩子                                     | `Layer`（`filter` / `and_then` / 自定义）                                                | 钩子能改写请求与结果，正是 layer 的形状                                                                                        |
| `convertToLlm` / `transformContext`                                                   | 消息变换（纯函数，不得抛错）                              | 普通函数，在编排层调用                                                                   | 不建模为 Service，避免污染 tower 语义                                                                                          |
| `Agent.subscribe` 事件                                                                | 多订阅者 push                                             | `tokio::sync::broadcast`/`mpsc` + `Stream`                                               | 与 tower 无关；背压策略需与 pi 的"await 所有 listener"对齐（Q6）                                                               |
| `steer` / `followUp` 队列                                                             | 输入消息队列                                              | 编排层内部队列（可用 `tower::buffer` 串行化注入）                                        | ⚠️ 名字与 `tower::steer::Steer`（请求路由）无关，不要混淆                                                                      |
| RPC（`protocol`）                                                                     | request/cancel → response，另有 service_update 订阅       | `Service<RpcRequest, RpcResponse>` + 订阅 `Stream`                                       | 要按 target（server / session+attachment）路由                                                                                 |
| 会话 / 持久化（`durable`、session-manager）                                           | 有状态存储                                                | 仓储 `Service`；写入路径可用 `tower::buffer` 做单写者串行化                              | 状态本身不是中间件                                                                                                             |
| 遥测（`pi-telemetry`）                                                                | 请求级遥测上下文                                          | `tracing` + 中间件；`telemetryContext` 进 Request                                        | tower 各中间件自带 tracing 语义                                                                                                |
| 多端点 / 动态 provider（cache-warmer、remote catalog）                                | 端点池与健康                                              | `discover` + `load` + `ready-cache` + `balance::p2c`                                     | 阶段 2；当前 feature 未启用                                                                                                    |
| TUI 差分渲染                                                                          | `tui` 包                                                  | 独立 crate，订阅事件流                                                                   | 不使用 tower                                                                                                                   |

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
ui/         // 前端无关 UI 会话后端（事件归约 / 命令 / 可序列化视图）
tui/        // 终端渲染（对应 pi packages/tui，不使用 tower）
tower-agent-cli/        // CLI 入口；bash/read/write/edit 等内置工具与扩展（对应 coding-agent）
```

阶段 1 落成 `protocol` / `ai` / `agent` / `tui` / `tower-agent-cli` 五个 crate，拆分依据是上面的模块边界而不是文件数量；公共 `PiError` / `BoxError` / Request/Response 类型放在 `ai`，避免额外引入第 6 个 crate。后续如需可再细分，但不改变该依赖提取策略。 **`tower-agent-cli` 是唯一的可执行入口**（`crates/tower-agent-cli/src/main.rs`），其余四个 crate 都是纯库。

当前进度：workspace 骨架已建立，`ai`（第一个移植单元 + 真实 provider）、`agent`（第 4.i 步）、`tower-agent-cli`（第 4.ii 步内置工具 + 第 4.iv 步会话入口）、`tui`（第 4.iii 步终端 UI）与 `protocol`（帧 / CBOR / RPC 信封）五个 crate 全部落地。

本轮把 tower 面扩展为：

- `tower_agent_runtime::Agent` 实现 `Service<AgentRequest, Response = AgentResponse>`，并提供 `BoxAgentService` / `AgentServiceExt`，可与 axum 等 tower 生态组合（定位类似 `tower-mcp`）。
- `tower_agent_runtime::layer` 提供 `BeforeToolCallLayer` / `AfterToolResultLayer`（pi 的 `tool_call` / `tool_result` 钩子即 tower `Layer`）；工具服务可与 `tower::timeout` / `tower::limit` 等生态 layer 混用（会把错误擦成 `BoxError` 的 layer 用 `map_err(AgentError::other)` 转回）。
- `tower_agent_runtime::ToolRegistration` 提供 schemars + serde 的 typed tool 注册：参数 schema 自动生成，入参自动反序列化，校验失败编码成 `is_error` 工具结果。
- `tower_agent_runtime::Compactor` 提供**核心的上下文压缩**（pi `core/compaction`）：宿主构造一个 `Compactor` 放进 `AgentOptions`，agent 循环在每次模型请求前按阈值（`context_window - reserve_tokens`）自动压缩；`compact()` / `request_compaction()` 供异步 / 同步宿主手动触发。摘要走同一个 provider 流服务，消息按 pi 的 `serializeConversation` 形状序列化，切点只落在 user / assistant 上（绝不切 tool result），压缩后模型看到的是 `system + 摘要 + 保留的消息`——**原始 transcript 不改写**，与终端无关，CLI / axum 共用。
  - **溢出 / 长度恢复**：provider 报上下文溢出（建立请求失败，或流内 `error` 文案命中溢出关键词），或响应被长度截断且没有工具调用时，会**压缩一次再重试当前请求**（每个请求只试一次）。终态事件（`MessageEnd`）由外层在确定不重试后才发出，因此被丢弃的那次尝试不会进 transcript、也不会渲染成条目。
  - **压缩钩子**：`CompactionHook`（`before` / `after` / `failed`）对应 pi 的 `session_before_compact` / `session_compact` / `session_compact_failed`；`before` 可取消压缩或用自定义摘要跳过内置摘要调用，`after` / `failed` 供遥测。`CompactionReason` 区分 `Threshold` / `Manual` / `Overflow` / `Length`。
  - **可持久化 / 可恢复**：`Compaction` 是 serde 可序列化的（`summary` / `first_kept` / `tokens_before`），`Compactor::restore` 可把已持久化的压缩装回去，续接会话时不必重新摘要。
- `tower_agent_ui::AgentUi` 是前端无关的 UI 会话后端（事件归约、斜杠命令、submit/handled/quit 动作），GUI 与 axum 可直接消费；`tower_agent_tui::UiPlugin` 只保留终端渲染扩展点（状态栏片段 / overlay / 工具结果定制渲染）。
- tower-agent-cli 侧：`cli` crate 重命名为 `tower-agent-cli`，其余 crate 名称与职责不变。

Provider 协议层（`tower-agent-provider`）与具体实现层（`tower-agent-provider-builtin`）：

- `http`：`HttpTransport` 接缝（真实实现是 `ReqwestTransport`，Q5 选型 reqwest + rustls）+ 请求/响应类型（响应体是分块字节流）。provider 只依赖这个 trait，因此测试可以用脚本化传输层覆盖，无网络。
- `sse`：增量 SSE 解析（`event:` / 多行 `data:` / 注释 / CRLF，任意切分）。
- `tower_agent_provider_builtin::provider::anthropic`：Anthropic Messages provider —— 请求构造（system / messages / tools / thinking / temperature / metadata）、`text` / `thinking` / `redacted_thinking` / `tool_use` 四种内容块的流式映射、usage 与成本（含计价档位）、`stop_reason` 映射（`end_turn`→stop、`max_tokens`→length、`tool_use`→toolUse、`refusal`/未知→error），错误按契约区分：非重试的 4xx 与流内 `error` 编码进流，429/5xx/传输错误走 `Service::Error` 交给 retry 层。
- `tower_agent_provider_builtin::provider::openai`：OpenAI Chat Completions provider —— 一条实现覆盖大量兼容端点（OpenAI / Azure OpenAI / OpenRouter / Cerebras / Groq / DeepSeek …）。请求构造（system / tools / temperature / `stream_options.include_usage` / `user`）、`content` / `reasoning_content` / `tool_calls` 的流式映射（含分片 partial JSON）、`finish_reason` 映射、`data: [DONE]` 收尾；错误归属与 Anthropic 一致。
- `tower_agent_provider_builtin::provider::google`：Google Generative AI provider —— 用来验证抽象能容纳 **形状完全不同**的 API：URL 把模型放进路径（`:streamGenerateContent?alt=sse`）、认证用 `x-goog-api-key`、请求体是 `contents`+`parts` 而不是 `messages`（system 走 `systemInstruction`、工具走 `tools[].functionDeclarations`）、`functionCall` 是整块对象（因此一次发 start/delta/end）、思考用 `thought: true` 的 part、用量在 `usageMetadata`（input 扣掉 cached、output 加上 thoughts）。另外按 pi 的行为在有工具调用时把 `STOP` 纠正成 `toolUse`。
- `tower_agent_provider_builtin::provider::openai_responses`：OpenAI Responses provider —— 与 chat completions 又是另一套协议：请求体是类型化 `input` item 列表（`function_call` / `function_call_output` 与消息平级、`store: false`、`max_output_tokens` 下限 16），流是 **事件类型化**的 SSE（`response.output_text.delta`、`response.reasoning_summary_text.delta`、`response.function_call_arguments.delta|done`、`response.output_item.done`、`response.completed|incomplete|failed`），需要按 `output_index` 把 deltas 归并到对应输出项，并用终态 item 修正文本/参数。
- `provider::layers`：`HeaderTransport`（auth：注入/覆盖请求头，大小写不敏感）、`RetryTransport`（对 429/5xx 与可重试传输错误重试；`Retry-After` 超过 `maxRetryDelayMs` 时立即返回，落实第 7 节第 7 条）、`ObserveTransport`（`onPayload` 可观测/替换载荷，`onResponse` 观测状态码与响应头；[`CallContext`] 携带 provider/api/model 但不上线），以及 `TimeoutService`（`timeoutMs`：只约束 `call` 返回的「建立请求」future，超时映射成 `PiError::Timeout`）。
- `models::Models`：三块内容 —— `(provider, api) → provider Service` 的路由 Service、`(provider, model id) → Model` 的模型目录，以及 `refresh`：逐个 provider 调用注册的目录来源，成功替换该 provider 的目录并统计 added/updated/removed，失败保留旧条目并把错误记进报告（对应 pi `Models.refresh()` 返回 `{ aborted, errors }` 的语义）。选项支持 `allow_network` / `providers`（限定范围）/ `signal`（取消）。未注册路由返回 `PiError::Unsupported`。
- `tower_agent_provider_builtin::provider::faux`：确定性离线 provider， **仅供测试**——挂在 provider-builtin 的 `test-util` feature（或 `cfg(test)`）下，正常构建不编入任何二进制。

尚未移植的 provider 相关部分：bedrock-converse / vertex / mistral-conversations / pi-messages / cloudflare 等（其中 bedrock 还需要 SigV4 与事件流签名）。CLI 已接上真实 provider：`--provider`（`anthropic` / `openai` / `openai-responses` / `google` / `deepseek`）+ `--model` 组装 `timeout → retry → provider` 的栈（`deepseek` 复用 OpenAI 兼容的 `/chat/completions`，base URL 指向 `api.deepseek.com`），凭据来自各家的环境变量（`ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `GEMINI_API_KEY` / `DEEPSEEK_API_KEY`）。TUI 下缺凭据会直接进入界面并用
`/login <provider> <api-key>` 配置；print 模式缺凭据则明确报错。

`protocol` 已落地的内容（`crates/tower-agent-protocol`）：

- `framing`：4 字节大端长度前缀的帧编解码，增量式（任意切分/粘包）、支持零长度帧，带单帧上限与截断检测；失败或结束后再次使用会报错。
- `cbor`：pi 的 **严格 CBOR 子集**自研实现（不使用通用 CBOR 库）：definite-length only、无 tag / indefinite-length / break、map 键必须是字符串且不得重复、整数与长度受 JS 安全整数范围约束；字节数 / 容器长度 / 嵌套深度上限的默认值与 pi 一致。`CborValue ↔ serde_json::Value` 互转用于协议载荷。
- `protocol` / `codec`：`hello` / `request` / `cancel` / `response` / `service_update` / `attachment` 信封的严格校验（对象多字段即非法、`serverId` 必须是规范 UUID v4、`call` / `result` / `update` 必须是不透明 JSON）+ 带帧编解码与增量解码器。`PROTOCOL_VERSION = 8`；`ClientHandshake` 落实「客户端第一条帧必须是 hello」。

`agent` 已落地的内容：工具元数据 + 工具 Service（`AgentTool`、`ToolRouter`）、`AgentEvent` / `AgentMessage` 事件类型、agent 编排循环（`agent_loop::run_agent_loop` / `run_agent_loop_continue`）、顺序与并行工具执行、`beforeToolCall` / `afterToolCall` / `finishTurn` 钩子、steer / follow-up 队列，以及带逐订阅者 mpsc 背压的有状态 `Agent`。

本轮相对设计的一处实现说明：`beforeToolCall` / `afterToolCall` 目前是 loop config 上的回调（与 pi 形状一致），而不是包裹单个工具 Service 的 tower Layer——它们需要 assistant 消息与 agent 上下文，且 `block` 要产出 `isError: true` 的工具结果而非 Service 错误。围绕工具 Service 的通用中间件（超时、限流、遥测）仍按第 4.3 节以 Layer 形式叠加。

尚未移植（后续步骤）：`prepareRequest` / `prepareNextTurn` 钩子、`getApiKey`、`validateToolArguments`（离线环境缺 JSON Schema 校验器）、`onPayload` / `onResponse` 观测 Layer、`normalizeContext` 的完整实现。

`tools` crate 已落地的内置工具（`crates/tower-agent-tools`）：`read` / `shell` / `edit` / `write` / `grep` / `find` / `ls`（`shell` 内嵌 [brush](https://github.com/reubeno/brush)（`brush-core` + `brush-builtins`，bash 兼容的 Rust shell），在进程内解释执行，Windows / macOS / Linux 行为一致），以及它们共享的 `truncate`（2000 行 / 50KB 双重限制）、`path_utils`（`~` 展开、Unicode 空格、macOS 变体）、`file_mutation_queue`（同文件写操作串行化）与 `edit_diff`（精确 + 模糊匹配、重叠检测、unified patch）。七个工具各自导出独立静态插件，注册表登记与解析由 `tower-agent::assembly` 完成。

与 pi 的实现差异（都是"进程内实现替代外部二进制"或环境能力所限）：

- `read` 不做图片自动缩放（离线缺图像处理能力），图片按 MIME 嗅探后原样作为附件。
- `shell` 不注入 `PI_*` 会话环境变量（`ToolCallRequest` 不携带会话上下文），输出累加器是 pi 有界版本的简化版。同一个工具复用一条持久 shell 会话：变量 / 函数 / 别名跨命令保留，但每条命令开始前把工作目录钉回 workspace，`cd` 只影响当前命令（避免 cwd 漂移、与其它锚在 workspace 的工具分歧）；超时 / 取消 / `exit` 会作废整条会话并拆掉它专属的 runtime，连带收掉 `cmd &` 留下的后台子进程。
- `find` 不调用外部 `fd`，改用 `ignore` + `globset` 在进程内实现相同语义（`*` 不跨分隔符、无 `/` 的模式按文件名匹配、`**/` 前缀规则、尊重 `.gitignore` 且含隐藏文件、非 git 仓库内也应用 `.gitignore`）。
- `grep` 不调用外部 `rg`，改用 `regex` + `ignore` 实现相同输出形状与截断/提示文案，并跳过二进制文件。

`ui` / `tui` 已落地的内容（`crates/tower-agent-ui` + `crates/tower-agent-tui`）：

- `tower_agent_ui::UiModel` 是唯一状态来源，由 `AgentEvent` 归约（`MessageUpdate` 驱动流式文本，`ToolExecutionStart/Update/End` 驱动工具条目原地更新，`TurnEnd` 记录错误）；快照可序列化为 JSON。
- `tower_agent_ui::AgentUi` 把模型、斜杠命令与宿主控制协议封装成静态插件，并输出 `UiAction::{Submit,Handled,Quit}`；GUI / axum 不依赖 ratatui 即可复用。
- `render` 是纯函数：`&UiModel` → 缓冲区，三段布局（状态栏 / transcript / 提示栏），按显示宽度折行（宽字符按 2 列计算）并从底部对齐。
- `app::Tui<B: Backend>` 把 `AgentUi`、输入编辑器与 ratatui 后端绑定；差分渲染由 `Terminal::draw` 承担（只写变化的单元格）。后端是泛型，因此生产用 termina、测试用 `TestBackend` 走同一套渲染代码。
- `backend` 模块是 **唯一**与具体后端耦合的地方：`init_terminal` / `restore_terminal` / `run_tui` / `run_agent_tui` / 全局按键。当前后端是 **termina**（`ratatui::backend::TerminaBackend`，由 manifest 的 `termina` feature 引入）；换 crossterm/termion/termwiz 只改这一个文件。
- `editor` 模块是输入编辑器： **`ratatui-textarea`**（官方 Ratatui 开发者维护的 fork，git 引入、`termina` 输入适配、`default-features = false`）+ 一层 vim 键位。 **默认 vim 普通模式**，五种模式：Normal / Insert / **Visual**（`v` 逐字符、`V` 整行；`y` `d` `c` `p` 作用在选区上）/ **Replace**（`r` 替换一次、`R` 覆盖输入）/ Operator（`d`/`y`/`c` + 动作，含 `dd`/`yy`/`dw`）。Normal 键位：`i a A I o O`、`Esc`、`h j k l`/方向键、`w e b`、`0 ^ $`、`gg G`、`x D C`、`p u Ctrl+r`、`v V r R`。 **默认显示行号**（`set_line_number_style`
  ），边框标题显示当前模式。Normal 模式 `Enter` 提交、`q` 退出；Insert 模式 `Enter` 换行。

`tui` 是 **纯库**（与 pi `packages/tui` 一致）：不含二进制，事件循环由 `backend::run_agent_tui` / `run_tui` 暴露给调用方。可运行的 TUI 入口在 `tower-agent-cli`（默认模式，见下），它把真实会话（真实 provider + 内置工具）接到同一套渲染上；`cargo run -p tower-agent-cli` 即可试用编辑器。全局按键：`Ctrl+C` 随时退出、`PageUp/PageDown` 滚动会话记录；其余按键归编辑器。

`tower-agent-cli` 已落地的会话入口（`crates/tower-agent-cli`）：

- `args`：命令行解析，用 **`clap` derive** 定义参数表， **名称与短选项对齐 pi**：`-p/--print`、`-v/--version`、`-h/--help`、`--provider`、`--model`、`--api-key`、`--thinking <level>`、`-t/--tools`、`--exclude-tools`、`--no-tools`、`--no-builtin-tools`（pi 的多字母短选项 `-nt` / `-nbt` / `-xt` 在解析前展开成对应的长选项），以及本移植版特有的 `-C/--cwd`、`--list-tools`。帮助/版本/参数错误由 clap 生成（帮助模板改成中文），`--provider` / `--model` 的取值语义仍由 `provider` 模块校验以保留中文错误；`--api-key` 走
  `build_with_api_key` 覆盖环境变量。
- `provider`：把 `--provider` / `--model` 映射到 `ai` 的真实 provider 栈——`ProviderKind`（`anthropic` / `openai` / `openai-responses` / `google`）给出 `(provider, api)`、凭据环境变量与默认模型 id；`build` 读环境变量并用 `Models` 注册路由 + 模型目录，真实 provider 用 `RetryTransport → ReqwestTransport` 加 `TimeoutService`（建立请求超时）。缺凭据返回 `MissingApiKey`，未知 provider 返回 `Unknown`（都带可读文案）。没给 `--provider` 时 `default_kind_from_env` 按 `anthropic → openai → google` 挑第一个有凭据的，都没有就报错。

- `store`：CLI 配置的持久化层，**配置一律经 `tower-agent-storage`**（默认后端 Turso，`~/.local/share/tower-agent/storage.db`，`TOWER_AGENT_STORAGE` 可覆盖路径）——provider 凭据存进 `ProviderStore`（对应 pi `~/.pi/agent/auth.json`），`defaultProvider` / `defaultModel` / `defaultThinkingLevel` 存进 `ConfigStore`（对应 pi `~/.pi/agent/settings.json`）。`ConfigStore` 按**作用域**分区（`ConfigScope::User` / `ConfigScope::Project(<项目 id>)`，项目级覆盖用户级，`resolve_config` / `effective_config` 是唯一实现）；作用是 storage 接口的职责，不是某个终端或某个后端的私事，axum / webui 走同一套接口。取值优先级对齐 pi：命令行（`--provider` / `--api-key` / `--model` / `--thinking`）> 环境变量 > 存储；启动时读出快照，运行期由 `/login` `/model` `/thinking` 写回**用户级**（fire-and-forget，退出前 `flush` 落盘）。查看 / 修改配置由 `--config-get` / `--config-set` / `--config-unset` / `--config-list` + `--config-scope <user|project>` 提供（写指令默认 user 作用域）。存储打不开时只警告，CLI 退回纯环境变量模式。
- `session::Session`：持有 `tower_agent_runtime::Agent` + 七个内置工具 + 系统提示，暴露 `subscribe` / `ask` / `continue_run` / `abort` / `state`，以及 `set_model` / `set_thinking_level` / `set_tools` / `reset`（CLI 选项与 TUI 的 `/model` `/thinking` `/new` 共用）。它实现 `tower_agent_ui::CommandHost`，把会话能力交给前端无关的命令层。
- **transcript 持久化**：会话按**项目（工作目录）**归属，元数据写进 `SessionStore` 的 `session.metadata.cwd`、消息逐条写进 `HistoryStore`（都在同一份 storage 里）。`store::Transcript` 在每轮结束时把「还没落盘的尾巴」追加进去（`sync` 幂等，成功 / 取消 / 失败都落盘），标题取第一条用户消息。CLI 参数对齐 pi：默认**自动保存**新会话，`--continue` 续接当前目录最近一次会话，`--session <id>` 续接指定会话，`--no-session` 关闭保存；`/new` 换一个新会话 id。续接时把落盘的消息作为 `AgentInitialState.messages` 重新喂回运行时的 agent 循环。
- `session::run_print` / `run_tui`：同一条事件流的两种渲染方式（纯文本 / TUI）。TUI 模式下界面在事件循环里跑，用户在输入区提交的文本经 mpsc 交给驱动任务：`Session::submit` 空闲时开新一轮、 **流式期间则走 `steer`**（不会撞上 "already processing"）；用户退出时取消进行中的一轮。以 `/` 开头的输入不进 agent，而是交给 `tui` 的斜杠命令层。
- `faux`：确定性的离线 provider， **测试替身**——实现放在 `tower_agent_provider_builtin::provider::faux`（provider-builtin 的 `test-util` feature，`tower-agent-cli` 以 dev-dependency 引入），发布的二进制不含它；不联网也能端到端跑通会话。
- 二进制 `tower-agent-cli`： **不带参数且 stdin/stdout 都是终端时直接进 TUI**（提示词可选，TUI 里用 `/` 命令驱动）；`--print` 或任一路被重定向时走一次性纯文本模式（提示词来自位置参数或 stdin），`--list-tools` 打印八个工具后退屏；`--provider` / `--model` / `--thinking` / `-t` 系列选择 provider、模型与工具。这与 pi「终端交互默认进 TUI、重定向默认 print」一致。
- 斜杠命令：`tower-agent-ui` 的表逐条对齐 pi `BUILTIN_SLASH_COMMANDS`（24 条，名称与说明一致），TUI 编辑器里 `/` 打开菜单（前缀优先过滤、`Tab` 补全、`↑/↓` 选择、`Enter` 执行、`Esc` 关闭）。已实现 `/model`、`/thinking`、`/session`（含会话 id）、`/hotkeys`、`/new`（开新会话记录）、`/quit`、`/login`（凭据与默认项写回 `store`）、`/compact`（请求压缩，排到下一个模型请求前）；其余命令（`/export`、`/resume`…）依赖尚未移植的导出 / 会话选择器能力，执行时给出明确的“尚未移植”提示。
- **上下文压缩**：核心实现（见上，`tower_agent_runtime::Compactor`）由 agent 循环自动执行；CLI 只是从 storage 读 `compaction.enabled` / `compaction.reserveTokens` / `compaction.keepRecentTokens`（user ⊕ project 有效值）构造压缩器。压缩状态随会话落进 `session.metadata.compaction`，`--continue` / `--session` 续接时用 `Compactor::restore` 装回去，不会重新摘要。原始 transcript 不改写，导出 / 续接仍能看到完整历史。
- 尚未移植：扩展（extensions/MCP）。TUI 模式下输入区即 vim 编辑器，提交的文本经 `Session::submit` 进入 agent。
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

- pi 侧有 faux provider（`packages/ai/src/providers/faux.ts`）与 `coding-agent/test/suite/tower-agent-cli.ts`，可用来生成确定的 provider 流事件序列做金标准。
- Rust 侧已有对应骨架：`crates/tower-agent-provider/tests/support`（确定性 faux provider，对照 pi faux 的 `start → *_start/*_delta/*_end → done` 与 setup-failure 单 `error`）与 `crates/tower-agent-provider/tests/conformance.rs`（固定金标准序列 + 事件协议不变量）。
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
2. ~~落地第一个移植单元：`tower_agent_provider::StreamFn → Service`，先写类型与 trait 边界（含 `PiError`、`StreamRequest`、`AssistantEventStream`）。~~ 已完成，落在 `crates/tower-agent-provider`：`PiError`/`BoxError`、`StreamRequest`/`StreamOptions`/`SimpleStreamOptions`/`TranscriptContext`、`AssistantEventStream`（`futures_core::Stream`）、`AssistantMessageEvent`，以及 `StreamService`/`BoxStreamService`/`StreamFn` 边界与 `boxed_stream` 装箱助手。
3. ~~建 conformance 测试骨架：用 faux provider 固定一组事件序列作为金标准。~~ 已完成，落在 `crates/tower-agent-provider/tests/conformance.rs` + `crates/tower-agent-provider/tests/support`：覆盖文本流精确金标准、thinking+text 与 tool call 的结构金标准、setup failure、pre-abort，以及共享的事件协议不变量与 `StopReason` wire 字符串。
4. 再按 `agent` → `tools` → `tui` → `tower-agent-cli` 的顺序推进，每步单独跑通 `cargo fmt` / `clippy` / `test`。
    1. ~~`agent`：编排循环、事件流、工具执行（顺序/并行 + 钩子）、steer / follow-up 队列。~~ 已完成，落在 `crates/tower-agent-runtime`（`agent_loop` / `agent` / `event_bus` / `tool` / `router` / `event`），共 30 个测试，覆盖第 7 节清单的第 4、5、6 条：并行 `tool_execution_end` 完成顺序 vs 工具结果消息源顺序、`terminate` 批次提前结束、steer 队列 `one-at-a-time` 与 `all` 的 drain 差异、只有被 await 的 listener 结算完 agent 才 idle。`cargo fmt` / `clippy --all --workspace --all-targets` / `test --all --workspace` 全绿。
    2. ~~`tools`：内置工具（bash/read/write/edit…，对应 pi `coding-agent`）。~~ 已完成，落在 `crates/tower-agent-tools`（`truncate` / `path_utils` / `queue` / `edit_diff` / `shell` / `read` / `write` / `edit` / `ls` / `find` / `grep`；`shell` 内嵌 brush（`brush-core` + `brush-builtins`）在进程内解释 bash 兼容语法，不再依赖外部 shell 可执行文件），共 36 个单元测试与 15 个端到端测试（其中一条把内置工具注册进 `Agent` 跑通完整 agent 循环）。`cargo fmt` / `clippy --all --workspace --all-targets` / `test --all --workspace` 全绿（90 个测试）。
    3. ~~`ui` / `tui`：前端无关 UI 后端 + 差分终端渲染。~~ 已完成：`tower-agent-ui` 负责 `AgentEvent` 归约、斜杠命令、可序列化 `UiModel` 与静态插件；`tower-agent-tui` 负责 `ratatui` 0.30 + **termina** 页面、`ratatui-textarea`（git 引入）+ vim 键位编辑器、`Tui<B: Backend>` 宿主与 `run_tui` / `run_agent_tui` 事件循环。 **两份 crate 都是纯库、无二进制**；可运行的 TUI 入口在 `tower-agent-cli`。测试：UI 16 个单元测试；TUI 39 个 lib 单元 + 9 个
       `TestBackend`
       端到端渲染测试。后端说明：ratatui 0.30 把后端拆成独立 crate，termina 由 `ratatui-termina` 提供（manifest 的 `termina` feature），切换点集中在 `backend.rs`。
    4. `tower-agent-cli`：`tower-agent-cli` crate 的会话入口与扩展。
        - 会话入口已完成： ~~`args` / `session` / `provider` / `faux` / 二进制入口~~ 落在 `crates/tower-agent-cli`（`args` / `session` / `provider` + `main.rs`）。会话把 agent、七个内置工具与事件渲染拼在一起：`--print` 打纯文本事件，默认走 TUI（`tui` 的 vim 编辑器 + 事件渲染），无位置参数时从 stdin 读提示词。
        - 真实 provider 已接上： ~~`provider` 模块~~ 用 `--provider`（`anthropic` / `openai` / `openai-responses` / `google` / `deepseek`）+ `--model` 组装 `timeout → retry → provider` 栈，凭据来自 `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `GEMINI_API_KEY` / `DEEPSEEK_API_KEY`，并注册进 `Models` 路由与模型目录；缺凭据或未知 provider 都给出可读错误。没给 `--provider` 时按 `anthropic → openai → google → deepseek` 挑第一个有凭据的；TUI 下一个都没有就先进界面，用 `/login` 配置（print 模式仍报错）。
        - 参数解析改用 `clap` derive：`args` 只声明参数表，帮助/版本/错误由 clap 生成（帮助模板中文化），`--provider` / `--model` 的语义仍由 `provider` 模块校验。`tower-agent-cli` 也是唯一的可执行入口（`tui` 已是纯库）。
        - CLI 参数与 TUI 斜杠命令对齐 pi：`-p/--print`、`-v/--version`、`--thinking`、`-t/--tools`、`--exclude-tools`、`-nt` / `-nbt`（展开成 `--no-tools` / `--no-builtin-tools`）；不带参数且终端交互时直接进 TUI，重定向时回落到 print 模式。TUI 里 `/` 打开与 pi 同名的 24 条内置命令菜单，已实现 `/model` `/thinking` `/session` `/hotkeys` `/login` `/new` `/quit`，其余明确标注“尚未移植”。
        - `tower-agent-cli` 收敛为 **二进制 crate**：crate 根是 `crates/tower-agent-cli/src/main.rs`，`args` / `session` / `provider` 是它下面的模块（不再有 `lib.rs`，`faux` 也不在这里）；provider 组装与会话入口的测试改成模块内的 `#[cfg(test)] mod tests`。
        - `faux` 不再编进二进制：实现落在 `tower_agent_provider_builtin::provider::faux`（provider-builtin 的 `test-util` feature），`tower-agent-cli` 只在 `[dev-dependencies]` 里带 feature 引入。
        - 测试：`args` 单元测试、`faux` 单元测试（provider-builtin 的 `test-util` 下）、Provider 协议单元测试、二进制测试和会话端到端测试。
        - 待办：见 [`roadmap.md`](./roadmap.md)（扩展 / MCP、会话树与分支摘要、codemode 与 SDK 统一、更多 provider 等）。
