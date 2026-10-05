# 插件 / 扩展体系（子设计）

> 状态：设计稿（尚未实现）。
> 关联：[`design.md`](./design.md)（总体设计、第 4.3 节 provider 分层、第 4.i~4.iv 移植路线）。
> 本文只描述插件体系本身的结构与规则，不改变 `agents` / `ai` / `tools` / `tui` / `cli` 的既有契约。

## 1. 目标

把 tower-pi-agent 从「编译期内置能力集合」升级为「可组合、可扩展、可热插拔的能力平台」，满足：

1. **内置能力插件化**：核心插件与内置 tool 走统一注册/解析体系，而不是写死在 `tools::default_tools()`。
2. **三种插件形态**：静态（tower Layer）、动态（stabby `cdylib`）、JIT（Roto + cranelift），覆盖从「零开销内置」到「热加载的 mods」的整条光谱。
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
| 宿主（Host）              | tower-pi-agent 本体，负责加载、解析、中介调用与生命周期                                 |
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

| 维度     | 静态插件                       | 动态插件                        | JIT 插件                      |
|----------|--------------------------------|---------------------------------|-------------------------------|
| 载体     | Rust crate（编译进宿主）       | `cdylib`（`.so` / `.dll`）      | Roto 脚本（`.roto`）          |
| 组合方式 | `tower::Layer` 叠加到服务栈    | stabby ABI 导出入口             | cranelift 编译为宿主内模块    |
| 加载     | 编译期                         | `libloading` 运行时加载         | 运行时编译 + 绑定             |
| 卸载     | 不卸载（随进程）               | 支持卸载                        | 支持热替换                    |
| 热更新   | 不支持（重新构建）             | 需 stop→unload→load（可选实现） | 原生热加载                    |
| 调用开销 | 零（直接函数调用）             | 一次间接调用 + ABI 编解码       | 编译后接近原生                |
| 典型用途 | 内置 tool、provider 层、观测层 | 沙箱、记忆、持久化、codemode    | cc `mods`、用户脚本、轻量扩展 |
| feature  | 默认开启                       | `dynamic`                       | `jit`                         |

### 4.1 静态插件（tower Layer）

- 每个静态插件实现 `StaticPlugin`，在 `install()` 里向宿主注册：
    - 若干 **能力**（`provides`）；
    - 若干 **`tower::Layer`**（中间件：超时、重试、鉴权、观测、限流……）；
    - 若干 **工具**（`AgentTool`）。
- 静态插件之间通过 `Layer` 顺序 + 能力注册组合； **内置 tool（`read` / `shell` / `edit` / `write` / `grep` / `find` / `ls`）迁移为 `core-tools` 静态插件**（第 9 节）。
- 静态插件 **不能**依赖动态或 JIT 插件（第 6.5 节）：它是进程内的编译期单元，必须自洽。

### 4.2 动态插件（stabby）

- 用 `plugins-sdk` 编写，`crate-type = ["cdylib"]`，导出 stabby 入口（如 `__tpa_plugin_entry`），返回 `PluginVTable`。
- 宿主通过 `libloading` 加载，做 **ABI 版本 + 插件版本** 双重校验，再调用 `init` 拿到能力表。
- 卸载安全：宿主以 `Arc<LoadedPlugin>` 持有模块，调用带引用计数； **存在 in-flight 调用时拒绝卸载**（`stop` 后等待归零）。
- 大插件落在这里：`sandbox`（进程/权限隔离）、`memory`、`persist`（会话/向量/文件持久化）、`codemode`。

### 4.3 JIT 插件（Roto + cranelift）

- 用 Roto 语言编写脚本，宿主用 Roto + cranelift 编译成模块；宿主函数（import）即能力调用。
- 文件监听 + 原子换绑实现 **热加载**：编译新版本 → 校验 → 原子替换槽位 → 旧版本等待 in-flight 完成后释放；失败保留旧版本并回报错误。
- 目标场景：cc 的 `mods` 模式（用户/LLM 可写、可热更的轻量逻辑）。

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

### 6.1 名字覆盖与优先级

- 同名插件（`PluginId.name` 相同）允许共存，但只有 **最高形态**的一份处于激活态：

```text
静态(0)  <  动态(1)  <  JIT(2)
```

- 覆盖是 **整体替换**：激活实现提供的 `provides` 集合生效，被遮蔽的实现进入 shadow 列表（供 `/plugins` 诊断），不参与解析。
- 能力名冲突（不同插件提供同名能力）默认同样按形态优先级裁决；如需并存，使用显式 alias 或不同 `major` 的多版本槽位（6.4）。

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

菱形：`A→B`、`A→C`、`B→D`、`C→D`。由于依赖落在能力上，D 的实现选择遵循：

1. 取所有消费者 `VersionReq` 的 **交集**；
2. 有交集 → 选满足交集的 **最高版本**（单实例，语义等价于 Cargo 的 semver 收敛）；
3. 无交集但 **不跨 major** → 报 `VersionConflict`（不静默降级）；
4. 无交集且 **跨 major** → 允许 **多版本槽位并存**：按 `major` 各加载一份，消费者绑定各自的 `major`（动态/JIT 可加载多份；静态插件按编译期唯一实例处理，跨 major 只能声明兼容）。

> 默认策略： **同 major 收敛到最高版本，跨 major 允许并存**。宿主 ABI（`sdk_abi`）独立校验，不参与此收敛。

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
- 任何阶段失败都回滚到上一个稳定状态，并保持其余插件不受影响（隔离失败）。

## 8. 宿主 API / SDK

### 8.1 宿主对外（插件可用能力）

- **固有能力**（不可卸载）：`model.*`（模型补全流）、`fs.*`、`process.*`、`log`、`config`、`events`。
- **可插拔能力**：`tool.*`、`sandbox.*`、`memory.*`、`persist.*`、`code.*`。
- 插件只能调用其清单 `requires` 声明过的能力，且受 `permissions` 约束（第 12 节）。

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

- **跨形态调用不直连**：无论动态还是 JIT，都通过 `HostApi` 调 `host.call(capability, args)`；宿主完成实现查找、版本绑定与 in-flight 记账。这既统一了两者的互调，也让卸载/热更可以安全等待。

## 9. 内置工具迁移到静态插件

1. 新增 `core-tools` 静态插件，`provides`：`tool.read` / `tool.shell` / `tool.edit` / `tool.write` / `tool.grep` / `tool.find` / `tool.ls`（版本从 `1.0.0` 起）。
2. `tools::default_tools()` 改为「构造插件清单 + 注册表解析」；`cli` 的 `select_tools`（`--tools` / `--exclude-tools` / `--no-tools`）在解析结果上做过滤。
3. 共享实用件（`truncate` / `path_utils` / `queue` / `edit_diff`）留在 `tools`，作为 `core-tools` 的实现细节，不对外暴露成能力。
4. 迁移完成后，`tools` crate 只负责「内置工具的实现」，注册与选择全部由 `plugins` 承担。

## 10. crate 划分

```text
crates/
  plugins/        宿主核心：模型、注册表、解析（图/版本/覆盖）、生命周期、静态插件
  plugins-sdk/    插件作者 SDK：stabby 封装、宏、HostApi（dynamic/jit 共用）
  plugins-jit/    Roto + cranelift 宿主集成（feature = jit）
  plugins-builtin/ 内置静态插件（core-tools、provider 中间件层等）
```

- 依赖方向：`plugins-builtin → plugins`、`plugins-jit → plugins`、`plugins-sdk → plugins`（仅共享类型）；`plugins` **不**依赖具体插件 crate，避免循环。
- `agent` / `ai` / `tools` 不反向依赖 `plugins`；`plugins` 依赖它们以包装 `AgentTool`、`BoxStreamService` 等。

## 11. Feature / 构建

```toml
[features]
default = ["static"]
static = []
dynamic = ["dep:stabby", "dep:libloading"]
jit = ["dep:roto", "dep:cranelift"]
```

- 默认只带静态插件，保证无额外平台/依赖负担。
- 动态/JIT 为可选 feature；`plugins-sdk` 单列，供插件 crate 以 `cdylib` 形式构建。

## 12. 安全与隔离

- 清单声明 `permissions`（如 `net`、`fs.write`、`process.spawn`）；宿主加载时校验并授予能力句柄，未授予的调用直接失败。
- 沙箱作为 **动态插件**提供 `sandbox.exec`，把不可信执行隔到子进程/受限运行时。
- 热更/卸载的边界：只允许在引用计数归零后释放；JIT 换绑是原子操作。

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

## 14. 测试策略

- **单元**：覆盖优先级、shadow 记录、环检测、菱形收敛、跨 major 并存、静态→动态拒绝。
- **静态插件**：内置工具经注册表解析后与 `tools::default_tools()` 等价（迁移前后行为不变）。
- **动态**：`dynamic` feature 下编译一个测试 `cdylib`，验证加载、调用、`UnloadBusy`、卸载。
- **JIT**：`jit` feature 下编译脚本、调用宿主能力、热更失败回滚。
- **失败注入**：加载失败不污染既有插件；in-flight 时卸载被拒。

## 15. 里程碑

| 里程碑 | 内容                                                                              |
|--------|-----------------------------------------------------------------------------------|
| M1     | `plugins` 核心：模型 + 注册表 + 解析（覆盖/环/菱形/版本）+ 静态插件；内置工具迁移 |
| M2     | `plugins-sdk` + 动态加载器（stabby）：加载/调用/卸载 + 示例插件                   |
| M3     | `plugins-jit`（Roto + cranelift）：编译、宿主能力调用、热加载与回滚               |
| M4     | 大型动态插件：沙箱 / 记忆 / 持久化 / codemode；cc `mods` 模式接入                 |

## 16. 未决问题

1. **多版本并存的粒度**：按 `major` 并存是否足够，还是需要按 `minor`？静态插件跨 major 只能选一份。
2. **工具名 vs 插件名的覆盖关系**：`tool.read` 的覆盖是跟随插件名优先级，还是允许对单个能力单独覆盖？
3. **权限模型细节**：`permissions` 的粒度（按能力 / 按资源 / 运行时提示）。
4. **JIT 热更的一致性**：热更瞬间正在构造的请求绑定哪个版本（建议绑定调用发起时版本）。
5. **stabby ABI 版本策略**：`sdk_abi` 的兼容窗口（建议 `^` 且 major 不兼容即拒绝）。
6. **与 provider 层的关系**：provider 中间件（retry/timeout/observe）是否也全部静态插件化，还是保留在 `cli::provider`。
