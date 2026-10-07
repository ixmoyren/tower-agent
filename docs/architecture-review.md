# tower-agent 架构评审（对照新需求）

> 目的：把当前实现盘点清楚，并逐条对照「用 Rust/tower 构建通用 agent（tower-agent，定位类似 tower-mcp / tower-llm）」的新需求，标出差距与需要确认的冲突。
> 说明：本文只做评审，不改代码。

## 0. 结论摘要

- 当前实现本质上是 **「pi 的 Rust 移植（tower-agent）」**，已经具备 tower 骨架：Provider、Tool、Agent 三层都是 `tower::Service`，钩子是 `tower::Layer`。
- 距离「通用 tower-agent」还差三件大事：
    1. **定位/命名与对外契约**：现在是 `agent` / `tower-agent`，不是独立的 `tower-agent` facade 与稳定公开契约。
    2. **与 tower 生态（axum）的良好结合**：`Agent` 是有状态单写者，`poll_ready` 恒 Ready、忙时在 `call` 里报错；缺少面向 axum 的无状态/池化包装与示例。
    3. **插件体系与需求不一致**（后续已解决）：静态插件当时只注册工具、热重载当时是 Roto。现状：静态插件已 Layer 化（`AgentLayer` / `ToolLayer` / `EventHandler`），热重载已改用 monty（`tower-agent-monty`，基于 `monty-pool`）。
- 另有一个 **当前仓库本地阻塞**：`crates/plugin-dyn-echo/`、`crates/plugin-sandbox/` 是空目录（只剩空 `src/`），而 `members = ["crates/*"]`，导致 `cargo metadata` 直接失败（`failed to read .../Cargo.toml`）。空目录未被 git 跟踪，属本地残留，删掉即可恢复解析。

## 1. 现状盘点

### 1.1 crate 结构

```
crates/
  ai/           provider Service / Models 路由 / 传输层中间件 / 错误与消息类型（对应 pi packages/ai）
  agent/        agent 运行时、工具边界、事件、编排循环（对应 pi packages/agent）
  tools/        7 个内置工具实现（read/shell/edit/write/grep/find/ls）
  plugins/      插件宿主：模型 / 注册表 / 解析 / 生命周期 / 宿主 API / 权限 / 动态加载 / JIT
  plugins-sdk/  插件作者 SDK：declare_plugin! / declare_dynamic_plugin! / HostApi
  protocol/     帧 / CBOR 子集 / RPC envelope（对应 pi packages/protocol）
  tui/          ratatui + termina 终端前端（纯库，附 UiPlugin 扩展点）
  tower-agent-cli/      CLI 入口 + 会话装配（原 cli）
examples/
  plugin-example/  示例动态插件（cdylib，独立 workspace）
```

### 1.2 tower 骨架现状

- **Provider**：`tower_agent_provider::StreamService` = `Service<StreamRequest, Response = AssistantEventStream>`，错误 `PiError`；「错误编码进流」是硬契约。传输/服务中间件是 `tower_agent_provider::provider::layers`（`HeaderTransport` / `RetryTransport` / `ObserveTransport` / `TimeoutService`）。
- **Tool**：`tower_agent_runtime::AgentToolService` = `Service<ToolCallRequest, Response = ToolResultStream, Error = AgentError>`；每个内置工具实现 `Service` **且**实现 `tower::Layer`（`crates/tower-agent-tools/src/layer.rs`），`create_*_tool` 把工具层叠到 `ToolNotFound` 上。
- **Agent**：`Agent`（`crates/tower-agent-runtime/src/agent.rs:280`）实现 `Service<AgentRequest, Response = AgentResponse, Error = AgentError>`（`:775`），并有 `BoxAgentService = BoxCloneService<...>`（`:317`）与 `AgentServiceExt::boxed_agent`。
- **钩子**：`tower_agent_runtime::layer` 提供 `BeforeToolCallLayer` / `AfterToolResultLayer`（`tool_call` / `tool_result` 的 Layer 形态）。
- **typed 工具**：`tower_agent_runtime::ToolRegistration`（schemars + serde）。
- **事件**：`AgentEvent` / `EventBus` 是 **逐订阅者 mpsc + 背压**（`docs/design.md` D3/Q6 明确不用 broadcast）， **不进 tower 中间件**（D1）；TUI 订阅事件流。

### 1.3 插件体系现状

- **静态插件**：`tower_agent_plugins::StaticPlugin { manifest(); install(&mut PluginContext) }`，`install` 把 `AgentTool` 登记进上下文；`tower_agent_plugins::default_registry` 为 7 个内置工具各注册一个静态插件。注册表支持能力/依赖/版本收敛/同名覆盖（静态<动态<JIT）/环检测。
- **生命周期/宿主 API/权限**：`Lifecycle` / `PluginState` / `InflightGuard`、`HostApi` / `PluginAccess`（`requires` 收窄 + 中介调用，目前只中介 `tool.*`）、`PermissionPolicy` / `PermissionPrompter`。
- **动态插件（stabby）**：`plugins` 的 `dynamic` feature（`dep:stabby` + `dep:libloading`）；`crates/tower-agent-plugins/src/dynamic.rs` 定义 `AbiVersion` / `AbiCapability` / `AbiManifest` / `PluginVTable`，`LoadedPlugin::load/call/stop/unload`，做 SDK ABI major 校验。`plugins-sdk` 的 `dynamic` feature 提供 `declare_dynamic_plugin!`。
- **热重载/JIT**：已改为 `tower-agent-monty`（早期实现是 `plugins` 的 `jit` feature + Roto，已删除）。
- 注意：插件 ABI 类型定义在 **宿主** `tower_agent_plugins::dynamic`，`plugins-sdk` 只是再导出；因此插件作者会连带依赖 `plugins → tools → agent → ai` 整棵树。

### 1.4 cli / tui 与 agent 的耦合

- `tower-agent-cli::session::Session` 直接持有 `agent: Agent`（`crates/tower-agent-cli/src/session.rs:24`），通过 `tower_agent_runtime::Subscription` 拿事件；`tower_agent_tui::run_agent_tui(subscription, ...)`（`crates/tower-agent-tui/src/backend.rs:176`）也是订阅事件流。
- 也就是说： **cli/tui 现在依赖的是 `Agent` 的具体类型 + 事件总线，而不是 `Service<AgentRequest>`**。

## 2. 需求对照

| 需求                                                  | 现状                                                                               | 差距                                                                                                               |
|-------------------------------------------------------|------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------|
| 名称/定位为 **tower-agent**，像 tower-mcp / tower-llm | crate 叫 `agent`，文档/描述是 `tower-agent`；已有 `Service`/`BoxCloneService` 形态 | 缺 `tower-agent` 这个门面（facade）与稳定对外契约；命名/发布形态未定                                               |
| pi-agent 架构迁移为 **tower layer service**           | Provider / Tool / Agent 三层都是 `Service`；钩子是 `Layer`                         | `agent_loop` 编排仍是函数；**事件/流不做 layer**（design.md D1/D3）；「静态插件 = 一组 Layer」未实现               |
| 与 **tower 生态**结合、可被 **axum** 集成             | `Agent: Service`、`AgentError: Into<BoxError>`、可 `.layer()`/`boxed_agent()`      | `Agent` 有状态单写者、`poll_ready` 恒 Ready、忙时报错；缺无状态/每请求包装、缺 axum 集成示例、缺流式事件的对外接口 |
| pi 的 cli/tui **调用 tower-agent**                    | cli/tui 直接持有 `Agent` + `Subscription`                                          | 没有走 `Service` 边界；若要「Service 化」需要重新定义流式/事件如何暴露                                             |
| 插件：**静态插件 = tower layer**                      | 工具是 Layer；插件本身只登记工具                                                   | 缺「插件贡献 layer 栈」（`agent_layers()`/`tool_layers()` 这类扩展点）                                             |
| 插件：**动态插件（stabby）**                          | 已实现（`dynamic` feature + `LoadedPlugin` + `declare_dynamic_plugin!`）           | 基本满足；ABI 类型与宿主耦合、缺少稳定的插件 ABI crate                                                             |
| 插件：**可热重载（基于 monty）**                      | 当前是 **Roto + cranelift**                                                        | **硬冲突**：需求要 monty，仓库/缓存/依赖里都没有 monty                                                             |

## 3. 主要冲突与风险

1. **热重载运行时：monty vs roto**。现有 `jit` feature 与 `JitPlugin` 完全基于 Roto；需求改为 monty。需要确认 monty 到底是什么（crate？git？语言运行时？API？），这决定是替换还是并列新增。
2. **定位：pi 移植仓 vs 通用 tower-agent**。`docs/design.md` 的目标是「行为对齐 pi、不新增 pi 没有的功能」；而 tower-agent 是通用组件。这两个目标会打架（例如流式事件、会话状态、默认值、错误类型）。需要明确这份仓库最终是「tower-agent 本体」还是「tower-agent 的一个 pi 移植参考实现」。
3. **有状态 Agent vs axum 多请求**。`Agent` 内含 transcript 与队列，`AgentService` 目前是「同一实例的顺序服务」。axum 场景通常需要每请求无状态或从共享状态派生。
4. **cli/tui 的调用边界**。若强行让 TUI 走 `Service`，会与「TUI 是事件流订阅、不是请求-响应」的既有决策冲突（design.md 4.1/D1）。
5. **插件 ABI 与依赖耦合**。`plugins-sdk` 依赖 `plugins`，`plugins` 依赖 `agent/tools/ai`，导致一个最小动态插件会拖入整棵 host 依赖树；作为通用 tower-agent 的插件生态这不理想（建议独立的 `plugins-abi` crate）。
6. **当前本地构建阻塞**：`crates/plugin-dyn-echo/`、`crates/plugin-sandbox/` 空目录使 `cargo metadata` 失败（见 §0）。`examples/plugin-example` 是唯一被跟踪的示例插件。

## 4. 需要你确认的问题

1. **monty 是什么？** 给出 crate 名 / git 地址 / 大致 API 或热重载机制；它是替换 Roto（JIT 热重载唯一后端），还是新增一个「热重载插件」种类（静态 / 动态 / JIT / 热重载 四类）？
2. **tower-agent 与本仓库的关系**：`tower-agent` 是独立仓库/独立 crate（通用），本仓库继续作为它的 pi 参考实现？还是把本仓库整体重命名/重构为 `tower-agent`？
3. **对外 crate 布局与命名**：是「单个 `tower-agent` facade 再导出 core/provider/tools/plugins」，还是把 `agent` 直接改名 `tower-agent`、`ai` 改名 `tower-agent-provider`？`protocol` 是否属于 tower-agent 的核心？
4. **Agent 的 Service 语义**：保留「有状态、单写者」（`Session` 持一个实例），还是同时提供「每请求无状态 / 池化」的 `Service` 形态给 axum？
5. **cli/tui 集成方式**：保持事件流订阅（现状），还是改成 tower `Service`/`Layer` 组合？如果是后者，流式增量事件如何暴露给 UI？
6. **静态插件是否要恢复「Layer 束」能力**（插件贡献 agent/tool 的 Layer 栈），而不只是注册工具？
7. **插件宿主是否属于 tower-agent 的公开能力**（发布给第三方写插件），以及 ABI（stabby）版本策略？
8. **是否仍以「pi 行为对齐」为目标**，还是以通用 agent 框架为目标（允许偏离 pi）？

## 5. 建议（供讨论）

- 先冻结 **tower-agent 公开契约**：`AgentRequest` / `AgentResponse` / `BoxAgentService` + 「Layer 扩展点」，再决定内部 crate 命名与 facade。
- 把「 **有状态运行时 `Agent`**」与「 **可组合 `AgentService`**」分层：前者是会话/队列/事件源，后者是面向 tower 生态（axum、中间件、测试）的无状态或句柄式包装。
- 插件分三类定契约： **静态 = 一组 `tower::Layer`（含工具）**、 **动态 = stabby cdylib**、 **热重载 = monty**；把 ABI 抽到独立 `plugins-abi`，让插件作者不依赖整棵 host。
- 明确 cli/tui 的边界：保留「TUI 订阅事件流」，但让 cli/tui 通过 tower-agent 的 **公开接口**（而不是内部 `Agent` 类型）装配。
- 明确本仓库定位（pi 参考实现 vs tower-agent 本体）后再谈目录/命名迁移。
