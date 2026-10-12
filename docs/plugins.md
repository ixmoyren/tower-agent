# 插件 / 扩展体系（子设计）

> 状态：v0.6（M1 静态插件、M2 stabby 动态插件、M3 monty 热重载已实现；静态插件已 Layer 化，见下方实现状态）。
> 关联：[`design.md`](./design.md)（总体设计、第 4.3 节 provider 分层、第 4.i~4.iv 移植路线）。
> 本文只描述插件体系本身的结构与规则，不改变 `agents` / `ai` / `tools` / `tui` / `tower-agent-cli` 的既有契约。

## 1. 目标

把 tower-agent 从「编译期内置能力集合」升级为「可组合、可扩展、可热插拔的能力平台」，满足：

1. **内置能力插件化**：核心插件与内置 tool 走统一注册/解析体系，而不是写死在 `tower_agent_tools::default_tools()`。
2. **三种插件形态**：静态（tower Layer）、动态（stabby `cdylib`）、热重载（monty，Python 子集沙箱），覆盖从「零开销内置」到「热加载的 mods」的整条光谱。
3. **跨形态能力调用**：动态与 JIT 插件可以互相调用对方提供的能力。
4. **确定性解析**：无循环依赖；菱形依赖用版本机制收敛；同名插件按形态优先级覆盖。
5. **可卸载 / 可热更**：动态插件可加载可卸载；JIT 插件可热加载，服务于后续 cc `mods` 模式。

## 2. 术语

| 术语                      | 含义                                                                                    |
|---------------------------|-----------------------------------------------------------------------------------------|
| 插件（Plugin）            | 一组能力的提供单元，有名字、版本、形态、依赖声明                                        |
| 能力（Capability）        | 宿主或其他插件可调用的接口单元，形如 `namespace.name`（如 `tool.read`、`sandbox.exec`） |
| 提供者 / 消费者           | 声明 `provides` / `requires` 的一侧                                                     |
| 槽位（Slot）              | 注册表中「一个能力名 → 当前激活实现」的位置，可被覆盖/热更                              |
| 宿主（Host）              | tower-agent 本体，负责加载、解析、中介调用与生命周期                                    |
| 中介调用（Mediated Call） | 插件之间不直接链接，而是通过宿主的能力注册表互相调用                                    |

## 3. 总体结构

```text
                        ┌─────────────────────────── Host ───────────────────────────┐
                        │  PluginRegistry                                            │
   manifests ─────────▶ │   ├─ 名字/能力槽位（含 shadow 记录）                       │
                        │   ├─ 依赖图 + 拓扑排序（无环）                             │
                        │   ├─ 版本解析（菱形收敛 / 多 major 并存）                  │
                        │   └─ 生命周期状态机                                        │
                        │                                                            │
                        │  CapabilityHost（中介层）                                  │
   dynamic ◀──────────▶ │   tools / model / sandbox / memory / persist / code ...    │ ◀──▶ JIT
                        │                                                            │
                        │  内建能力（固有能力，不可被插件卸载）：model / fs / process│
                        └────────────────────────────────────────────────────────────┘

   静态插件 ── tower::Layer ──▶ 直接组合进宿主服务栈（进程内、零 ABI 开销）
```

三种形态共存于同一张注册表；解析规则（第 6 节）与生命周期（第 7 节）对所有形态一致。

## 4. 三种插件形态

| 维度     | 静态插件                       | 动态插件                        | 热重载插件                      |
|----------|--------------------------------|---------------------------------|---------------------------------|
| 载体     | Rust crate（编译进宿主）       | `cdylib`（`.so` / `.dll`）      | monty 脚本（Python 子集）       |
| 组合方式 | `tower::Layer` 叠加到服务栈    | stabby ABI 导出入口             | 内嵌 wasm 组件（wasmtime 执行） |
| 加载     | 编译期                         | `libloading` 运行时加载         | 运行时编译 + 换绑               |
| 卸载     | 不卸载（随进程）               | 支持卸载                        | 支持热替换                      |
| 热更新   | 不支持（重新构建）             | 需 stop→unload→load（可选实现） | 原生热加载                      |
| 调用开销 | 零（直接函数调用）             | 一次间接调用 + ABI 编解码       | 编译后接近原生                  |
| 典型用途 | 内置 tool、provider 层、观测层 | 沙箱、记忆、持久化、codemode    | cc `mods`、用户脚本、轻量扩展   |
| feature  | 默认开启                       | `dynamic`                       | `jit`                           |

### 4.1 静态插件（tower Layer）

- 每个静态插件实现 `StaticPlugin`，在 `install()` 里向宿主注册：
    - 若干 **能力**（`provides`）；
    - 若干 **工具**（`AgentTool`）；每个内置工具本身就是一个 `tower::Layer`（第 9 节）。
- 插件还可贡献三层中间件与事件处理器：`agent_layers()`（包 `BoxAgentService`）、`tool_layers()`（包 `BoxToolService`）、`event_handlers()`（纯通知）。`AgentLayer` / `ToolLayer` 是对象安全的装箱层 trait，也可用 `tower_agent_layer` / `tower_tool_layer` 适配任意 tower `Layer`；宿主用 `PluginContext::apply_agent_layers` / `apply_tool_layers` 组合、`dispatch_event` 派发。
- 静态插件之间通过 `Layer` 顺序 + 能力注册组合； **内置 tool（`read` / `shell` / `edit` / `write` / `grep` / `find` / `ls`）各自是一个独立的静态插件**，拆出来的每个 tool 都是 tower 中的 layer（第 9 节）。
- 静态插件 **不能**依赖动态或 JIT 插件（第 6.5 节）：它是进程内的编译期单元，必须自洽。

### 4.2 动态插件（stabby）

- 用 `plugins-sdk` 编写，`crate-type = ["cdylib"]`，导出 stabby 入口（如 `__tpa_plugin_entry`），返回 `PluginVTable`。
- 宿主通过 `libloading` 加载，做 **ABI 版本 + 插件版本** 双重校验，再调用 `init` 拿到能力表。
- 卸载安全：宿主以 `Arc<LoadedPlugin>` 持有模块，调用带引用计数； **存在 in-flight 调用时拒绝卸载**（`stop` 后等待归零）。
- 大插件落在这里：`sandbox`（进程/权限隔离）、`memory`、`persist`（会话/向量/文件持久化）、`codemode`。

> 实现状态（M2 已落地）：`plugins` 的 `dynamic` feature（=`dep:stabby` + `dep:libloading`）提供 `LoadedPlugin::load` / `call` / `stop` / `unload`，`#[repr(C)]` 的 `PluginVTable`（`manifest` / `init` / `call` / `shutdown`）与 stabby 的 ABI 稳定清单（`AbiManifest` / `AbiCapability` / `AbiVersion`）做 SDK ABI major 校验；`plugins-sdk` 的 `dynamic` feature 提供 `declare_dynamic_plugin!`。示例插件在 `../examples/plugin-dyn`（独立 workspace 的 cdylib）。

### 4.3 热重载插件（monty）

- 用 monty（Python 子集）编写脚本；宿主把 monty 解释器编译成 **wasm 组件**（`wasm/monty-sandbox`）内嵌进二进制，用 wasmtime 在沙箱里执行（崩溃/越界/内存超限转成 trap，不波及宿主）；脚本调用的未定义外部函数即宿主能力调用（组件返回 `function-call` → `MontyHost` 回调 → `resume`）。
- 原子换绑实现 **热加载**：试跑新版本 → 成功才替换源码槽位 → 旧版本等待 in-flight 完成后释放；失败保留旧版本并回报错误。
- 目标场景：cc 的 `mods` 模式（用户/LLM 可写、可热更的轻量逻辑）。

> 实现状态（M3 已落地，mods 作者闭环见下）：独立 crate `tower-agent-monty` 提供 `MontyPlugin::load` / `activate` / `call_function` / `reload` / `reload_checked` / `stop` / `unload`。 **执行在 wasm 沙箱里**：`wasm/monty-sandbox` 把 monty 解释器（及其 ruff 解析器依赖）编译成 `wasm32-wasip2` 组件，经 `wasm-opt -Oz --strip-debug` 优化后内嵌进 `tower-agent-monty`（`assets/monty-sandbox.wasm`，约 6.5 MB），宿主用 wasmtime 的 **组件绑定**驱动，开启 JIT + JIT 缓存（缓存键含 wasmtime 版本，升级后自动失效）。没有外部 `monty`
> CLI、没有子进程、不需要 `TPA_MONTY_BIN`；mods 代码的越界/栈溢出/内存超限被 wasmtime 转成 trap 错误，不会带走宿主进程。WIT 接口是宿主驱动的 REPL 状态机（`start` + `resume-*`），参数/返回值都是 JSON；脚本里未定义的函数调用经 `MontyHost::dispatch` 异步派发到宿主，热加载用 `reload_checked` 试跑成功后原子替换源码。重新生成组件：`./wasm/monty-sandbox/generate.sh`。
>
> **mods（cc `mods` 模式）**：`ModManager` 管理一组 monty mod，对 LLM 暴露 `mod_write` / `mod_reload` / `mod_list` / `mod_remove` 作者工具。mod 脚本可：
> 1. **调宿主能力**：`tool_call(name, args)` / `log` / `state_get` / `state_set`；
> 2. **提供工具**：安装期 `tool_register(name, function, description, parameters)` 把 Python 函数包成 `ModTool`，运行时动态注册进 `HostApi` 与 agent 工具集（`tool_unregister` 注销）；
> 3. **贡献 UI**：`ui_status` / `ui_panel` / `ui_clear` / `ui_tool_render` 写进前端无关的 `tower_agent_core::UiState`（状态栏片段 + 面板文本 + 工具结果渲染模板），TUI 由 `tower_agent_tui::ContributionPlugin` 渲染，web/GUI 前端消费同一份数据。
>
> 沙箱侧还提供： **实例池**（`SandboxPool`，一次运行借出、跑完归还）+ **预热**（`ModManager::warmup` / `spawn_warmup` 后台编译组件、预建实例）、 **文件系统挂载**（`ModManager::mount*` 用 `monty-fs` 的 `MountTable` 在宿主侧服务 OS 调用：`ReadOnly` / `OverlayMemory` / `ReadWrite`）与 **每-mod 权限**（`ModPermissions`：`tools` / `register_tools` / `ui` / `state` / `fs`，在派发与 OS 处理时强制）。CLI 默认把工作目录只读挂到 `/work`。
>
> **三形态同表（解析规则一致）**：mod 以 `PluginKind::Jit` 载荷注册进 **同一个 `PluginRegistry`**（内置工具是同一注册表里的静态插件），因此覆盖优先级（静态 < 动态 < JIT）、能力级覆盖、依赖图/拓扑排序、版本收敛与被遮蔽记录（`ModManager::shadows`）对三形态统一生效——同名 JIT mod 会遮蔽同名静态插件。工具目录的唯一来源是「解析 + 安装」的结果（`ModManager::tools`），`HostApi` 同步获得同一份工具集；`--tools` / `--exclude-tools` / `--no-tools` 以 `ToolFilter` 作用在该目录之上。
>
> **`protected` 标记与遮蔽规则**：`PluginManifest.protected` 的实现 **永不被遮蔽**——JIT 运行时（agent-monty）就以一张 protected 插件清单注册（提供 `tool.mod_*` 与 `jit.run`），任何试图用同名插件/同名工具遮蔽它的 mod 会被 `ModManager` **阻断**并回滚。除它之外：JIT 可以遮蔽静态与动态插件， **JIT 之间也允许**，规则是「后写覆盖先写」，工具目录按名去重并登记 `ModManager::shadowed_tools()`。
>
> **持久化（`dirs`）**：mod 默认只活在会话内，源码同时写到 `dirs::cache_dir()/tower-agent/mods`（临时，不算保存）；`mod_persist` 把它复制到 `dirs::config_dir()/tower-agent/mods`（跨会话保留，启动时自动加载），`mod_discard` 卸载并从 config/cache 删除。动态插件从 `dirs::config_dir()/tower-agent/plugins` 加载（`.so`/`.dll`/`.dylib`，其 `tool.<name>` 能力被包装成工具，因此可被更高优先级的 JIT mod 遮蔽）。

## 5. 插件模型

```rust
/// 插件形态；数值即覆盖优先级（越大越优先）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PluginKind {
    Static = 0,
    Dynamic = 1,
    Jit = 2,
}

/// 插件标识：名字 + semver 版本。
pub struct PluginId {
    pub name: String,
    pub version: semver::Version,
}

/// 能力标识：命名空间 + 名字。
pub struct CapabilityId {
    pub namespace: String, // tool / sandbox / memory / persist / code / model ...
    pub name: String,
}

/// 声明「提供某能力」。
pub struct ProvidedCapability {
    pub id: CapabilityId,
    pub version: semver::Version,
}

/// 声明「依赖某能力」；依赖的是**能力**，不是具体插件。
pub struct Dependency {
    pub id: CapabilityId,
    pub req: semver::VersionReq,
    pub optional: bool,
}

/// 插件清单（静态插件在编译期提供，动态/JIT 在加载时解析）。
pub struct PluginManifest {
    pub id: PluginId,
    pub kind: PluginKind,
    pub sdk_abi: semver::Version, // 宿主 SDK / ABI 版本
    pub provides: Vec<ProvidedCapability>,
    pub requires: Vec<Dependency>,
    pub permissions: Vec<Permission>, // 沙箱/权限声明
}
```

要点：

- **依赖针对能力而非插件**：消费方只声明「需要 `tool.read >=1.2`」，由宿主选择满足条件的实现。这天然支持覆盖与菱形收敛。
- 清单中的 `sdk_abi` 与 `PluginId.version` 分离：前者是宿主兼容性，后者是插件自身语义版本。
- 静态插件的清单由 Rust 侧常量提供；动态/JIT 的清单在加载时随入口一起返回。

## 6. 注册表与解析

### 6.1 覆盖与优先级

覆盖分两级，解析时从高到低：

1. **能力级覆盖（显式）**：`CapabilityOverride` 把某个 `CapabilityId` 直接绑定到指定实现（插件名 + 版本 / 形态）。用于「插件整体排在后面，但它的某一项能力要胜出」这种细粒度场景。
2. **插件级覆盖（默认）**：同名插件（`PluginId.name` 相同）按形态优先级裁决：

```text
静态(0)  <  动态(1)  <  JIT(2)
```

- 插件级覆盖是 **整体替换**：激活实现提供的 `provides` 集合生效，被遮蔽的实现进入 shadow 列表（供 `/plugins` 诊断），不参与解析。
- 能力级覆盖只改被点名的能力，其余能力仍走插件级优先级与版本规则。
- 解析顺序： **能力级覆盖 → 插件级优先级 → 版本规则（6.4）**。

### 6.2 依赖图与拓扑排序

1. 收集所有未被遮蔽插件的能力索引：`CapabilityId → [候选实现]`。
2. 对每个插件实例的每条 `requires`，从候选里选满足 `VersionReq` 的实现（按 6.1 优先级、6.4 版本规则）。
3. 建立有向图 `consumer → provider`，Kahn 拓扑排序得到 `init` 顺序。
4. 缺失/可选缺失分别处理：必需缺失 → `MissingCapability` 错误；可选缺失 → 关闭相关分支。

### 6.3 循环依赖

- **任何形态之间都禁止循环依赖**（含动态 ↔ JIT 互调）。
- 运行时通过宿主中介调用不算「绕过」：声明层仍必须无环，否则解析直接失败。
- 检测：Tarjan/Kahn；错误信息带出 **完整环路径**（`a → b → c → a`）。

### 6.4 菱形依赖与版本机制

菱形：`A→B`、`A→C`、`B→D`、`C→D`。依赖落在能力上，D 的实例化策略按形态区分：

**动态 / JIT：按 major 多版本并存**

1. 版本匹配一律用 `^`（semver 兼容）；若某消费者要求的主版本与候选实现 **major 不兼容**，直接 `VersionConflict`， **不静默降级**。
2. 同一 `major` 内：取满足所有消费者 `^req` 的最高版本，收敛为 **单实例**。
3. 不同 `major`：允许多版本槽位 **并存**（动态/JIT 各加载一份），消费者绑定各自 `major`。

**静态：单实例，不支持多版本**

- 静态插件编译进宿主，一个名字只有一份实例。
- 它的版本必须满足所有消费者的 `^req`；否则解析失败（`VersionConflict`），由使用者调整依赖，或把该能力下沉到动态/JIT。

> 宿主 ABI（`sdk_abi`）独立校验：一律 `^` 兼容， **major 不兼容即拒绝加载**，不参与上述能力版本收敛。

### 6.5 形态约束

| 依赖方 → 被依赖方 | 静态 | 动态 | JIT |
|-------------------|------|------|-----|
| 静态              | ✅   | ❌   | ❌  |
| 动态              | ✅   | ✅   | ✅  |
| JIT               | ✅   | ✅   | ✅  |

- 静态插件在 **清单校验阶段**就拒绝依赖动态/JIT（`KindViolation`），因为它编译进宿主、不存在「运行时再加载对方」的时机。
- 动态与 JIT 互相依赖时，调用一律走宿主中介（第 8 节），不直接链接彼此的 ABI。

## 7. 生命周期

```text
Discover ─▶ Load ─▶ Validate(manifest/ABI/权限) ─▶ Resolve(图/版本) ─▶ Init ─▶ Start
                                                                              │
                                                        Stop ─────────────────┤
                                                          ▲                   │
                                          Reload(JIT) ────┘                   │
                                                                              ▼
                                                           Unload(dynamic only)
```

- **静态插件**：`Init/Start` 在进程启动阶段完成；无 `Stop/Unload`。
- **动态插件**：`Unload` 前必须 `Stop`，且引用计数归零；否则 `UnloadBusy`。
- **JIT 插件**：`Reload` = 编译新版本 → 校验 → 原子替换槽位 → 旧模块等待 in-flight 归零后释放；失败回滚到旧版本。
- **调用在发起时绑定版本**：in-flight 调用继续在旧版本上完成，`Reload` 之后发起的新调用使用新版本；一次调用不会跨版本。
- 任何阶段失败都回滚到上一个稳定状态，并保持其余插件不受影响（隔离失败）。

> 实现状态：`tower_agent_plugins::Lifecycle` / `PluginState` / `InflightGuard` 已落地（合法迁移、`Started ↔ Stopped` 供 JIT 重载、`Unload` 前要求 `Stopped` 且 in-flight 归零，否则 `UnloadBusy`）。动态/JIT 的实际 `dlopen` / 编译换绑随加载器落地。

## 8. 宿主 API / SDK

### 8.1 宿主对外（插件可用能力）

- **固有能力**（不可卸载）：`model.*`（模型补全流）、`fs.*`、`process.*`、`log`、`config`、`events`。
- **可插拔能力**：`tool.*`、`sandbox.*`、`memory.*`、`persist.*`、`code.*`。
- 插件只能调用其清单 `requires` 声明过的能力，且受 `permissions` 约束（第 12 节）。

> 实现状态：`tower_agent_plugins::HostApi` / `PluginAccess` 已落地——宿主按能力名中介调用 `tool.<name>`，`access_for(manifest)` 按 `requires` 收窄视图，未声明的调用直接 `PermissionDenied`。

### 8.2 SDK

- `plugins-sdk` 提供：
    - 清单/入口宏（静态与动态共用 `declare_plugin!`）；
    - `HostApi` 句柄（能力调用的类型化封装）；
    - 工具注册助手 `ToolRegistration`（把 `AgentTool` 暴露成 `tool.<name>` 能力）。
- 动态插件入口（stabby）示意：

```rust
// 插件侧（cdylib）
stabby::export! {
    #[stabby::export]
    extern "C" fn __tpa_plugin_entry(host: HostApi) -> &'static PluginVTable;
}
```

- **跨形态调用不直连**：无论动态还是 JIT，都通过 `HostApi` 调 `host.call(capability, args)`；宿主完成实现查找、 **调用发起时的版本绑定**与 in-flight 记账。这既统一了两者的互调，也让卸载/热更可以安全等待。

> 实现状态：`plugins-sdk` crate 已落地，提供 `DeclaredPlugin` / `declare_plugin!`（静态插件路径）、`HostApi` / `ToolRegistration` 再导出与共享类型。stabby 的 `__tpa_plugin_entry` 动态入口随 `plugins` 的 `dynamic` feature 落地。

## 9. 内置工具迁移到静态插件

1. **每个内置工具本身是一个静态插件**：`read` / `shell` / `edit` / `write` / `grep` / `find` / `ls` 各自注册为独立插件，分别提供 `tool.read` / `tool.shell` / … / `tool.ls`（版本从 `1.0.0` 起），不再由一个 `core-tools` 插件整体打包。
2. **每个 tool 都是 tower 中的 layer**：`tools` crate 里每个工具除实现叶服务 `Service<ToolCallRequest>` 外，还实现 `tower::Layer<S>`——层叠到内层按名分派服务上后，命中本工具名就执行工具、否则委托内层。`tower_agent_tools::create_*_tool` 把工具层叠到「工具未找到」基础服务上，得到发给 agent 的可执行服务。工具层可以与 tower 生态的其它 layer（`timeout` / `limit` / `buffer` ……）自由组合。
3. **工具钩子也是 layer**：`tower_agent_runtime::layer` 提供 `BeforeToolCallLayer` / `AfterToolResultLayer`，分别对应 pi 的 `tool_call`（可放行/改写/拦截）与 `tool_result`（逐条改写结果）钩子。
4. **typed 工具注册**：`tower_agent_runtime::ToolRegistration::new::<P, _, _>(...)` 用 schemars 生成参数 schema、用 serde 反序列化入参，校验失败编码为 `is_error` 工具结果。
5. `plugins` 宿主把这些工具逐个注册为静态插件，并通过 `default_registry` / `resolve_default` / `default_tools` 提供内置集合；`tower-agent-cli` 的 `select_tools`（`--tools` / `--exclude-tools` / `--no-tools`）在解析结果上做过滤。
6. 共享实用件（`truncate` / `path_utils` / `queue` / `edit_diff`）留在 `tools`，作为工具的实现细节，不对外暴露成能力。
7. 不再有独立的 `plugins-builtin` crate：内置工具插件由 `plugins` 宿主直接装配，`plugins → tools` 依赖单向。
8. **provider 中间件不插件化**：`retry` / `timeout` / `observe` 由 `tower-agent-cli::provider` 直接编排（传输层叠 `RetryTransport`，服务层叠 `TimeoutService`），不进入注册表。

## 10. crate 划分

```text
crates/
  tower-agent-plugins/      宿主核心：模型、注册表、解析、生命周期、静态插件、插件层（AgentLayer/ToolLayer/EventHandler）、动态加载器（feature = dynamic）
  tower-agent-plugin-sdk/   插件作者 SDK：宏、HostApi、插件层类型
  tower-agent-monty/        热重载插件宿主（monty + monty-pool，子进程崩溃隔离）
```

- 依赖方向：`plugins-sdk → plugins`（仅共享类型）；`plugins → tools`（宿主装配内置工具插件），`tools` 是工具实现所在 crate。
- 热重载宿主独立成 crate `tower-agent-monty`（不依赖具体动态插件 ABI）。
- `agent` / `ai` / `tools` 不反向依赖 `plugins`；`plugins` 依赖它们以包装 `AgentTool`、`BoxStreamService` 等。

## 11. Feature / 构建

```toml
[features]
default = ["static"]
static = []
dynamic = ["dep:stabby", "dep:libloading"]
# 热重载后端见 tower-agent-monty（monty + monty-pool）
```

- 默认只带静态插件，保证无额外平台/依赖负担。
- 动态/JIT 为可选 feature；`plugins-sdk` 单列，供插件 crate 以 `cdylib` 形式构建。

## 12. 安全与隔离

- 清单声明 `permissions`（如 `net`、`fs.write`、`process.spawn`）； **授权在运行时按需提示**：某能力首次被调用且需要权限时，宿主在界面上询问用户（允许一次 / 本次会话允许 / 拒绝），决定按会话缓存；无人值守模式（`--print`）默认拒绝并在错误里说明。
- 宿主只把已授予的能力句柄交给调用方，未授予的调用直接 `PermissionDenied`。
- 沙箱作为 **动态插件**提供 `sandbox.exec`，把不可信执行隔到子进程/受限运行时。
- 热更/卸载的边界：只允许在引用计数归零后释放；JIT 换绑是原子操作。

> 实现状态：`tower_agent_plugins::PermissionPolicy` / `PermissionPrompter` / `PermissionDecision` 已落地——`AllowOnce` 不缓存、`AllowSession` 按会话缓存、`Deny` 拒绝，无人值守（`--print`）默认拒绝；TUI 提示的 UI 形态待接。

## 13. 错误处理

```rust
pub enum PluginError {
    Manifest(String),                  // 清单非法
    KindViolation { from: PluginKind, to: PluginKind }, // 静态 → 动态/JIT
    MissingCapability { consumer: PluginId, id: CapabilityId },
    VersionConflict { id: CapabilityId, reqs: Vec<String> },
    Cycle(Vec<PluginId>),              // 环路径
    Abi { expected: Version, found: Version },
    Load(String),
    UnloadBusy { name: String, inflight: usize },
    PermissionDenied { capability: CapabilityId },
}
```

解析失败必须给出 **可操作**的信息（缺哪个能力、哪条环、哪些版本互斥）。

> 实现状态：Rust 侧 `PluginError` 已含 `Load` / `UnloadBusy` / `PermissionDenied`，外加 `Manifest` / `KindViolation` / `MissingCapability` / `VersionConflict` / `Cycle` / `AbiMismatch` / `OverrideTargetMissing` / `Install`。

## 14. 测试策略

- **单元**：插件级覆盖优先级、 **能力级覆盖**、shadow 记录、环检测、同 major 收敛、跨 major 并存、静态单实例、静态→动态拒绝。
- **静态插件**：每个内置工具都是一个独立静态插件，经注册表解析后工具集合与迁移前等价。
- **权限**：首次调用触发运行时提示、决定按会话缓存、无人值守模式拒绝。
- **动态**：`dynamic` feature 下编译示例 `cdylib`（`../examples/plugin-dyn`），验证加载、调用、`UnloadBusy`、卸载。端到端用例读 `TPA_PLUGIN_EXAMPLE` 指向产物；未设置时跳过（避免与 `cargo test` 抢 target 锁）：

  ```text
  cargo build --manifest-path examples/plugin-example/Cargo.toml
  TPA_PLUGIN_EXAMPLE=<target>/debug/libtpa_plugin_example.so \
    cargo test -p plugins --features dynamic --test dynamic
  ```
- **JIT**：`jit` feature 下编译脚本、调用宿主能力（`host_call`）、源码/文件热加载、热更失败回滚。
- **失败注入**：加载失败不污染既有插件；in-flight 时卸载被拒。

## 15. 里程碑

| 里程碑 | 内容                                                                                                                                                                 |
|--------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| M1 ✅  | `plugins` 核心：模型 + 注册表 + 解析（覆盖/环/菱形/版本）+ 静态插件；内置工具（每个工具一个静态插件，都是 tower Layer）迁移；生命周期 / 宿主 API / 权限 / 错误已落地 |
| M2 ✅  | `plugins-sdk` + 动态加载器（stabby + libloading）：`LoadedPlugin` 加载/调用/`stop`/卸载 + `../examples/plugin-dyn` 示例 cdylib + 端到端测试                          |
| M3 ✅  | `tower-agent-monty`（monty + monty-pool）：宿主函数回调、原子换绑热加载与失败回滚                                                                                    |
| M4     | 大型动态插件：沙箱 / 记忆 / 持久化 / codemode；cc `mods` 模式接入                                                                                                    |

## 16. 已确认决策

1. **多版本**：静态插件 **不支持多版本**（单一实例，版本必须满足所有消费者的 `^` 要求）；动态与 JIT 按 **major 多版本并存**。
2. **覆盖粒度**：既支持插件名整体覆盖（形态优先级 `静态 < 动态 < JIT`），也允许 **对单个能力显式覆盖**（`CapabilityOverride`）。
3. **权限**：采用 **运行时提示**（首次调用需要权限的能力时询问用户，按会话缓存；无人值守默认拒绝）。
4. **热更一致性**：调用在 **发起时绑定版本**；in-flight 调用在旧版本上完成，之后的新调用用新版本。
5. **版本 / ABI 兼容**：一律 `^`（semver 兼容）； **major 不兼容即拒绝**，不静默降级。
6. **内置工具与 provider 中间件**：内置 tool（`read` / `shell` / …） **各自是一个静态插件，且每个都是 tower Layer**；provider 中间件（`retry` / `timeout` / `observe`） **不插件化**，由 `tower-agent-cli::provider` 直接编排。

仍待实现的细节（不阻塞立项）：权限提示的 UI 形态、`CapabilityOverride` 的配置来源（配置文件 / CLI / 运行时命令）。

## 16. codemode / wasmtime 插件化：已完成与待办

目标（2026-10-12 定）：`tower-agent-wasmtime` 与 `tower-agent-codemode` **都是静态插件**，
并且与 `tower-agent-monty` 一样 **不允许被动态插件与 JIT 插件用同名方式覆盖能力**；
静态插件与 **cargo feature** 联动，用 feature 决定「tower-agent 提供哪些能力」；
插件集合由 **组装层**（`tower-agent::assembly`）统一给出， **不在任何会话装配里硬编码**。

### 16.1 已完成

- **机制本来就有**：`PluginManifest.protected` + `PluginRegistry::{protected_names, protected_capabilities}`；
  `resolve()` 的规则是「受保护的插件永不被遮蔽：protected 组内取胜者，其余（含同名 JIT/动态）进 `Resolution::shadows()`」。
- **JIT 那条路早已响亮报错**（`manager::check_shadowing`，两处 JIT 入口），命中保护名或保留名即 `PluginError::Install`。
- **动态那条路补上了同名拒绝**：`dynamic::load_from_dir` 在注册前走 `registry.ensure_not_protected(name)`，
  命中即 `PluginError::Install { … "reserved by a protected static plugin and cannot be shadowed" }`——
  不再静默进入 shadow（与 JIT 行为一致）。
- **两个插件形态定义好了**（`CodemodePlugin`：静态 / `protected` / `provides("tool","codemode")` / `install` 注册工具；
  `WasmtimePlugin`：静态 / `protected` / `provides("sandbox","wasmtime")` / 空安装）。已确认新增依赖不成环。
- **组装层落位（2026-10-12 追加）**：宿主静态插件集合收敛到 `tower-agent::assembly::HostPlugins`
  （门面 crate，依赖 `tower-agent` 的 axum 等宿主同样能用）；`ModManager` 不再自己塞 wasmtime。
- **能力矩阵 feature 化（2026-10-12 追加）**：`tower-agent` 门面 `[features]` =
  `sandbox-wasmtime`（wasmtime 占位插件）/ `codemode`（codemode 插件）/ `mods`（monty JIT 宿主），
  组装层按 feature 决定注册哪些静态插件；`tower-agent-cli` 同样有 `mods` / `codemode` feature
  （默认都开），关掉后二进制只剩内置工具。两端 `--no-default-features` 都已验证编译。

### 16.2 待办（按顺序；落地后勾选）

- [x] **1. 组装层 + 登记进注册表**：宿主静态插件集合由 `tower-agent::assembly::HostPlugins`
  给出（内置工具 + `wasmtime`〔feature `sandbox-wasmtime`〕+ codemode〔feature `codemode`，
  `with_codemode(session, mounts)`〕）—— **与会话无关**，CLI / axum / 测试同一入口。
  `ModManager::new` 只再叠加自己的 JIT 运行时地基，不再自己塞 wasmtime；
  `ModManager::register_static_plugin(&self, Arc<dyn StaticPlugin>)` 保留，供运行时追加插件
  （CLI 的 `--codemode` 走它；未启用 mods 时退化为直接追加工具）。
  —— 这条同时修掉了一个真实缺口：mods 默认启用时工具目录来自 `mods.tools()`，旧的
  `base_tools.push` 根本不会生效，`--codemode` 会静默失灵。
- [x] **2. 抽出可测的检查**：`PluginRegistry::ensure_not_protected(name)` 成为唯一规则实现，
  `dynamic::load_from_dir` 与 JIT 的 `check_shadowing` 都调它（不再两处漂移）；
  `tests/resolution.rs` 覆盖「保护名 → Err / 普通名 → Ok」，monty 侧补 `register_static_plugin` 用例，
  `tower-agent/tests/assembly.rs` 覆盖组装层（内置工具 + wasmtime/codemode 受保护）。
- [x] **3. 能力级覆盖的口子**：`resolve()` 现在把「受保护插件提供的能力」收敛到受保护实现——
  `select_providers` 对受保护能力只考虑受保护提供者（JIT 形态优先级 / 更高版本不再能顶掉），
  `CapabilityOverride` 指向非受保护实现时返回新错误 `PluginError::OverrideProtected`
  （与「未知实现」的 `OverrideTargetMissing` 区分）；`tests/resolution.rs` 补三条断言。
- [x] **4. 静态插件与 cargo feature 联动**：`tower-agent` 门面加 optional 依赖 +
  `[features] default = ["sandbox-wasmtime", "codemode"]`、`sandbox-wasmtime = ["dep:tower-agent-wasmtime"]`、
  `codemode = ["dep:tower-agent-codemode", "sandbox-wasmtime"]`；组装层用 `#[cfg(feature = "…")]`
  决定注册哪些插件，`--no-default-features` 下只剩内置工具（已验证编译）。见 §11。
- [x] **5. mods（monty）也 feature 化**：`tower-agent` 加 `mods = ["dep:tower-agent-monty", "sandbox-wasmtime"]`
  可选依赖；组装层 `HostPlugins::mods(ui)` 在插件集合之上建 `ModManager`，monty 只再叠加自己的
  运行时地基。关掉 `mods` 时 monty 连它的 wasmtime 沙箱实现一起不编进来——「monty 硬依赖
  wasmtime」只剩在 monty 内部成立（要 mods 就必须有沙箱），对宿主不再构成约束。
- [x] **6. CLI 二进制的 feature 化**：`tower-agent-cli` 加 `[features] default = ["mods", "codemode"]`，
  `mods` / `codemode` 分别把 `tower-agent-monty` / `tower-agent-codemode` 变成 optional 依赖；
  `session` 与 `main` 里所有 mods / codemode 触点都 `#[cfg]` 包住——无 mods 时回退到 `base_tools`、
  作者工具的保留名判定恒假、TUI 不挂 mods 面板；无 codemode 时 `--codemode*` 明确报「本构建未包含」。
  `tokio` 补 `rt-multi-thread`（少了 monty 的隐式 feature 后，`main` 的多线程 runtime 才成立）。
  `cargo check/test -p tower-agent-cli --no-default-features` 已验证（只剩内置工具）。
