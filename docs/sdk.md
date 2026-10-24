# 插件能力与扩展点（`tower-agent-plugin-sdk`）

| 项目     | 值                                                                                          |
|----------|---------------------------------------------------------------------------------------------|
| 状态     | 当前实现 + 后续提案；tool / hook / Provider 能力已落地，其余扩展点按表推进                  |
| 关联     | [`design.md`](./design.md)、[`plugins.md`](./plugins.md)、[`codemode.md`](./codemode.md) §0 |
| 参考基线 | pi `packages/coding-agent/src/core/extensions/`（`ExtensionAPI`、41 个事件）                |

本文档回答： **"给 agent 加能力"这件事应该长什么样**，以及它如何落在现有 `tower-agent-plugin-sdk` / `tower-agent-plugin`，并与 codemode 脚本共用同一套语义。

---

## 1. 定位

pi 的扩展点是 `ExtensionAPI`（`pi.registerTool()` / `pi.on(...)` / `ctx.ui.*` …）。我们要一个对应的东西：

**`tower-agent-plugin-sdk` = 唯一的、面向作者的扩展点 API**（Q7 已定，不新建 crate）。

它要同时满足四类作者、四种前端：

| 作者                                 | 怎么用                                          |
|--------------------------------------|-------------------------------------------------|
| Rust 静态插件                        | 实现 trait / 调用注册 API，编译进宿主（零开销） |
| 动态插件（cdylib）                   | 同一套 API，但跨 stabby ABI                     |
| **codemode 脚本**（monty / JS）      | 同一套**语义**，以 host function 形式暴露       |
| 宿主自身（CLI / TUI / axum / webui） | 直接用 API 注册内置能力                         |

四种前端（CLI / TUI / axum / webui）必须共享同一份扩展点与注册表 —— 与 `docs/plugins.md` 第 8 节"跨形态调用不直连、一律走宿主中介"的结论一致。

非目标：

- 不重新发明一套与 `tower-agent-plugin` 并行注册表。SDK 是 **扩展点定义 + 注册入口**，`PluginRegistry` 仍是背后的解析 / 版本 / 覆盖机制。
- 不在本阶段对齐 pi 的全部 41 个事件；先做"够脚本改行为"的一条线（见 §3 缺口列）。

## 2. 与现有 crate 的关系

现状：

| crate                                                      | 现在负责                                                                                | 问题                                                   |
|------------------------------------------------------------|-----------------------------------------------------------------------------------------|--------------------------------------------------------|
| `tower-agent-plugin`                                       | 通用插件宿主：三形态、能力注册表、依赖/版本、生命周期、权限、`HostApi`                  | 面向"插件系统"，不是"扩展点"                           |
| `tower-agent-plugin-sdk`                                   | 插件作者：Host/ABI、tool、Provider、动态入口，以及可选 storage、codemode、jit、wasmtime | 扩展能力、打包/ABI、脚本运行契约与可选沙箱基础设施     |
| `tower-agent-runtime`                                      | `BeforeToolCallLayer` / `AfterToolResultLayer` / `Compactor` / `AgentOptions`           | 有钩子，但很薄、且不统一                               |
| `tower-agent-plugin-monty` / `tower-agent-plugin-rquickjs` | 脚本宿主能力（`tool_call` / `tool_register` / `state_*` / `log` / `ui_*`）              | 两个引擎已共享工具、状态、日志与 UI 语义；钩子仍有缺口 |

结论（`codemode.md` Q7 已定）： **扩展点作者 API 直接加在 `tower-agent-plugin-sdk` 上**，按模块分层；`tower-agent-plugin` 仍是背后的注册表与生命周期机制。

依赖方向固定为 `plugin-* 实现 → plugin-sdk → plugin`。所有插件实现
（tools / providers / ui / shell / turso / rquickjs / monty）只能依赖 SDK；
SDK 统一再导出宿主 API 和动态 ABI，避免插件直接耦合通用宿主 crate。

`tower-agent-plugin-sdk` 的目标形态（模块划分，不是新 crate）：

- `tool`：工具注册（含 pi 的 `exposure` / `namespace` / `annotations` / `outputSchema`）
- `command`：斜杠命令、快捷键、CLI flag
- `hook`：事件 / 钩子（工具调用前后、消息、回合、压缩…）
- `provider`：Provider 注册、JSONL wire、动态 Provider 适配；virtual model / MCP 后续扩展
- `ui`：UI 贡献（状态栏 / 面板 / 渲染器）
- `session`：发消息、追加条目、会话控制
- `host`：`HostApi` / 能力调用（ **已有**，保留）
- `packaging`：manifest 宏、stabby 动态入口、动态工具描述与 ABI 类型（ **已有**，保留）
- `storage`（feature）：配置、凭据、会话、历史、记忆与 codemode 脚本的持久化契约；默认关闭，由存储插件和需要持久化能力的宿主启用。
- `codemode`（feature）：脚本权限、CapabilitySurface、Hook、Prompt、解释器描述与 codemode 执行路由；同时启用 `wasmtime`。
- `jit`（feature）：JIT 插件 load / reload / unload 契约和解释器路由；依赖 `codemode` 与 `storage`。
- `wasmtime`（feature）：组件沙箱、WASI、资源限制、JIT 缓存与实例池；默认关闭，由 codemode、monty 或其他 wasm 解释器插件启用。它是编译期基础设施，不是插件能力。

解释器通过 `ScriptInterpreter` 向模型声明 id、名称、语言和支持的
`codemode` / `jit` 模式。`ScriptRuntimeCatalog` 汇总所有解释器，宿主将
`describe_for_llm()` 加入提示词；LLM 据此选择模式与语言，再由
`ScriptRuntimeRouter`、`CodemodeRegistry` 或 `JitRegistry` 路由到具体实现。

也就是说：plugin-sdk 从"只有打包 + HostApi"扩成"打包 + HostApi + 扩展点"。好处是脚本侧（codemode host function）与 Rust 侧天然同一套定义，不再有第三个 crate。

## 3. 扩展点对照（pi → 我们）

状态图例：✅ 已有 / ◐ 部分（形式不同或只读）/ ❌ 没有。

| pi 扩展点                                                                      | 现在                                                                                          | 缺口                                          | 目标 API                                           | 脚本可用                         |
|--------------------------------------------------------------------------------|-----------------------------------------------------------------------------------------------|-----------------------------------------------|----------------------------------------------------|----------------------------------|
| `registerTool`（含 `exposure` / `namespace` / `annotations` / `outputSchema`） | ✅ `AgentTool` 已带 exposure / namespace / annotations；SDK 提供 typed 注册与完整元数据       | provider 可逐步消费新字段                     | `plugin_sdk::tool::ToolMetadata::…`                | ✅（monty 已有 `tool_register`） |
| `ctx.executeTool(name, args)`（嵌套调用、`parentToolCallId`、记账）            | ❌                                                                                            | 需要 runtime 支持"工具内再调工具"的记录与事件 | `plugin_sdk::tool::call(...)`                      | ✅（monty 已有 `tool_call`）     |
| `pi.on(...)` 41 个事件（多数可 transform / block / continue）                  | ◐ Rust 与 codemode 脚本均支持 `tool_call` / `tool_result` hook；其余事件仍为通知面            | 其余事件                                      | `plugin_sdk::hook::on(Event, handler)`             | ✅ v0 工具前后事件               |
| `registerCommand` / `registerShortcut` / `registerFlag`                        | ❌ 斜杠命令是硬编码表                                                                         | 命令/快捷键/flag 注册表 + 前端接线            | `plugin_sdk::command::*`                           | ❌                               |
| `registerProvider` / `registerVirtualModel`                                    | ◐ Provider 作为普通 Static / Dynamic 插件注册 `Arc<dyn ProviderPlugin>`；virtual model 未实现 | virtual model 目录与动态替换                  | `plugin_sdk::provider::ProviderPluginRegistration` | ❌                               |
| `registerMcpServer`                                                            | ❌ MCP 未实现                                                                                 | MCP 客户端                                    | `plugin_sdk::provider::register_mcp(...)`          | ❌                               |
| `sendMessage` / `sendUserMessage` / `appendEntry`                              | ❌                                                                                            | 会话注入入口                                  | `plugin_sdk::session::*`                           | ❌                               |
| `ctx.ui.*`（对话框 / 状态 / widget / footer / 自定义组件）                     | ◐ `ui_status` / `ui_panel` / `ui_tool_render` 写进前端无关的 `UiState`                        | 交互式 UI（确认框、输入框）与渲染器注册       | `plugin_sdk::ui::*`                                | ◐（monty 已有 `ui_*`）           |
| `pi.events`（扩展间总线）                                                      | ◐ `HostApi` 中介调用（能力级）                                                                | 事件总线式订阅                                | `plugin_sdk::host::events()`                       | ❌                               |
| 会话控制（`setModel` / `setThinkingLevel` / `setActiveTools` / `compact`）     | ◐ `/model` `/thinking` `/compact` 有宿主实现                                                  | 暴露成扩展点                                  | `plugin_sdk::session::control()`                   | ❌                               |
| 工具结果渲染器 / Markdown transformer                                          | ◐ `ui_tool_render`（模板）                                                                    | 渲染器注册                                    | `plugin_sdk::ui::renderer(...)`                    | ◐                                |
| 资源发现（skills / prompts / themes / mcp.json）                               | ❌                                                                                            | ——                                            | `plugin_sdk::resources::*`                         | ❌                               |

**结论：现在能支撑"脚本注册一个工具 + 改 UI 片段"，不能支撑"脚本改变 agent 的决策路径"。** v0 的目标就是把第 3 行的 **工具前后**两条做实（Q4 已定），其余留到 S4。

### 3.1 v0 范围（Q4 已定）与后续补齐

**v0（本轮要做的两条）**：

1. `tool_call`：可改写入参、可拦截（已有 `BeforeToolCallLayer`，暴露成扩展点 + 让脚本可挂）。
2. `tool_result`：可改写结果（已有 `AfterToolResultLayer`）。

```rust
use tower_agent_plugin_sdk::hook::{self, ToolCall, ToolResult};
use tower_agent_runtime::ToolCallDecision;

let before = hook::on(ToolCall, | mut request| async move {
// 可改 request.tool_call.arguments；这里演示拦截。
Ok(ToolCallDecision::block("blocked by policy"))
});
let after = hook::on(ToolResult, | _request, mut result| async move {
result.is_error = false;
Ok(result)
});
context.register_tool_layer(before);
context.register_tool_layer(after);
```

handler 返回 `Result<…, AgentError>`。before/after hook 失败会转换成 `is_error` 工具结果，不会被静默吞掉。

加上已有的 **工具注册**（`tool_register` / `tool_call`），这三条合起来就等价于"现在 monty 能做的一切 + 能改变工具行为"。

Rust 工具注册现可直接写：

```rust
use tower_agent_plugin_sdk::{
    ToolAnnotations, ToolExposure, ToolNamespace,
    tool::{self, ToolMetadata},
};

let metadata = ToolMetadata::new("demo.echo", "Echo text")
.exposure(ToolExposure::Codemode)
.namespace(ToolNamespace::new("demo"))
.annotations(ToolAnnotations { read_only_hint: Some(true), ..Default::default () })
.output_schema(serde_json::json!({ "type": "object" }));
let tool = tool::typed::<Params, _, _ > (metadata, handler);
context.register_tool(tool);
```

现有 `ToolRegistration` 仍从 SDK 根与 `tool` 模块再导出；已构造的 `AgentTool` 也可用 `AgentToolMetadataExt::with_metadata` 补齐元数据。

**后续（S4，按价值排序）**：

3. `context` / `system_prompt`：可改写本轮请求的上下文与系统提示（pi 的 `context` / `before_agent_start`）。
4. `message_end`：可替换定稿消息（pi 的 `message_end`）。
5. `turn_end` / `agent_before_settle`：可追加条目并请求一次续跑（pi 的 `continue: true`， **注意防死循环**）。

压缩钩子（`CompactionHook`）已经有，可作为扩展点接线的第一个样例。

## 4. 分层与调用路径

```
   Rust 插件 ──┐
   动态插件  ──┼─▶ tower-agent-plugin-sdk（扩展点 API）
   codemode  ──┘        │
   脚本(host call) ─────┘
                        ▼
              ExtensionRegistry（扩展点 → 实现）
                        │
        ┌───────────────┼────────────────┬──────────────┐
        ▼               ▼                ▼              ▼
   工具集/路由   钩子管线(Layer)      UI 贡献       命令/flag 表
        │               │                │              │
        └──── tower-agent-runtime / core / ui / tui ────┘
```

要点：

- **一个注册表**：SDK 的注册就是往 `ExtensionRegistry` 写；插件的 `PluginContext`（`register_tool` / `register_agent_layer` / `register_tool_layer` / `register_event_handler`）与它是同一件事的两种写法。目标是把 `PluginContext` 收敛成 SDK 的一个视图。
- **Rust 与脚本同名**：`registerTool` ↔ `tool_register`、`pi.on("tool_call")` ↔ `on("tool_call")`。脚本侧是 host function，宿主侧把参数解码后调用同一个注册表。
- **同步 vs 异步**：Rust 钩子可以返回 `BoxFut`（runtime 已经是这个风格）；脚本侧由引擎适配（monty 用 `function-call` + `resume-function` 挂起，JS 用同步 import + Promise 包装）。

## 5. 脚本侧表面（host functions）

按 `codemode.md` §0.3 的 `CapabilitySurface` 定义，脚本可见的扩展点如下（名字暂定）：

| host function                                               | 语义                                      | 权限开关         |
|-------------------------------------------------------------|-------------------------------------------|------------------|
| `exec(name, args, opts)`                                    | 跑 coreutils-wasm 命令                    | `exec`           |
| `fs.read/write/readdir/...`                                 | 会话文件系统（挂载表）                    | `fs`             |
| `tool_call(name, args)`                                     | 调用宿主工具                              | `tools`          |
| `tool_list()`                                               | 列出可用工具                              | `tools`          |
| `tool_register(name, fn, desc, params)` / `tool_unregister` | 注册新工具                                | `register_tools` |
| `on(event, handler)`                                        | 挂钩子（`tool_call` / `tool_result` / …） | `hooks`          |
| `register_command(name, handler)`                           | 注册斜杠命令                              | `commands`       |
| `state_get` / `state_set` / `log`                           | 脚本私有状态与日志                        | `state`          |
| `ui_status` / `ui_panel` / `ui_clear` / `ui_tool_render`    | UI 贡献                                   | `ui`             |

约束：

- 每个 host function 都必须 **有权限开关**，未授权时返回明确错误（不是静默忽略）。
- 钩子里的返回值要能被引擎序列化回来（monty：`resume-function` 的 JSON；JS：返回值）。
- 钩子执行必须能 **超时 / 取消**，否则一个坏脚本会卡住整个 turn。

`exec` 的跨引擎 JSON wire 为 `{code:int, stdout:string, stderr:string}`，输入是
`name:string`、`args:string[]`、`opts:{stdin?:string, timeout_ms?:int, cwd?:string}`。
字节一律按 UTF-8 字符串传输；非法输出字节用替换字符表示。monty 的 `exec` 与 Python
内建同名，因此引擎预载包装函数并转发到内部 host call `__codemode_exec`。`cwd`
当前仅保留在类型里；coreutils 适配器会明确拒绝，直到统一文件系统实现落地。

## 6. 权限模型

旧模型有 `tools / register_tools / ui / state / fs` 五个字段；monty 现在保留
`ModPermissions` 这个名字，但它是 `ScriptPermissions` 的类型别名。引擎无关的共享模型已经落在
`tower-agent-plugin-sdk`：

```rust
pub struct ScriptPermissions {
    pub exec: bool,           // coreutils-wasm
    pub fs: bool,             // 实际可写范围仍由挂载决定
    pub tools: bool,          // tool_call / tool_list
    pub register_tools: bool, // tool_register
    pub hooks: bool,          // on(...)
    pub commands: bool,       // register_command
    pub ui: bool,
    pub state: bool,
}
```

`ScriptPermissions` 与 `Capability::ALL` 一一对应；`authorize` 返回
`CapabilityError::Unauthorized`。所有能力错误都带稳定的 `kind` tag，可直接序列化为
JSON 回传脚本（未授权 / 未知能力 / 引擎错误 / 超时 / 取消 / 非法请求）。实际写范围仍由
挂载表决定，权限开关只负责能力级准入。

与 `docs/plugins.md` 第 12 节的运行时提示机制打通：首次调用需要权限的扩展点时询问用户（`AllowOnce` / `AllowSession` / `Deny`），无人值守默认拒绝。

脚本 hook 由 `tower-agent-plugin-sdk::ScriptHookRegistry` 统一注册和调用：

- `on("tool_call", handler)` / `on("tool_result", handler)` 在脚本安装时注册；同一 owner 的同一事件重复注册会替换。
- registry 按 owner 稳定顺序调用 handler，默认每个 hook 5 秒超时，并接受 `CancellationToken`。
- monty 通过 `call_function`、JS 通过脚本函数调用适配为统一的 `ScriptHookHandler`。
- 脚本卸载或 JS host drop 时按 owner 注销；权限由 `ScriptPermissions.hooks` 强制。

## 7. 里程碑（提案）

| 阶段  | 内容                                                                                                                                                     |
|-------|----------------------------------------------------------------------------------------------------------------------------------------------------------|
| S0    | 本文档 + `codemode.md` §0（评审） ✅ 决策已定：Q1/Q2/Q4/Q7                                                                                               |
| S1 ✅ | **`tower-agent-plugin-sdk` 扩展点 v0**：`tool` + async `hook`；工具前后钩子已接 `BeforeToolCallLayer` / `AfterToolResultLayer`，Rust 侧可用              |
| S2 ✅ | 脚本侧 host function（v0 范围）：`tool_call` / `tool_register` / `on(tool_call)` / `on(tool_result)`；权限矩阵；prompt 同步；两引擎已改变真实 agent 行为 |
| S3    | `command` / `ui` 扩展点；Rust 与脚本都能注册斜杠命令                                                                                                     |
| S4    | runtime 事件面补齐（§3.1 的第 3–5 条），脚本可真的“改行为”                                                                                               |
| S5    | Provider 注册能力 ✅；`mcp` / `resources` 后续补齐                                                                                                       |

## 8. 待决问题

| #    | 问题                                                       | 备选                                                                                                |
|------|------------------------------------------------------------|-----------------------------------------------------------------------------------------------------|
| S-Q1 | ✔️ 已定：扩展点落在 `tower-agent-plugin-sdk`，不新增 crate | 见 `codemode.md` Q7                                                                                 |
| S-Q2 | 扩展点的注册粒度                                           | (a) 每种扩展点一个注册函数；(b) 一个 `Extension` trait 全都在里面（类似 pi 的 `ExtensionAPI` 对象） |
| S-Q3 | 钩子能否"拦截"而非"观察"                                   | v0 已定要 `tool_call` 可拦截/改写（Q4）；`continue` 型留到 S4                                       |
| S-Q4 | 脚本钩子的生命周期                                         | 跟随脚本生效？跟随会话？如何卸载 / 热更                                                             |
| S-Q5 | pi 的 41 个事件里，哪些是 v1 必须的                        | v0 已定：只要工具前后两条；S4 再加 3 条                                                             |
