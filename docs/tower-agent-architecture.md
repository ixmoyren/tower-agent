# tower-agent 架构与演进

> 状态：当前架构与后续演进方向。
> 目标：`tower-agent` 是独立的 tower-native 通用 agent，定位类似 `tower-mcp` / `tower-llm`，可被 axum / tonic / cli / web 等宿主集成。pi-agent 只作为 agent 能力模型的参考，不是兼容性目标。
> 关联：[`architecture-review.md`](./architecture-review.md)（现状评审）、[`design.md`](./design.md)、[`plugins.md`](./plugins.md)。

## 1. 已确认决策（本轮）

1. **TUI 做成插件**（宿主入口将来是 axum / tonic / cli / web；本轮不实现这些入口）。
2. **热重载运行时使用 monty**：monty 解释器编译为 wasm 组件，由 Wasmtime 在进程内沙箱执行；资源超限和 trap 被隔离为脚本失败。
3. **本仓库重构改名为 tower-agent**。
4. **分离出 core 箱子**（`tower-agent-core`），放置 agent 核心能力。
5. **提供无状态/池化状态**（先给 core/runtime 用；axum 集成延后）。
6. **cli 以 Service / Layer 组合方式集成**。
7. **静态插件应当是 layer**。
8. **以 tower-native 通用 agent 为主体**；pi-agent 只作为能力设计参考，保留对关键 agent 行为的回归测试。
9. **所有 crate 都加 `tower-agent` 前缀**（可能发布 crates.io，全前缀是较好实践）。
10. **本轮范围**：core / runtime / stateless（无状态 + 池化）/ cli； **mcp / tonic / web / axum 先不做**。
11. **响应形态两种都要**：非流式 `AgentResponse` 与流式 `AgentEventStream` 都提供（非流式 = 流式的 fold）。

## 2. 当前 crate 布局

```text
tower-agent/                 （facade：re-export core + runtime + provider + tools + plugin）
  tower-agent-core/          核心能力：Request/Response、消息/事件、tool 边界、Layer 扩展点、错误
  tower-agent-runtime/       有状态运行时：编排循环、队列、事件总线；无状态/池化 Service 适配
  tower-agent-provider/      Provider 协议：消息/模型/请求/Service/Models/HTTP/SSE
  tower-agent-plugin-providers/ 具体 provider 实现与静态插件声明
  tower-agent-tools/         六个静态内置工具 + tower-agent-plugin-shell（动态 shell）
  tower-agent-protocol/      帧 / CBOR / RPC（按需保留）
  tower-agent-plugin/        通用插件宿主：注册表 / 生命周期 / HostApi / 权限 / 三类加载器 / extension slot
  tower-agent-plugin-sdk/    插件能力 SDK：静态宏 / 动态 ABI / Host 能力 / 可选 Provider 能力
  tower-agent-codemode-core/ 引擎无关的脚本能力、Hook 与 Prompt 契约
  tower-agent-codemode/      JavaScript wasm 宿主与命令层
  tower-agent-monty/         monty/Python 沙箱与 JIT 热重载后端
  tower-agent-wasmtime/      通用 Wasmtime 组件、WASI、资源限制与实例池
  tower-agent-plugin-sdk/    插件 SDK；storage feature 提供配置、凭据、会话、历史、记忆与脚本契约
  tower-agent-plugin-turso/  默认嵌入式存储插件
  tower-agent-ui/            前端无关 UI 会话插件（事件归约 / 命令 / 可序列化视图）
  tower-agent-tui/           终端前端（实现 UiPlugin）
  tower-agent-cli/           CLI 宿主（Service/Layer 组合）
  tower-agent-axum/          （新，可选）axum 集成：handler / extractor / SSE
  tower-agent-tonic/         （新，可选）tonic 集成：gRPC 服务
```

- `tower-agent` 是用户入口 facade；具体领域实现仍放在独立 crate。
- `tower-agent-core` 必须 **不依赖** runtime/plugin/frontends，只放契约与类型。
- `tower-agent-plugin` 不依赖 Provider 领域；Provider 注册与动态适配由 `tower-agent-plugin-sdk` 提供。

## 3. 分层与 tower 映射

| 概念                | tower 落点                                                | 说明                                                                              |
|---------------------|-----------------------------------------------------------|-----------------------------------------------------------------------------------|
| Agent（每请求）     | `Service<AgentRequest, Response = AgentResponse>`         | 面向 axum/tonic/测试的**无状态**形态                                              |
| Agent（流式）       | `Service<AgentRequest, Response = AgentEventStream>`      | 面向 SSE / TUI 的流式形态（`BoxStream<AgentEvent>`）                              |
| Agent（有状态会话） | `Agent` + `AgentService` 句柄                             | cli/tui 会话保持 transcript/队列/事件                                             |
| Provider 调用       | `Service<StreamRequest, Response = AssistantEventStream>` | 现 `ai`                                                                           |
| 工具执行            | `Service<ToolCallRequest, Response = ToolResultStream>`   | 每个工具同时是 `Layer`                                                            |
| 生命周期钩子        | `Layer`                                                   | `BeforeToolCallLayer` / `AfterToolResultLayer`，后续补 `BeforeCompletionLayer` 等 |
| 静态插件            | 一组 `Layer`（+ 工具/能力）                               | §5.A                                                                              |
| 动态插件            | stabby `cdylib`                                           | 已实现，迁到新 crate                                                              |
| 热重载插件          | monty 脚本                                                | §5.B                                                                              |
| 事件流              | 不进通用 tower 中间件                                     | 保留「事件是 push、多订阅、带背压」的决策                                         |

## 4. 关键设计

### 4.1 core 箱子（`tower-agent-core`）

放置 **契约**，不放编排实现：

- `AgentRequest` / `AgentResponse` / `AgentEventStream`；
- 消息/事件类型（`AgentMessage`、`AgentEvent`，从现 `agent` 抽出，保持与 pi 形状对齐）；
- 工具边界（`AgentTool`、`ToolCallRequest`、`ToolResultStream`、`AgentToolResult`、`AgentToolService`）；
- Layer 扩展点（`BeforeToolCallLayer` / `AfterToolResultLayer`，以及新的 agent 级 Layer 契约）；
- 错误（`AgentError` / `BoxError`）与取消（`CancellationToken`）约定；
- typed 工具注册（`ToolRegistration`）。

编排循环、队列、事件总线、`Agent` 有状态运行时移到 `tower-agent-runtime`。

### 4.2 无状态 / 池化（给 axum）

- **无状态 Service**：`AgentService` 每次 `call` 只依赖请求里的完整 `messages`，不保存 transcript；`poll_ready` 恒 Ready，无「忙」状态。
- **池化 Service**：`AgentPool`（N 个 runtime worker）+ `tower::buffer` / `ConcurrencyLimit`；`poll_ready` 负责取用一个空闲 worker，实现并发与背压。
- **流式变体**：`Service<AgentRequest, Response = AgentEventStream>`，让 axum 直接 SSE、TUI 直接订阅，避免「先跑完再返回」。
- 有状态会话仍由 `Agent`（`Session`）承担；两者共享 core 契约。

### 4.3 cli 以 Service / Layer 组合

- `tower-agent-cli` 用 `ServiceBuilder` 装配：provider Service →（provider 中间件）→ runtime → `BoxAgentService`；中间件（观测、超时、并发、录制）以 `Layer` 叠加。
- CLI/TUI 需要流式事件时，使用 §4.2 的流式形态（或给 runtime 挂一个「事件 tap」Layer），不再直接依赖内部 `Agent` 类型。

### 4.4 UI 后端与 TUI 渲染分离

- 宿主（host）统一提供：request/stream 通道、命令通道、事件订阅、Layer 装配。
- `tower-agent-ui` 是前端无关的 UI 会话插件：归约 `AgentEvent`、维护 transcript 视图、执行斜杠命令，并输出 `submit` / `handled` / `quit` 动作。GUI 与 axum 可直接持有其 `AgentUi`，读取或序列化 `UiModel` 快照。
- `tower-agent-tui` 只实现 ratatui 渲染、编辑器和 termina 事件循环；它依赖 `tower-agent-ui`，不依赖 runtime/provider/core，也不反向被门面 crate 依赖。
- 入口适配器：`tower-agent-axum`（handler/SSE）、`tower-agent-tonic`（gRPC）、`tower-agent-cli`、未来 web。

### 4.5 静态插件 = Layer

把 `StaticPlugin` 的贡献面从「只登记 `AgentTool`」扩展为：

- `agent_layers()`：agents 级中间件（观测、限流、录制、上下文变换）；
- `tool_layers()`：工具执行级中间件（超时、重试、权限）；
- `tools()`：工具实现（每个工具本身仍是 `Layer` + `Service`）；
- `event_handlers()`：纯通知事件（不进 Layer）。

注册表（能力/依赖/版本收敛/覆盖/生命周期/HostApi/权限）保留；`PluginContext` 从「工具集合」升级为「Layer 栈 + 工具 + 能力」。

### 4.6 热重载插件 = monty（替代 Roto）

- 用 monty（Python 子集）作为 codemode Python 引擎，解释器编译为 wasm 组件并由 Wasmtime 执行。
- 宿主能力调用通过 `MontyHost` 派发；脚本未定义函数会挂起为宿主动作，宿主完成后 resume。
- 热更新使用 `reload_checked`：新版本试跑成功后原子替换，失败保留旧版本。
- 资源限制由 Wasmtime 组件、WASI 配置、内存/栈上限和调用超时共同约束；trap 只终止本次脚本调用，不波及宿主进程。
- 旧 `tower-agent-plugin` 的 Roto JIT 实现已退役；当前热重载由独立的 `tower-agent-monty` 提供。

### 4.7 命名与仓库

- 仓库改名 `tower-agent`（目录/git remote 由你操作，我可以改 crate 名与文档）。
- crate 名统一 `tower-agent-*`；对外发布 facade `tower-agent`。

### 4.8 transcript（会话上下文）与无状态语义（详解）

#### 4.8.1 transcript 是什么

transcript（会话记录 / 上下文）是 **按顺序发给模型的全部消息**：system 提示 + 用户消息 + assistant 消息（含 tool call）+ 工具结果。它就是 agent 的「对话记忆」。

代码里的落点：

- 现在 `tower_agent_runtime::Core.messages: Vec<AgentMessage>` 就是 transcript（`crates/tower-agent-runtime/src/agent.rs`）；
- 每次 run 用 `AgentContext { messages, tools }` 把它交给编排循环；
- `convert_to_llm` 把 `AgentMessage[]` 转成模型真正吃的 `Message[]`；
- 事件 `MessageStart/Update/End` 折进 transcript（`reduce()`）；
- `AgentState.messages` 是对外快照。

#### 4.8.2 为什么必须有（用途）

1. **模型上下文**：LLM API 是无状态的——每次请求都要把 system + 历史消息 **整段重发**。不发就没有多轮记忆。
2. **工具循环**：assistant 发出 tool call 后，必须把 tool result 追加进 transcript，下一次请求模型才能看到结果继续。
3. **多轮连续**：follow-up / 追加提问都依赖既有 transcript。
4. **流式累积**：assistant 的流式增量要拼成一条完整消息再进 transcript。
5. **持久化 / 恢复**：pi 的会话文件、会话树/分支，存的就是 transcript（+元数据）。
6. **上下文管理**：截断 / compaction 的输入输出都是 transcript。
7. **UI**：TUI 渲染的「消息列表」就是 transcript 的视图。
8. **计费 / usage**：每条消息的 usage 归属。
9. **排队注入点**：steer / follow-up 在 drain 点插入 transcript。

一句话： **transcript 是 agent 唯一真正重要的状态**；其余（模型/工具配置、队列）都是它的附属。

#### 4.8.3 关键决定：谁持有 transcript

这直接决定「无状态 Service」长什么样：

- **A. 客户端持有（严格无状态 Service）**：请求携带完整 `messages`，服务器不保存任何会话状态。像 OpenAI Chat Completions。
    - 优点：水平扩展/无粘性、handler 是纯函数、重试幂等、与 LLM API 心智一致。
    - 代价：每轮重发整段上下文（token/带宽）；服务端无法天然做跨请求 steer/队列/服务端 compaction；客户端要自己管 transcript（或另做存储）。
- **B. 服务端持有（有状态会话 + 池化句柄）**：`session_id → Agent 实例（含 transcript）`；`AgentPool` 借还 worker。
    - 优点：服务端管记忆，支持 steer/follow-up、长会话、服务端 compaction；接近 pi 的 `Session`。
    - 代价：粘性会话/状态亲和、需要存储与淘汰、并发加锁、HA 更难。
- **C. 混合（推荐）**： **Service 契约是无状态的（请求带 messages）**，另外提供一个可选的 `SessionStore`（core 定义接口、runtime 提供内存/文件实现）：传 `session_id` 就不必重发，传 `messages` 就按无状态跑。
    - axum handler 天然是 A；需要服务端会话时叠一层 SessionStore；两种用法共享同一 core 契约。

#### 4.8.4 响应形态

- **非流式** `AgentResponse { messages, text, usage, stop_reason }`：简单，但看不到首个 token 前的进度与工具执行过程。
- **流式** `Response = AgentEventStream`（Item 为 `AgentEvent` 或其精简版）：SSE / TUI 必须；transcript 是事件的 fold（`message_start/update/end`、`tool_execution_*`、`usage`、`agent_end`）。
- 建议两者都提供：非流式 = 流式的 fold，core 定义同一套事件协议。

#### 4.8.5 对本次重构的影响

- `tower-agent-core` 定义：`AgentRequest`（含 `messages`）、`AgentResponse`、`AgentEventStream`、事件协议。
- `tower-agent-runtime` 提供：`Agent`（有状态，持 transcript）+ `AgentService`（无状态，请求带上下文）+ `AgentPool`（池化，借还 worker）。
- 注意：现在的 `Service<AgentRequest>` 把请求 `messages` 当「追加 prompt」用（内部 transcript 仍保留）；无状态模式要把它改成「请求即完整上下文」，两者语义要分清楚。
- cli/tui 默认走有状态会话（贴近 pi），无状态/池化主要服务多请求宿主。

## 5. 落地阶段

**本轮（聚焦 core / runtime / stateless / cli）**

- **P0 清理与冻结契约**：删掉 `crates/plugin-dyn-echo`、`crates/plugin-sandbox` 空目录，恢复 `cargo metadata`（已恢复）；在 `tower-agent-core` 冻结 `AgentRequest` / `AgentResponse` / `AgentEventStream` / tool 边界 / Layer 契约。
- **P1 ✅ 抽 core**：`tower-agent-core` 已建立并迁入契约类型（`error` / `event` / `tool` / `layer` / `registration` / `service`：`AgentRequest` / `AgentResponse` / `AgentEventStream` / `BoxAgentService` / `AgentServiceExt`）；`agent` 通过 re-export 保持 `tower_agent_runtime::*` 路径兼容，行为与 pi 对齐测试不变。
- **P2 ✅ 改名与 facade**：全部 crate 加 `tower-agent` 前缀并落到 `crates/tower-agent-*` 目录——`tower-agent-core` / `tower-agent-runtime` / `tower-agent-provider` / `tower-agent-tools` / `tower-agent-plugin` / `tower-agent-plugin-sdk` / `tower-agent-ui` / `tower-agent-tui` / `tower-agent-protocol` / `tower-agent-cli`（二进制名 `tower-agent`），并新增 facade crate `tower-agent`（再导出各层；`ui` / `protocol` / `plugin-sdk` 为可选 feature，终端 `tui` 不进入门面）。
- **P3 ✅ 无状态 / 池化 / 流式**：`tower-agent-runtime` 新增 `AgentService`（无状态：请求带完整 transcript，每次用全新 `Agent` 跑一轮）、`AgentStreamService`（`Response = AgentEventStream`）、`AgentPool`（信号量限制并发、`poll_ready` 背压）、`MemorySessionStore`；`tower-agent-core` 新增 `SessionStore` 与 `AgentRequest.session_id`（按 C：无状态契约 + 可选会话存储）。
- **P4 ✅ cli 走 Service/Layer**：`tower-agent-cli` 新增 `stack::assemble`，用 `ServiceBuilder` + `ConcurrencyLimitLayer` 把 runtime 装配成类型擦除的 `BoxAgentService`（单写者串行 turn）；控制面抽成 `AgentControl`/`BoxAgentControl`（订阅、状态、切模型/工具/流、取消、重置）。`Session` 只持 `BoxAgentService` + `BoxAgentControl`，turn 走 `Service::oneshot`，不再直接持 `Agent` 具体类型。provider 侧中间件（retry/timeout）仍由 `provider` 模块装配。

**后续（本轮不做）**

- **P5 ✅ 静态插件 Layer 化**：`tower-agent-plugin::layer` 提供对象安全的 `AgentLayer` / `ToolLayer` / `EventHandler`（含闭包与 tower `Layer` 适配器 `tower_agent_layer` / `tower_tool_layer`）；`PluginContext` 增加登记 + `apply_agent_layers` / `apply_tool_layers` / `dispatch_event`；`StaticPlugin` 增加 `agent_layers()` / `tool_layers()` / `tools()` / `event_handlers()` 默认方法，`Resolution::install` 统一收集。
- **P6 ✅ monty 替换 Roto**：删除旧 JIT 路径，新增 `tower-agent-monty`：monty 解释器编译为 wasm 组件并由 Wasmtime 执行，宿主函数经 `MontyHost` 回调，`reload` 原子替换源码、`reload_checked` 失败保留旧版本。
- **P7 ✅ UI 后端与 TUI 拆分**：`tower-agent-ui` 提供前端无关的 `AgentUi`、事件归约、命令协议与静态插件；`tower-agent-tui` 仅保留 ratatui 页面和终端输入。门面 crate 不再依赖 `tower-agent-tui`。
- **P8 入口适配器**：axum / tonic / mcp / web。

## 6. 待确认（仅剩 transcript / 无状态语义）

1. **transcript 归属**（§4.8.3）：已按 **C（无状态契约 + 可选 `SessionStore`）** 落地；A（纯客户端持有）与 B（服务端有状态会话）也都被覆盖——不配 `SessionStore` 即为 A，配 `session_id` 即为服务端会话。

（其余已确认：monty 作为 Python 热重载后端；全前缀命名；本轮 core/runtime/stateless/cli；保留关键 agent 行为回归测试；响应形态两者都要。P0、P1、P2、P3、P4 已完成。）
