# codemode 设计

> v2（本节 §0， **提案 / 待评审**）：把 `mods` 与 `codemode` 统一成同一个能力—— **agent 可以编程**，
> 解释器可选（monty = Python / rquickjs-sandbox = JavaScript）。
> §1–§11 是 v1 的 **JS 引擎（rquickjs）实现细节**，作为该统一设计的第一个后端保留原文。

## 0. 统一设计（v2 提案 · 待评审）

### 0.1 一句话定位

**codemode = agent 可以编程**：LLM（或用户）生成一段脚本，agent 在沙箱里执行它。脚本能做两件事：

1. **组合工具**：在一次工具调用里跑多步「文件 + 命令 + 其它工具」，只把最终结果带回上下文（省 token、少往返）；
2. **扩展 agent**：注册新工具、挂上钩子、贡献 UI —— 也就是 pi extensions 那套"改 agent 行为"的能力。

`mods` 与 `codemode` 的差别只是 **第 1 条和第 2 条谁做得多**，以及 **用什么语言写脚本**，不是两种东西。
因此统一为一个名字： **`codemode`**。

### 0.2 现状盘点（2026-10-14）

| 部件           | crate / 产物                                                                                                        | 已有能力                                                                                                                                                                                                                                                                                                                                                                                                                                                     | 缺口                                                                           |
|----------------|---------------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------|
| 沙箱基础设施   | `tower-agent-wasmtime`：`SandboxEngine` / `HostState` / `Sandbox` / `SandboxPool`                                   | wasm 组件编译 + WASI + 内存/栈上限 + JIT 缓存 + 实例池；`wasmtime` 静态插件占位（`protected`）                                                                                                                                                                                                                                                                                                                                                               | ——                                                                             |
| guest 命令集   | `wasm/coreutils-wasm`（uutils multicall，**60 个命令**，argv[0] 分发；权威列表见 `tower_agent_codemode::COMMANDS`） | `exec(name, args, opts)`                                                                                                                                                                                                                                                                                                                                                                                                                                     | 两个引擎均已接入；monty 通过注入的 `CapabilitySurface` 转发                    |
| Python codemode | `tower-agent-monty` + `wasm/monty-sandbox`                                                                          | monty 子集、宿主导发的 REPL 状态机；共享能力表；`ScriptPermissions`；热重载；`ScriptStore` 持久化；`on("tool_call"/"tool_result")`；作者工具 `codemode_write` / `codemode_reload` / `codemode_list` / `codemode_remove` / `codemode_persist` / `codemode_discard` | JS 记录的启动执行仍待统一 host 分发；Python 宿主当前只加载 `monty` 记录 |
| codemode（JS） | `tower-agent-codemode` + `wasm/rquickjs-sandbox`                                                                    | rquickjs（guest）、`exec` + 60 个命令函数 + pipeline + `fs`；工具调用 / 注册、`on("tool_call"/"tool_result")`、state、log、UI；会话根（`<config>/tower-agent/codemode/sessions/<name>`）；命名会话列表 / 清理 | 脚本注册工具尚未自动同步到 CLI 的 agent 工具目录 |
| 插件体系       | `tower-agent-plugins` / `tower-agent-plugin-sdk`                                                                    | 静态（Layer）/ 动态（stabby cdylib）/ JIT（monty）三形态；能力注册表、依赖与版本、`protected`、权限提示；`HostApi` 中介调用                                                                                                                                                                                                                                                                                                                                  | 脚本扩展点仍少（见 [`sdk.md`](./sdk.md)）                                      |

结论：两者缺的恰好是对方有的。合并之后，一个脚本 **同时**能跑命令、调工具、注册工具、挂钩子。

### 0.3 目标架构

```
                        ┌──────────────────── 宿主（CLI / TUI / axum / webui）────────────────────┐
                        │  CodemodeHost                                                           │
   LLM 生成脚本 ──────▶ │   ├─ ScriptEngine（可插拔）：monty(Python) │ rquickjs-sandbox(JS)       │
                        │   ├─ CapabilitySurface（统一能力层）                                    │
                        │   │    exec / fs / tool_call / tool_list / tool_register /              │
                        │   │    state_get|set / log / ui_* / hooks / commands …                  │
                        │   ├─ 权限（Permissions，按会话 / 按脚本）                               │
                        │   ├─ 会话与存储（脚本仓库、会话根、快照）                               │
                        │   └─ PromptBuilder（按「引擎 + 能力集 + 挂载」生成给 LLM 的说明）       │
                        │                                                                         │
                        │  tower-agent-plugin-sdk：扩展点 API（Rust 侧与脚本侧同一套语义）        │
                        └─────────────────────────────────────────────────────────────────────────┘
                                        │
                                        ▼
                        tower-agent-wasmtime（引擎 / WASI / 限制 / 池）
                        wasm/coreutils-wasm（命令集） · wasm/*-sandbox（解释器组件）
```

三层职责：

1. **引擎层**（`ScriptEngine`）：语言与执行模型。monty 是"宿主导发的 REPL 状态机"（`start` / `resume-function` / `resume-os`），JS 是"同步 import + 预载 JS 门面"。差异只在这一层。
2. **能力层**（`CapabilitySurface`）： **两种引擎完全一致**的能力集合，也就是脚本能调用的宿主函数表。它由 `tower-agent-plugin-sdk` 定义、由 `CodemodeHost` 实现，并被沙箱以 host import 的形式暴露。
3. **宿主层**：权限、会话、存储、提示词、以及与插件注册表 / 工具集 / UI 状态的接线。

> 关键点： **统的是能力层与提示词，不是执行模型。** 不要求 monty 和 JS 用同一种 host import 机制（一个是挂起 + resume，一个是同步调用）；只要求它们面对脚本时看起来是同一个"标准库"。

### 0.4 解释器可选

| 维度     | `monty`（Python 子集）                                        | `rquickjs-sandbox`（JavaScript，rquickjs）          |
|----------|---------------------------------------------------------------|-----------------------------------------------------|
| 语言     | Python 子集                                                   | JavaScript（含 ES2020 左右特性）                    |
| 执行模型 | 宿主驱动状态机：`function-call` / `os-call` 挂起后 `resume-*` | 同步 host import + guest 内 JS 门面（Promise 包装） |
| 现状     | 已有工具注册 / UI / 状态 / 挂载 / 权限 / 热重载               | 已有命令、文件、工具、状态、日志与 UI 能力          |
| 适合     | "改 agent 行为"：写小函数、注册工具、挂钩子                   | "跑一次复杂任务"：组合命令与文件操作、数据处理      |
| 短板     | Python 子集表达能力有限                                       | 暂无工具钩子；脚本注册工具尚未同步到 CLI 工具目录   |

目标： **两者都补齐到同一张能力表**，差异只体现在提示词与语言特性上。

引擎选择优先级固定为：**脚本元数据 `language` > `--codemode-engine` >
有效配置 `codemode.engine`（项目覆盖用户） > 默认 `js`**。CLI 使用
`--codemode-engine monty|js`；配置可用
`--config-set codemode.engine=monty --config-scope project` 写入。脚本声明始终
覆盖进程级默认值。

### 0.5 提示词由「引擎 + 能力集」生成

同一个 codemode 工具，给 LLM 的说明按运行时配置生成，而不是写死：

```
Codemode: 写一段 <Python | JavaScript> 脚本完成任务，脚本在沙箱里执行。

可用命令（coreutils-wasm）：
  cat ls wc sort head tail grep cut uniq tr mkdir
  用法：await cat("/data/a.txt") / 链式 sort("x").head(3)   ← JS 引擎
        cat("/data/a.txt") / 同名函数                        ← Python 引擎

可用工具（宿主工具，需权限）：read / write / edit / grep / find / ls / shell / codemode …
  用法：tool_call("read", {"path": "src/main.rs"})

扩展点（可注册，需权限）：
  tool_register(name, function, description, parameters)  —— 给 agent 增加一个工具
  on("tool_call", handler)                                —— 在工具执行前放行 / 改写 / 拦截
  on("tool_result", handler)                              —— 改写工具结果
  ui_status / ui_panel / ui_tool_render                   —— 前端无关的 UI 贡献

挂载：/work（只读，工作目录）、/data（可写，会话根）
限制：单次 30s / 128 MB / 无网络
```

要点：

- **能力表就是提示词的来源**：引擎注册了哪些 host function、管理员开了哪些权限、挂了哪些目录，提示词就写哪些。`PromptBuilder` 是 `CapabilitySurface` 的投影，不另维护一份文档。
- 引擎差异只影响"语法示例"段落（Python / JS 各一段）。
- 与 pi 的做法一致：工具描述 + instructions 由宿主生成，模型据此写脚本。

**已落地**：`tower-agent-codemode-core::PromptBuilder` 接收 `ScriptLanguage`、能力集合、
`PromptMount` 挂载表和 `PromptLimits`，生成 `PromptDocument { tool_description, instructions }`。
快照位于 `crates/tower-agent-codemode-core/tests/snapshots/`，JavaScript / Python 共用一个能力
模型，仅语法示例分支不同。

### 0.6 扩展点注入（落在 `tower-agent-plugin-sdk`）

脚本能"改 agent 行为"，靠的是宿主把扩展点暴露成 host function。扩展点清单、与 pi extensions 的逐条对照、以及"哪些点现在还没有"都在 [`sdk.md`](./sdk.md)。这里只给结论：

- **扩展点定义在 `tower-agent-plugin-sdk`**（已定，见 0.9 Q7）： **不新增 `tower-agent-sdk` crate**，作者 API 直接长在现有的 plugin-sdk 上；Rust 插件与沙箱脚本用同一套名字与语义（`registerTool` ↔ `tool_register`，`pi.on("tool_call")` ↔ `on("tool_call", …)`）。
- **脚本侧的扩展点受权限约束**：`ModPermissions` 扩成 `ScriptPermissions`，每一种扩展点一个开关（`tools` / `register_tools` / `hooks` / `commands` / `ui` / `state` / `fs` / `exec`）。
- **扩展点的数量受运行时的钩子面限制**：现在 runtime 只有 10 个 `AgentEvent`（纯通知）+ `BeforeToolCallLayer` / `AfterToolResultLayer` / `CompactionHook`。pi 有 41 个事件且多数可 transform / block。所以"脚本改行为"的第一步是 **把 runtime 的事件面补到够用**（详见 `sdk.md` §3 的缺口列）。

### 0.7 迁移计划（提案，按可独立验收的粒度切）

| 阶段 | 内容                                                                                                                                                                                                                                                                         | 验收                                                                                                 |
|------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------|
| P1   | **统一能力层**：抽出 `CapabilitySurface`（exec / fs / tool_call / tool_register / state / log / ui）到新的内部 crate `tower-agent-codemode-core`（引擎无关，避免 monty ↔ codemode 依赖成环）；monty 接上 `exec`（coreutils-wasm）；JS 引擎接上 `tool_call` / `tool_register` | 两个引擎各有一组同样的能力测试                                                                       |
| P2   | **统一提示词 + 引擎选择**：`PromptBuilder` 由 `CapabilitySurface` 生成；引擎**全局默认 + 脚本可覆盖**（Q1）：`--codemode-engine monty\|js` 定进程默认，脚本头声明语言时覆盖它                                                                                                | 同一任务在两种引擎下都能跑通；提示词随能力/引擎变化                                                  |
| P3   | **合并 feature / CLI**：feature 已收敛为 `codemode`（共享层）+ `codemode-python` / `codemode-js`（Q2）；CLI 开关与 `mod_*` 作者工具改名仍待后续任务                                                               | 无默认只留内置工具；Python / JS 可分别启用，也可同时启用                                               |
| P4   | **统一会话与存储**：`ScriptStore` 已落 storage/Turso；作者工具读写 storage；CLI 支持显式导出/导入，默认项目目录为 `.tower-agent/codemode/`                                                                        | 会话可列出 / 复用 / 清理；换引擎不换心智模型                                                         |
| P5   | **接扩展点 v0**：`tower-agent-plugin-sdk` 的扩展点 = **工具注册 + `tool_call` / `tool_result` 钩子**（Q4）；codemode 把 host function 映射过去                                                                                                                               | 一个脚本能注册工具并在下一轮生效（与现在 monty 等价）+ 能在工具执行前改写/拦截、执行后改写结果       |

兼容性：

- **名字**：两个引擎 crate 都保留（Q2）；`tower-agent-monty` 是 monty/Python 引擎、`tower-agent-codemode` 是宿主 + JS 引擎（或把 JS 引擎拆成 `tower-agent-quickjs`）；对外只有 `codemode`。
- **`mods` 这个词**：降级为"codemode 的持久化脚本"这一子概念；CLI 的旧 `--no-mods` / `TPA_NO_MODS` 已删除，统一由 `--no-codemode` 关闭 codemode。
- 不保留两套会话目录；统一到 `<config>/tower-agent/codemode/{scripts,sessions}/`。

### 0.8 影响面

- crate：`tower-agent-codemode`（宿主，吃掉 monty 的宿主职责或改为引擎适配器）、`tower-agent-monty`（Python 引擎）、`tower-agent-wasmtime`（不变）、 **新增内部 crate `tower-agent-codemode-core`**（能力层 / 提示词 / 脚本模型，引擎无关）、 **扩展 `tower-agent-plugin-sdk`**（不新增对外 SDK crate）。
- feature：`tower-agent` 门面 / `tower-agent-cli` 已收敛为 `codemode` + `codemode-python` + `codemode-js`；
  默认三者都开，`mods` feature 已删除且没有兼容别名。
- CLI：`--codemode` / `--no-codemode` / `--codemode-engine` / `--codemode-session*`；旧
  `--no-mods` 与 `TPA_NO_MODS` 不再识别。
- 文档：`docs/codemode.md`（本文件，v2 提案 + JS 细节）、`docs/sdk.md`（扩展点设计，落在 plugin-sdk）、`docs/plugins.md`（§4.3 / §16 需改：JIT 形态与 codemode 的关系）、`docs/design.md`（§5 CLI 段落与待办）。
- 测试：能力层的"双引擎一致性"测试（同一脚本语义在两种语言下等价）、提示词快照测试、权限矩阵测试。

### 0.9 已确认决策（2026-10-17）

| #  | 问题                                   | 结论                                                                                                                                |
|----|----------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------|
| Q1 | 引擎选择粒度                           | **(c) 两者都支持**：进程级默认引擎（`--codemode-engine` / 配置项 `codemode.engine`）+ 脚本可声明语言覆盖它                          |
| Q2 | 两个引擎是否都保留                     | **(a) 都保留**：monty/Python 与 rquickjs/JS 都是正式后端                                                                            |
| Q3 | `mods` 词汇是否彻底消失                | **(a) 彻底消失**：不再有顶层 `mods` 模式；`mod_persist` 等降级为 codemode 的"持久化脚本"子概念；CLI / 环境变量 / feature 名一并改名 |
| Q4 | 脚本侧扩展点 v0 范围                   | **(b) 工具注册 + 工具前后钩子**：`tool_register` / `tool_call` + `on("tool_call")` / `on("tool_result")`                            |
| Q5 | 权限模型                               | **沿用并扩展**：`ModPermissions` → `ScriptPermissions`，增加 `exec` / `hooks` / `commands` 等开关；不做"能力名 + 资源"两级大改      |
| Q6 | 脚本仓库形态                           | **(c) storage 为主 + 导出/导入**：SQLite 行是真源，文件是显式导出的投影（详见 0.10）                                                |
| Q7 | `tower-agent-sdk` 与 plugin-sdk 的关系 | **合并到 `tower-agent-plugin-sdk`**：不新增 crate，扩展点作者 API 直接加在 plugin-sdk 上（见 `sdk.md` §2）                          |

### 0.10 Q6 决策说明：为什么是 (c)

三条路线的形态：

| 路线                         | 真源在哪                                 | 作者怎么改                             | 人类可读 / git      | 多前端共享                            |
|------------------------------|------------------------------------------|----------------------------------------|---------------------|---------------------------------------|
| (a) 每脚本一条 storage 记录  | SQLite 行（新增一个 `ScriptStore`）      | 走工具 / API（`script_write`）         | ❌ 要导出才能看     | ✅ 现成（`tower-agent-storage` 抽象） |
| (b) 每脚本一个文件           | `<config>/tower-agent/codemode/*.py\|js` | 编辑器 / `script_write` 都行           | ✅ 天然             | ⚠️ 每个前端都要知道路径约定 + 文件锁  |
| (c) storage 为主 + 导出/导入 | SQLite 行为真源，文件是投影              | 工具 / API 写库；人类改文件后 `import` | ✅（显式导出/导入） | ✅                                    |

判断前先回答两个子问题（它们直接决定选哪条）：

1. **脚本的主要作者是谁？** LLM 用工具写 → (a)/ (c) 顺手； (b) 会让"工具写文件"变成又一条写路径（还得处理半写状态、外部编辑触发 reload）。
2. **脚本需要提交进项目仓库、与人协作 review 吗？** 需要 → (b) 最自然（`.tower-agent/codemode/*.py` 直接进 git）；不需要 → (a)/ (c) 够。

与既有决策的张力：

- 前面已经定过"**配置一律走 `tower-agent-storage`，不能走别的路子**"（CLI/axum/webui 共用同一套存储抽象）。脚本更接近"用户资产"而不是"运行时产物"，按同一条原则应该进 storage。
- Python codemode 脚本已迁到 `ScriptStore`：`codemode_write` 默认写当前项目作用域，`scope=user` 可写用户级；启动时项目同名记录覆盖用户记录。
- 无论哪条， **脚本元数据（语言、权限、启用状态、源码哈希、最近运行）都应该进 storage**——它是权限矩阵与热重载校验的依据，JSON 侧车文件只会制造两处真相。

**结论 (c)**：storage 是真源（与"配置走 storage"一致、多前端零成本、LLM 写入路径天然），文件用于 **显式导出 / 导入**（满足"人类要读、要 git、要 review"）。`ScriptStore` 已实现 `(scope_kind, scope_id, name)` 主键与 project/user 隔离；确实需要进仓库时，导出到 `.tower-agent/codemode/` 再纳入 git。

落地形状：

1. `tower-agent-storage` 增加 `ScriptStore` 子接口（与 `ConfigStore` / `HistoryStore` 并列）：脚本 id、名字、语言（`monty` / `js`）、作用域（user / project）、源码、启用状态、权限、元数据、时间戳。
2. `codemode_write` / `codemode_reload` / `codemode_list` / `codemode_remove` 直接读写 `ScriptStore`；`codemode_persist` / `codemode_discard` 作为启用保存与删除的兼容入口。
3. `codemode export <dir>` / `codemode import <dir>`：每个脚本一个文件（源码 + 元数据头）；项目级默认导出到 `.tower-agent/codemode/`。
4. 脚本的 **元数据永远在 storage**（权限、源码哈希、最近运行），文件只是投影——避免两处真相。

权限执行顺序：

1. `ScriptRecord.permissions` 是持久预授权集合；允许的能力直接执行，不提示。
2. 未持久授权的能力在首次调用时进入 `PermissionPolicy`：`AllowOnce` 仅本次通过，`AllowSession` 按会话缓存，`Deny` 拒绝。
3. 无人值守模式（`--print` 或没有注入 prompter）不提示，直接返回结构化的 `unauthorized` 错误。
4. monty 与 JS 共用同一个 Session 策略实例；前端通过 `Session::with_permission_policy` 注入确认 UI。

脚本 hook wire 契约：

- `tool_call` 收到 `{ "name": string, "arguments": object }`。
- 返回 `null` / `true`：放行；返回 `false` 或字符串：拦截并作为错误结果。
- 返回 `{ "action": "block", "message": string }`：拦截；返回 `{ "action": "allow", "arguments": object }`：改写入参后继续。
- `tool_result` 收到 `{ "name", "arguments", "text", "is_error" }`。
- 返回 `null`：不改；字符串或 `{ "text": string, "is_error"?: bool }`：替换模型可见文本/错误状态。
- hook 执行错误、超时或取消会转成失败的工具结果。多个脚本按 owner 顺序执行；当前 v0 将每个脚本的同一事件视为一个可替换 registration。

导出格式（v1）：

```text
--- tower-agent-codemode v1
{
  "version": 1,
  "id": "...",
  "name": "example",
  "language": "monty",
  "scope": { "Project": "/abs/workspace" },
  "enabled": true,
  "permissions": { "...": true },
  "metadata": { "requires": [] },
  "created_at": 0,
  "updated_at": 0,
  "source_hash": "0123456789abcdef"
}
---
<原始脚本源码>
```

- `--codemode-export [dir]` 默认只导出当前 project 作用域；省略目录时写 `<cwd>/.tower-agent/codemode/`。`--codemode-script-scope user` 可导出用户级。
- `--codemode-import [dir]` 默认按文件头中的 scope 恢复；`--codemode-script-scope project|user` 可覆盖全部文件的目标作用域，便于项目迁移。
- 每个脚本一个 `<安全名>-<名字哈希>.codemode` 文件。导出会清理目标目录中属于该格式、但本次已不存在的旧文件，不碰其它 `.codemode` 文件。
- 冲突策略固定为 **同名同作用域覆盖（upsert）**。导入前会校验格式版本和源码哈希；哈希不匹配时报错，不静默修改文件。

不再考虑 (b) 作为真源：它会形成"第二套配置写入路径"，与"配置一律走 storage"的既定原则冲突。

---

## 附录：v1 细节（JS 引擎 · rquickjs）

> 以下 §1–§11 是 v1 的原文，描述 **JS 引擎**那一部分的设计与实测结论；下面的状态表只覆盖这部分进度。

| 项目     | 值                                                                                                                                                   |
|----------|------------------------------------------------------------------------------------------------------------------------------------------------------|
| 状态     | v0.6 草案。**Q1–Q11 已定**（含组合 R1 + Q2(a)，权威表见 §8）；M0–M5 + CLI 接线 + M7 第一层已完成，下一阶段见 §9（M6：并发走 ticket + join，不做 p3） |
| 日期     | 2026-10-11                                                                                                                                           |
| 关联     | [`design.md`](./design.md)、[`plugins.md`](./plugins.md)、`crates/tower-agent-wasmtime`、`crates/tower-agent-monty`（同构样板）                      |
| 本地引用 | `~/Projects/DelSkayn/rquickjs`、`~/Projects/awslabs/llrt`、`~/Projects/rquickjs/rquickjs-extra`、`~/Projects/rquickjs/rquickjs-serde`                |

## 1. 目标与非目标

codemode：让 LLM 用 **一段 JavaScript** 在一次工具调用里完成多步「文件 + 命令」操作，只把最终结果带回上下文。

目标：

1. 脚本与它执行的命令 **都在 wasm 沙箱里**：内存隔离由 wasmtime 提供，可见路径由 WASI 能力决定，能跑哪些命令由我们给的 host import 决定。
2. 三种能力可枚举、可限制、可审计： **内存 / 时间 / 文件系统可见范围**，外加「执行了哪些命令」的流水。
3. 命令复用成熟实现：uutils coreutils 的 multicall 组件（M0 已完成，见 §3.5），不自研内置命令。
4. 与 monty 互补：monty = 插件 / codemode Python 脚本的解释器路径；codemode = 面向文件与命令的脚本路径。

非目标（本阶段）：

- 不做完整 Node 运行时：不做网络（`net`/`tls`/`http`/`fetch`/`dns`）、不做 `worker_threads`、不做 npm 生态。
- 不做 bash 兼容、不解析 shell 语法（是否要 `sh()` 见 §8 Q4）。
- 不做 guest 内并发（`Promise.all` 式的并行命令；受 p2 限制，见 §6）。
- 不替换 `tower-agent-tools` 的 `shell` 工具（它仍是宿主直连、面向交互）。

## 2. 架构

### 2.1 两个组件 + 一个宿主

```
LLM
 │  codemode 工具调用：{ code, timeout_ms?, cwd?, mounts? }
 ▼
┌──────────────────── 宿主进程（tower-agent）───────────────────────┐
│ codemode 工具层：参数校验 / 权限 / 超时 / 截断 / 审计             │
│        │                                                          │
│        ▼  wasmtime（tower-agent-wasmtime：引擎 / 池 / JIT 缓存）  │
│  ┌──────────────────────┐      host import: exec / fs             │
│  │ JS guest 组件        │──────────────────────────────┐          │
│  │ wasm32-wasip2        │                              ▼          │
│  │ rquickjs + 我们的 API│                    ┌──────────────────┐ │
│  │  └─ 脚本在这里跑     │                    │ 命令层：uutils   │ │
│  └──────────────────────┘                    │ multicall 组件   │ │
│           │                                  │ (cat…sort, 11)   │ │
│           │ wasi:filesystem                  └──────────────────┘ │
│           ▼                                           │           │
│   会话文件系统（§5：真实目录 / 内存 VFS / overlay）◄──┘           │
└───────────────────────────────────────────────────────────────────┘
```

三条边界，职责不重叠：

| 边界          | 决定什么                                     | 由谁实现                                         |
|---------------|----------------------------------------------|--------------------------------------------------|
| wasm 内存隔离 | 脚本崩了/越界了也伤不到宿主                  | wasmtime                                         |
| WASI 能力     | 脚本与命令**能看见哪些路径**                 | preopen 或宿主 shadow 的 `wasi:filesystem`（§5） |
| host import   | 脚本**能发起哪些操作**（跑命令、读写字面量） | 我们的 host 函数 + 命令注册表                    |

### 2.2 为什么脚本放 guest 也够用

关键点： **JS 不需要 WASI 提供进程与管道**。

| shell 脚本依赖的机制 | codemode 里的形态                             |
|----------------------|-----------------------------------------------|
| 管道 `a \| b`        | JS 组合：`await cat("a.txt").sort().head(3)`  |
| 命令替换 `$(…)`      | 普通赋值：`const n = await wc("-l", "a.txt")` |
| 外部命令             | host import `exec`（宿主起命令组件）          |

组合发生在 **语言内部**，命令由 **宿主动词**提供。所以在 guest 里跑 JS，不需要 WASI 的 pipe，也不需要进程派发——这正好绕开了 WASI 的表达力缺口。

### 2.3 这个选择的取舍（要认账的）

好处：

- **强隔离**：脚本是 wasm 代码，内存安全边界与 monty 一致；崩溃不会带倒宿主。
- **边界统一**：脚本与命令在同一个沙箱模型里，同吃 preopen / VFS。
- **llrt 的损失有限**：`llrt_fs`/`llrt_child_process` 本来也不能用（一个直落宿主真实文件系统、一个真派发宿主进程），而这两者正是我们要 **收窄**的能力。

代价：

- **Node API 要我们自己实现**：能复用的第三方模块只剩「无 tokio」那一小撮（见 §3.2/§3.3），LLRT 的价值降级为「API 形状参考」。
- **guest 里没有 tokio**：异步靠 rquickjs 自带的 executor，首版干脆用同步 host import（§4.3）。
- **构建要多一套工具链**：rquickjs 的 wasi 构建需要 wasi-sdk（§3.1），本机当前没有。

## 3. 调研结论

标注： **[实测]** 本机跑过； **[源码]** 读本地源码得到的事实； **[判断]** 基于前两者的推断，未验证。

### 3.1 rquickjs 0.14.0 —— guest 引擎（`~/Projects/DelSkayn/rquickjs`）

**[源码]** 支持矩阵（README）：原生各平台、`wasm32-wasip1`、`wasm32-wasip2`、`wasm32-unknown-unknown` 全部 ✅（ **没有 `wasm32-wasip3`**）。CI 里有 `wasm32-wasip2` + nightly + `full-async-wasi` 的测试项（`.github/workflows/ci.yml:217`）；`full-async-wasi = ["full-wasi", "futures"]`。

**[源码]** 我们要的资源控制（`core/src/runtime/base.rs`）：

| API                              | 用途                      |
|----------------------------------|---------------------------|
| `Runtime::set_interrupt_handler` | 脚本总预算（超时中断）    |
| `Runtime::set_memory_limit`      | JS 堆上限                 |
| `Runtime::set_max_stack_size`    | 栈上限（wasm 上**必设**） |
| `Runtime::set_gc_threshold`      | GC 阈值                   |
| `Runtime::memory_usage`          | 用量观测                  |

**[源码]** 构建：

- wasi 目标：`sys/build.rs` **用 curl 下载 wasi-sdk 24.0**，并 `--sysroot=<sdk>/share/wasi-sysroot`；可用 `WASI_SDK=<path>` 指向本地 sdk，或 `RQUICKJS_SYS_NO_WASI_SDK=1` 跳过。→ CI/离线构建要预置，或 vendor 一份。
- 原生构建：`cc` 编 `sys/quickjs`，无外部依赖（宿主侧若要跑同一份代码做单测很方便）。

**[源码] wasm 上必设栈上限**：`JS_DEFAULT_STACK_SIZE` = 1 MiB，正好等于 wasm shadow stack 大小，默认 `stack_top - 1MB` 会回绕，栈溢出检测静默失效 → 深递归从「可捕获的 `RangeError`」变成「硬 trap」。wasm 目标上必须 `runtime.set_max_stack_size(256 * 1024)`。

**[实测] 已跑通（M1 第一步）**：rquickjs 0.14 编到 `wasm32-wasip2` 并在 wasmtime 上跑起 JS。构建前置三项：

1. **wasi-sdk**：本机在 `~/.cache/wasi-sdk-34.0-x86_64-linux`（`bin/clang`、`bin/ar`、`share/wasi-sysroot` 齐备）；用 `WASI_SDK=<该目录>` 即可，build.rs 里那个「下载 wasi-sdk 24」的分支不会走（SDK 34 也能用）。
2. **quickjs-ng 子模块**：`sys/quickjs` 是 submodule（`github.com/quickjs-ng/quickjs`），未初始化时构建会 `Unable to copy source; try 'git submodule update --init'`。本次是在 `/tmp` 的副本里 `git submodule update --init --depth 1`（pin `6d46d07`）—— **不改本地仓库**。
3. 命令：`WASI_SDK=… cargo build --release --target wasm32-wasip2`（rustc 直出组件），产物 **1.26 MB**。

**[实测] 子模块/vendor 问题不存在**： **crates.io 上的 `rquickjs` 0.14 直接可用**——`rquickjs-sys` 自带那 4 个 C 文件（`quickjs.c` / `libregexp.c` / `libunicode.c` / `dtoa.c`），只用 `WASI_SDK` 就能编过并跑通。所以 `wasm/rquickjs-sandbox` 直接依赖 registry 版本， **不需要 quickjs-ng 子模块，也不需要 vendor**（上面第 2 条只在用 path 依赖时才需要处理）。

跑起来的结果：`1 + 2 * 3` → `7`；`JSON.stringify({…,"中文"})` 正确；`[3,1,2].sort().join("-")` → `1-2-3`；`throw` 变成 Rust 侧 `Err`。 **不需要 feature gate，也不需要 wasm-opt。**

**[实测] 但递归会 trap —— 上游推荐的 256 KiB 反而更糟**：用 `set_max_stack_size(256 * 1024)`（README 建议值）时，深递归（~1000 层） **直接 `wasm trap: call stack exhausted`**，整个实例报废，而不是抛可捕获的 `RangeError`；改成 **64 KiB** 后，同样深度变成 `caught:RangeError`（实测 200 层 ok、400 层起稳定可捕获）。原因是 wasm shadow stack 只有 1 MiB、QuickJS 每个 JS 帧约 1 KiB，限额报的是「相对某个 base 的值」，256 KiB 落在真实耗尽点之上，检查来不及触发。

→ 结论： **`set_max_stack_size` 要取小值（首版 64 KiB）**，让栈溢出保持「可捕获」；同时宿主侧 **仍必须把 wasm trap 当成脚本失败处理，并丢弃该实例**（trap 之后实例不可复用）。

**[实测] 但「可捕获」只在独立跑时成立**：同一段代码放进 **组件**里（rquickjs 编到 `wasm32-wasip2`、宿主用 wasmtime 同步调用）后，64 KiB 限额下深递归 **仍然是硬 trap**（`wasm trap: call stack exhausted`）——组件里 JS 的栈检查来不及在 wasm 影子栈耗尽前触发。所以宿主必须把 trap 当「脚本失败」：`tower-agent-codemode` 就是这么做的（trap → `CodemodeError::Script`；每次 `run` 新建 `Store`，不污染后续调用，冒烟里有断言）。

**另注（宿主线程栈）**：wasmtime 的 **同步**执行用的是 **调用方线程**的栈，所以 `max_wasm_stack`（codemode 取 1 MiB）必须明显小于宿主线程栈——测试线程默认只有 2 MiB，深递归用例会先把原生栈压爆（实测 SIGABRT）。宿主要把 codemode 跑在栈够大的线程上。

### 3.2 llrt_modules 0.9.0-beta（`~/Projects/awslabs/llrt`）

**[源码]** 结构：聚合 crate `llrt_modules`（`src/module_builder.rs` 用 `with_module` / `with_global` 装模块），模块在 `modules/llrt_*`，共享库在 `libs/llrt_*`，每个模块一个 optional feature。

**[源码]** 依赖面：`tokio` + `simd-json` + `tracing` + `llrt_utils`（`llrt_utils` 自己也依赖 `tokio/sync`）。`llrt_fs` = `tokio::fs`（宿主真实文件系统， **无 root jail**）；`llrt_child_process` = `tokio::process`（真派发宿主进程）。

**[实测] tokio 在 wasm 上直接禁掉了我们最想要的那两个模块**：tokio 1.53.2 在 `target_family = "wasm"` 下 `compile_error!("Only features sync,macros,io-util,rt,time are supported on wasm.")`；而 `llrt_fs` 的 tokio feature 是 `["fs","io-util","rt"]`、`llrt_child_process` 是 `["process"]` → **这两个模块在 wasm32-wasip2 上根本编不过**。

**[源码]** 按「`[dependencies]` 里的 tokio feature 是否落在允许集合内」重新分类：

| 分类                                        | 模块                                                                                                                                                                                                                                | 依据                                                                           |
|---------------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------|
| 可直接进 guest（`[dependencies]` 无 tokio） | `llrt_path` `llrt_buffer` `llrt_util` `llrt_string_decoder` `llrt_url` `llrt_os` `llrt_events` `llrt_assert` `llrt_console` `llrt_exceptions`；共享库 `llrt_utils`（sync）`llrt_encoding` `llrt_json` `llrt_hooking` `llrt_context` | 各 `Cargo.toml` 的 `[dependencies]`（早先看到的差异来自 `[dev-dependencies]`） |
| 能编，运行行为待验                          | `llrt_stream`（macros,io-util）、`llrt_timers`（macros,sync,time）                                                                                                                                                                  | feature 在 tokio 允许集合内，但需要有人驱动 timer / driver                     |
| **编不过（tokio 禁用）**                    | `llrt_fs`（fs,io-util,rt）、`llrt_child_process`（process）                                                                                                                                                                         | 实测 `compile_error`                                                           |
| 不进（网络/重型）                           | `net` `tls` `http` `https` `fetch` `dns` `dgram` `crypto` `intl` `temporal` `zlib` `tty`                                                                                                                                            | 与 codemode 无关                                                               |

**[源码] 好消息**：`llrt_fs` / `llrt_child_process` 是 **叶子**——没有任何模块或共享库依赖它们（只有聚合 crate `llrt_modules` 引用），所以「排除这两个、其余不 fork 直接用」不会传染。

**[判断]** 于是 LLRT 的定位从「只能做形状参考」修正为： **可以做 guest 的模块底座（除了 `fs` / `child_process`），而且不用 fork**。而这两个恰好正是本该由我们控制后端的模块。`llrt_path` / `llrt_encoding` / `llrt_buffer` 这类纯逻辑件原样复用。版本是 `0.9.0-beta`。

#### 白名单（已定，Q6）

| 层级          | 模块                                                                                                                                                                                                              | 理由                                         |
|---------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|----------------------------------------------|
| **M1 就上**   | `llrt_path` `llrt_buffer` `llrt_encoding`(lib) `llrt_utils`(lib)                                                                                                                                                  | 路径语义、Buffer 与编码转换——codemode 的刚需 |
| **M3 顺手加** | `llrt_util` `llrt_string_decoder` `llrt_events` `llrt_assert` `llrt_exceptions`                                                                                                                                   | 格式化 / 流式解码 / 事件基类，成本低         |
| **不进**      | `llrt_os`（报告宿主信息：沙箱里要么给假值要么泄露宿主信息）、`llrt_console`（`console.log` 由我们收进 `run-result.stdout`，更可控）、`llrt_url`（用不上）、`llrt_stream` `llrt_timers`（能编但需要 driver，暂缓） |                                              |
| **绝不**      | `llrt_fs` `llrt_child_process`（guest 编不过）、`net` `tls` `http` `https` `fetch` `dns` `dgram` `crypto` `intl` `temporal` `zlib` `tty` `module`                                                                 |                                              |

### 3.3 rquickjs-extra 0.3.0（`~/Projects/rquickjs/rquickjs-extra`）

**[源码]** 模块与 tokio 依赖：`console`（无）、`url`（无）、`os`、`timers`（`tokio/sync+time`）、`sqlite`。license Apache-2.0，edition 2024。

**[判断]** guest 首选 `console` / `url`（干净）；`timers` 用的是 tokio 允许的 wasm 子集（sync/time/macros）， **能编但要有 driver 才跑得起来**，按需实测；`sqlite` 与 `os` 不进 guest。

### 3.4 rquickjs-serde 0.8.0（`~/Projects/rquickjs/rquickjs-serde`）

**[源码]** serde ↔ `rquickjs::Value` 双向，Apache-2.0，纯 Rust 无 tokio。 **guest 可用**：用来把 Rust 结构体（`ExecResult`、`StatResult`、`run` 的返回）直接变成 JS 对象。

### 3.5 uutils/coreutils（M0，已实测）

**[实测]** uutils `0.12.0` → wasip2 multicall， **上游源码零改动**：

- 自建 53 行 wrapper crate（依赖 `uucore` + 11 个 `uu_*`），`cargo build --release --target wasm32-wasip2` 直出组件， **6.37 MB**（`wasm-tools strip` 只省 9 KB）。11 个 util = `cat head tail ls wc cp mv rm mkdir touch sort`，全部实测通过。
- 上游 0.12.0 已原生处理 p2（`uucore` 的 `std::os::wasi::ffi` 被限定到 `target_env = "p1"`，p2 走 `as_encoded_bytes()`），所以 **不需要** `#![feature(wasip2)]`（`uu_env` 例外）。
- **flush 补丁不需要**：`std::process::exit` 在 stable 1.99 / nightly 1.101 上都 flush（有无 flush 输出逐字节相同）。
- **cwasm 预编译必需**：否则每次 spawn ~141 ms（预编译后 ~8 ms），flags 必须与 child engine 一致。

#### 工具集扩容（M0.1，已实测）

**[实测]** 把 wrapper 从 11 个 util 扩到 **60 个**，`cargo build --release --target wasm32-wasip2` 一次编过、 **仍然零补丁**：

| 组          | util                                                                                                    |
|-------------|---------------------------------------------------------------------------------------------------------|
| 文本        | `cat head tail tac nl fold expand unexpand od cut tr uniq comm join paste split shuf tsort sort wc seq` |
| 文件 / 路径 | `ls cp mv rm mkdir touch mktemp dirname basename realpath readlink ln link unlink truncate`             |
| 杂项        | `true false echo printf test expr date sleep tee yes pwd numfmt factor printenv`                        |
| 校验 / 编码 | `md5sum sha1sum sha256sum sha512sum b2sum cksum sum base64 base32 basenc`                               |

冒烟实测（preopen 到 guest 根）：`cut/tr/sort/uniq/seq/printf/test/sha256sum/base64/dirname/basename/realpath/mktemp/readlink/date/shuf/od/tee/ls -l` 全部正确； **stdin 也能用**（宿主管道喂给 guest）；`ls -l` 里 uid/gid 是宿主给的假身份。

体积： **11 MB**（60 util，未优化；`wasm-opt` 对组件无效）。相对 11 util 的 6.37 MB 涨得不少 → 后续按 §6 拆组件（核心集 + 扩展集）或压 profile。

**扩容的规则（重要）**： **碰 `std::os::wasi` 的 util 在 p2 上编不过**，实测两个：

- `du` → `MetadataExt::dev()/ino()`（硬链接判重）；
- `env` → `std::os::wasi::ffi`（OsStrExt，§3.2 提过）。

两者都要 `#![feature(wasip2)]` 且该属性必须写进 **那个 crate 的 crate 根**。而且 **不能用 `RUSTFLAGS=-Zcrate-attr=feature(wasip2)` 绕过**：`wasip2` 是 **std 的 feature gate**，注入到图里的 `#![no_std]` 依赖（`cfg-if` / `typenum` / `smallvec` / `litemap` / `utf8_iter` / `stable_deref_trait` / `wasi` …）会直接 `error[E0635]: unknown feature 'wasip2'`（实测；单独编一个带 std 的 crate 才不报）。

→ 规则： **新增 util 先编一遍**；命中这类 API 的，二选一： **不收**，或给它那 **一个 crate** 加一行 attribute（改动面 1 行/1 crate，不 fork 整棵 coreutils 树）。`stat` 目前也不在离线缓存里，需要单独确认。

**已落地**：`wasm/coreutils-wasm`（独立 workspace，同 monty-sandbox 形态；名字里没有 sandbox—— **它是被宿主调用的 wasm 工具，不是沙箱**，沙箱是 `wasm/rquickjs-sandbox`）。`./generate.sh` 只依赖 `wasm32-wasip2` target—— **不需要 wasi-sdk / wasm-opt / wasm-tools**（rustc 直出组件）；产物 `dist/coreutils.wasm`（工具名 `coreutils`；`dist/` 不入库）。加命令 = `Cargo.toml` + `src/main.rs` 各一行。

**[实测] profile 有效**：`opt-level="z" + lto=true + strip=true` 把裸构建的 11 MB 压到 **7.4 MB**（`wasm-opt` 处理不了组件，所以这是唯一的压缩手段）。冒烟全过：`argv[0]` 分发、`--version` 里 `util_name()` 正确、未知命令退出码 127、无尾换行文件的尾巴不丢。

另：`ln -s`（符号链接）在 guest 里报 `Permission denied`，硬链接正常（§6 坑 13）。

### 3.6 既有设施

**[源码]** `tower-agent-wasmtime`：`SandboxEngine` / `Sandbox` / `SandboxPool` / `HostState` / `SandboxOptions`（`DEFAULT_MEMORY_LIMIT = 256 MiB`）。现状 **只开 p2**、 **默认无 preopen**、 **无 epoch/fuel 超时**。

**[源码]** `tower-agent-monty` + `wasm/monty-sandbox` 是现成样板：独立 workspace 产出组件 → 内嵌 `assets/*.wasm` → 宿主 `bindgen!` 绑定 + 池化 + 预热 + JIT 缓存 + `run(...)` 导出。codemode 的 guest 组件照这个形状做。

## 4. JS guest 组件设计

### 4.1 组件形状

- **两个**独立 workspace，都放在 `wasm/` 下，照 `wasm/monty-sandbox` 的形态（不进主 workspace，`generate.sh` 一条链路出组件）：
    - `wasm/rquickjs-sandbox`：rquickjs + 我们的 API 层 → JS guest 组件；
    - `wasm/coreutils-wasm`：uutils wrapper crate（§3.5）→ multicall 命令组件，产物 `dist/coreutils.wasm`。

  两者都是普通 wasm 组件，由宿主 `crates/tower-agent-codemode` 用 `tower-agent-wasmtime` 驱动。`rquickjs-sandbox` 的导出面（已核对）：import `tower-agent:codemode/host` + `wasi:*@0.2.12`，export `tower-agent:codemode/sandbox`，产物 `dist/codemode.wasm`（ **796 KB**）。

  **两个产物都入库并内嵌**（决定：内嵌）：`generate.sh` 直接写到 `crates/tower-agent-codemode/assets/{coreutils.wasm,codemode.wasm}`，宿主用 `include_bytes!`（`EMBEDDED_COREUTILS` / `EMBEDDED_SANDBOX`）。形状与 monty-sandbox 一致，代价是仓库 +8.2 MB、二进制 +8.2 MB。

  **工具（M2，已落地）**：`CodemodeTool` 实现 `AgentTool`——`into_agent_tool()` 给出 `codemode` 工具，schema = `{code, timeout_ms?}`；结果 `content` 是 stdout（+ `return:` 值），`structured_content` = `{ok, value, stdout, commands[], truncated, duration_ms}`； **命令级进度**（`command-start` + name/args）作为 `ToolResultStream` 的中间项推给 UI，终态结果在最后一项。沙箱跑在工具自带的 32 MiB worker 线程上（wasmtime 同步执行用调用方线程栈）。
- 目标 `wasm32-wasip2`：rustc **直出组件**（不需要 `wasm-tools component new`）。代价是 **`wasm-opt` 处理不了组件**（Binaryen 不支持），体积只能靠 profile（`opt-level="z"` + `lto` + `strip`）压。
- WIT world 草案：

```wit
world codemode {
  export run: func(code: string, opts: run-opts) -> result<run-result, string>;

  record run-opts {
  cwd: option<string>,
  timeout-ms: option<u32>,
  memory-limit: option<u64>,
}
record command-log {name: string, args: list<string>, code: s32, ms: u32 }
record run-result {
value: option<string>, // 脚本 return 值的 JSON
stdout: string, // console.log 收集
commands: list<command-log>,
truncated: bool,
}

import exec: func(name: string, args: list<string>, opts: exec-opts
)
-> result<exec-result, string>;
// 可选：import fs-* （若走 §5 档位 (b)/(c) 的宿主 VFS，见 §4.4）
}
```

### 4.2 JS 可见表面（M3 已落地）

```js
// 1) 命令函数：`COMMANDS` 里的 60 个 coreutil 各一个，返回 thenable 的 Cmd，可链式
const n = await wc("-l", "data/nums.txt");        // await 得 stdout
const top = await sort("data/nums.txt").head(3);    // 链式 = 宿主侧 pipeline
const out = await ls("/data", {l: true});

// 2) 逃生口：任意已注册命令 + 原始 stdio
const r = await exec("sort", ["-n", "a.txt"], {stdin: "4\n2\n"});
// r = { code: 0, stdout: "2\n4\n", stderr: "" }

// 3) 会话文件系统（越界路径直接抛错）
const raw = await fs.readFile("/data/a.txt", "utf8");
await fs.writeFile("/out.txt", JSON.stringify(rows));
const entries = await fs.readdir("/data");

// 4) 返回值：宿主转成 JSON 回给工具层
return {total: rows.length, top: rows.slice(0, 10)};
```

实现要点：

- 原生侧只暴露 **7 个**同步函数（`__exec`、`__readFile`、`__writeFile`、`__readdir`、`__exists`、`__mkdir`、`__rm`），其余（`Cmd` 链式、pipeline、`fs` 门面、`exec` 的 Promise 包装）用随 runtime 预载的 **JS 层**实现——API 迭代不需要重编组件。
- **命令名由宿主注入**：`run-opts.utils`（宿主的 `COMMANDS` 常量）传给 guest，由它生成 `cat()` 与 `Cmd.prototype.cat`——JS 侧不抄命令表；`tower-agent-codemode` 里有个 **漂移测试**直接拿 `wasm/coreutils-wasm/src/main.rs` 的登记表比对。
- **pipeline 语义**：链式 = 宿主侧顺序执行，每段 stdout 作为下一段的 stdin（内存缓冲，非流式）；某段非零退出就中断并交回该段结果。`await cmd` 得 stdout（非零退出抛 JS 异常），`.result()` 得 `{code, stdout, stderr}`（不抛）。
- **API 是 async 形状，实现是同步的**：host import 是同步调用（wasmtime sync linker），JS 侧包一层 Promise，将来换真正的异步 import 时脚本不用改。
- `console.log` 收进 `run-result.stdout`（TUI 可见），不混进命令输出。
- **暂不引入 `rquickjs-serde`**（与早先设想不同）：目前过界的都是 JSON 字符串（返回值、`exec` 结果、`fs` 数据），WIT record 的编解码由 `bindgen!` 负责，serde 只会多一个依赖而不带来功能；等真需要「Rust 结构体 ⇄ JS 对象」的富类型时再引。

### 4.3 host import 接口

```wit
record exec-opts {
stdin: option<list<u8>>,
cwd: option<string>,
timeout-ms: option<u32>,
}
record exec-result { code: s32, stdout: list<u8>, stderr: list<u8>
}

exec: func(name: string, args: list<string>, opts: exec-opts
)
-> result<exec-result, string>;
```

宿主实现：按命令名取组件（11 个名字共用同一份 uutils multicall，argv[0] 分发）→ 起 `Sandbox` → 内存 stdio 灌入/取出 → 返回 code + 输出。首版 **同步**实现（`add_to_linker_sync`），宿主内部阻塞等待；升级为异步 import 需要 p3 / component-model-async，本期不做。

### 4.4 脚本侧 `fs` 的两条实现路线

前提： **`llrt_fs` 进不了 guest**（§3.2，tokio 禁用 `fs` feature），所以脚本侧 `fs` 只能由我们实现。两条路：

| 路线                                      | 做法                                                                                            | 与 §5 档位的关系                   |
|-------------------------------------------|-------------------------------------------------------------------------------------------------|------------------------------------|
| (i) 直接吃 guest 自己的 `wasi:filesystem` | guest Rust 侧用 `std::fs` / `rustix` 读写 preopen 内的路径；脚本侧 `fs` 模块是 guest 内的薄封装 | 对应档位 (a)：preopen 决定可见范围 |
| (ii) 用我们自己的 `fs-*` host import      | 所有读写走宿主 VFS，guest 不拿 WASI 文件能力                                                    | 对应档位 (b)/(c)                   |

推荐： **先 (i)**（零成本、`mv` 等命令的路径语义天然一致），档位上到 (b)/ (c) 时 **不用改脚本**——因为那时宿主 shadow 了 `wasi:filesystem`，guest 的 `std::fs` 自动落到宿主 VFS 上。两条路都 **不 fork llrt**：可复用的是 `llrt_path`（路径语义）、`llrt_encoding`（编码）、`llrt_buffer`（Buffer），只是不用 `llrt_fs`。

## 5. 会话文件系统（Q2：三档位的区别）

三档是 **递进**的，不是三选一的平行方案：每档都在上一档之上加一层能力，代价也逐档上升。

### 档位 (a) 会话目录（真实磁盘）+ preopen

做法：会话开始时在磁盘上建一个目录，把它**作为 guest 的 `/` ** preopen 给 JS guest 与命令组件；脚本里看到的就是这个目录树。

- 能力：脚本与命令读写真实文件，两者天然一致；宿主侧可以直接 `ls` 会话目录排查问题。
- 代价： **有磁盘残留**（会话结束要清理，进程崩溃会留下）；无细粒度权限（只能到「哪些目录 preopen」这一级）； **无法即焚/快照/回滚**；大文件真走磁盘 IO。
- 谁来做：几乎不用写代码，`SandboxOptions` 加 preopen 即可。
- 适合：M1–M3，先跑通。

### 档位 (b) 内存 VFS + linker shadow `wasi:filesystem`

做法：宿主自己实现 `wasi:filesystem`，用 `linker.allow_shadowing(true)` 盖掉 wasmtime-wasi 的默认实现（conch 的 `eryx-vfs` 就是这个思路）。guest（JS guest **和** 命令组件）的所有文件调用都被路由到宿主存储。

- 能力： **零磁盘残留、会话即焚**；权限能细到文件/目录（`DirPerms`/`FilePerms`）；每次读写都过宿主 → 可审计、可 **快照/回滚**；可做 **hybrid**：部分路径在内存、部分路径透传到真实目录。
- 代价：要写一份 **完整的** `wasi:filesystem`（open/read/write/readdir/stat/rename/symlink/时间戳/元数据……），工作量大、边角多；大文件全进内存；wasmtime 升级要跟着维护（conch 记录的坑：p3 + 多 preopen 会 `bad-descriptor`；`File::try_clone` 之类要自己绕）。
- 适合：M4+，当「无残留 / 细粒度权限 / 可回滚」成为需求时。

### 档位 (c) (b) + 只读挂载 / overlay

做法：在 (b) 之上定义挂载语义——宿主目录挂成 **只读**、写入落到 overlay 层、会话结束丢弃 overlay；或临时目录即焚。

- 能力：给 agent 一个「 **能读工作区、写不污染工作区**」的默认姿态，最贴近我们想要的权限模型（默认只读、写需显式授权）。
- 代价：overlay 语义（copy-on-write、跨层 rename、元数据一致性）是实现量最大的一块，边界情况多。
- 适合：产品化阶段。

### 三档对比

|     | 实现成本 | 磁盘残留 | 权限粒度          | 快照/回滚 | guest 侧改动      |
|-----|----------|----------|-------------------|-----------|-------------------|
| (a) | 极低     | 有       | 目录级            | 无        | 无（preopen）     |
| (b) | 高       | 无       | 文件级            | 有        | 无（shadow 透明） |
| (c) | 最高     | 无       | 文件级 + 挂载模式 | 有        | 无                |

**建议**：M1 用 (a) 起步； (b)/ (c) 的关键性质是「对 guest 透明」，所以 **现在不需要为它改任何 guest 代码**，等 M4 需要时再上。唯一要提前注意的是 §6 的「preopen 必须覆盖 guest 根」——三档都遵守这条。

## 6. 关键机制与实测坑

**M0 已实测（命令层）：**

1. **argv[0] 必须是裸命令名**（`cat`，不是 `cat.wasm`）：`uucore::util_name()` 取 argv[0] 的 basename，错误信息 / `--help` / `--version` 都用它。这是「一份组件注册到多个命令名」的实质。
2. **交给 `uumain` 的 argv 必须包含 argv[0]**：`#[uucore::main]` 生成的 `uumain` 直接交给 clap，clap 认为第 0 个元素是程序名。先 `next()` 掉再传，clap 会吃掉第一个真实参数：`cat FILE` 变成读 stdin（静默空输出）、`wc -c FILE` 退化成默认三列—— **不报错，只是做错事**。
3. **guest 必须有连贯的根**：uucore 的 `canonicalize` 会逐级 `stat` 路径前缀（含 `/`）；只 preopen 子目录时 `stat("/")` 失败 → `mv` 直接 `ENOENT`（`cp` 不受影响）。wasmtime 下 `--dir=<host>::/`，生产里等价于「会话根就是 guest 根」。
4. **cwasm 预编译**：`.wasm` 每次 spawn 重新编译 ~6 MB 模块（~141 ms），cwasm ~8 ms；flags 与 child engine 一致，缓存键含 wasmtime 版本。

**guest 侧（JS）：**

5. **必设 `set_max_stack_size`**（§3.1），否则深递归直接 trap。
6. **构建要 wasi-sdk + quickjs-ng 子模块**：`WASI_SDK=<path>`（本机已有 34）；`sys/quickjs` 是 submodule，未 init 会直接构建报错（§3.1 实测）。
7. **`fs` 的可见性是 preopen 决定的**，不是路径字符串决定的：脚本里 `/data` 能不能打开，取决于我们给了哪个 preopen（§5）。
8. **guest 里的 tokio 只允许 `sync,macros,io-util,rt,time`**（`fs`/`process` 直接 `compile_error`，§3.2 实测），也 **没有可用的阻塞线程池**：首版同步 host import；rquickjs 的 executor 只用来驱动 JS 内部的 Promise，不做宿主等待。
9. **超时两层**：JS 侧 `set_interrupt_handler`（脚本总预算，含纯计算死循环）+ 命令侧超时（要给 `tower-agent-wasmtime` 加 epoch/fuel；现状没有）。 **硬中断会丢 guest 缓冲区**：中断粒度放在「命令」，不要指望中断后还能拿到半截输出。
10. **内存在两侧都要设**：JS 侧 `set_memory_limit`；命令侧 `StoreLimits`（默认 256 MiB）。
11. **输出截断**：命令 stdout/stderr 各限 N MB，超限截断并打 `truncated`——否则一次 `cat` 大文件就能吃掉整个上下文预算。
12. **stdio 是内存缓冲，不是流式管道**：需要流式时按块 drain（宿主侧），或在 JS 侧用 Promise 分块（复杂度高，本期不做）。
13. **guest 里符号链接建不了**：preopen 到 guest 根时 `ln -s` 报 `Permission denied`（硬链接 `ln` 正常）。属 std 的 wasip2 支持 / wasmtime preopen 权限范畴，M1 冒烟里要确认并写进「工具能力表」（可能要在文档/提示词里告诉模型 `ln -s` 不可用）。
14. **QuickJS 默认没有 `console`**（它是宿主对象）：prelude 得先 `globalThis.console = globalThis.console || {}` 再挂方法，否则一加载就 TypeError（实测踩到）。
15. **顶层 `await` 要求脚本包成 async 函数**：用 `new AsyncFunction(src)()`（既能 `await` 又能 `return`），源码用 `serde_json::to_string` 转成 JS 字符串字面量传入（不拼字符串）；之后手工驱动 microtask 队列（`PromiseState::Pending` + `ctx.execute_pending_job()`）。
16. **`SandboxOptions::max_wasm_stack`**：为上面的线程栈约束新增；默认仍是 wasmtime 自己的 512 KiB（monty 不受影响）。

## 7. codemode 工具设计

| 项     | 设计                                                                                                                                                         |
|--------|--------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 入参   | `{ code: string, timeout_ms?: u32, cwd?: string, mounts?: [{ path: string, mode: "ro"\|"rw" }] }`                                                            |
| 返回   | `{ ok, value?, stdout, stderr, commands: [{name, args, code, ms}], duration_ms, truncated }`                                                                 |
| 权限   | 默认**只读**会话根；写路径 / 额外挂载需显式授权；能力开关沿用 `ModPermissions`（`crates/tower-agent-monty/src/manager.rs:84`）那种「一个能力一个开关」的形状 |
| 审计   | `commands[]` 记录实际执行的命令与参数，TUI 可展示；为「执行前审批」（先列命令、批准再跑）留好数据                                                            |
| 描述词 | 工具 description = API 摘要 + 4~5 个范例（读+排序+截断 / 目录遍历过滤 / 写文件 / 错误处理），范例只用已注册函数                                              |
| 集成   | 走 `tower-agent-core` 的 `AgentTool` / `ToolSpec`（`serde_json` Schema），与 monty 的工具注册方式同构（`crates/tower-agent-monty/src/tool.rs`）              |

### 7.1 默认限额（已定，Q8）

| 项          | 默认值                                        | 实现处                                                                              |
|-------------|-----------------------------------------------|-------------------------------------------------------------------------------------|
| 脚本总超时  | 30 s                                          | `Runtime::set_interrupt_handler`（纯计算死循环也管）                                |
| 单命令超时  | 10 s                                          | 命令侧 epoch（M5 给 `tower-agent-wasmtime` 加）                                     |
| 脚本大小    | 64 KiB                                        | 工具入参校验                                                                        |
| JS 堆上限   | 64 MiB                                        | `Runtime::set_memory_limit`                                                         |
| JS 栈上限   | **64 KiB**（不是上游建议的 256 KiB，见 §3.1） | `Runtime::set_max_stack_size`（wasm 上必设；组件里超限是 trap，宿主降级成脚本失败） |
| wasm 影子栈 | 1 MiB                                         | `SandboxOptions::max_wasm_stack`（须 < 宿主线程栈）                                 |
| 命令内存    | 256 MiB                                       | `SandboxOptions`（沿用现有默认值）                                                  |
| 命令条数    | 100                                           | 命令层计数                                                                          |
| 输出上限    | 每流 1 MiB（超限截断并置 `truncated`）        | 工具层                                                                              |

这些值都做成工具参数可覆盖；M1 先写死默认值。另两条本轮定下： **Q9 = 不引入 sqlite 模块**； **Q10 = 阶段一会话即焚**（跑完即丢，不写任何持久位置），全部跑通后再开放「保存与复用」（§9 M7）。

## 8. 决策与待决问题

**本表是唯一权威编号**——早期草案曾把白名单 / 限额排在别的号上，一律以本表为准。

| #   | 问题             | 选项 / 现状                                                                                                      |
|-----|------------------|------------------------------------------------------------------------------------------------------------------|
| Q1  | JS 跑在哪        | ✅ **已定：wasm guest**（本文按此写）                                                                            |
| Q2  | 会话文件系统档位 | ✅ **已定：(a) 会话目录**（配 R1）。等需要「无残留 / 文件级权限 / 回滚」时再上 (b)/(c)——对 guest 透明，见 §5     |
| Q3  | JS API 风格      | ✅ **已定：thenable `Cmd` 链式 + `exec` 逃生口**（§4.2）                                                         |
| Q4  | 要不要 `sh()`    | ✅ **已定：不做**，命令函数 + `exec` 足够（不引入 shell 解析）                                                   |
| Q5  | llrt 模块取舍    | ✅ **已定：不 fork llrt**，进 guest 的模块见 §3.2 分类表。⚠️ 唯一卡点：**`llrt_fs` 在 guest 里编不过** → 见 §8.1 |
| Q6  | llrt 模块白名单  | ✅ **已定：§3.2「白名单」**                                                                                      |
| Q7  | 预览版本         | ✅ **已定：保持 `wasm32-wasip2`**（rquickjs 支持矩阵一致；rquickjs 无 p3）                                       |
| Q8  | 默认限额         | ✅ **已定：§7.1**                                                                                                |
| Q9  | sqlite 模块      | ✅ **已定：不要**                                                                                                |
| Q10 | 持久化           | ✅ **已定：阶段一会话即焚**；全部跑通后再开放保存 / 复用（§9 M7）                                                |
| Q11 | 并发（新增）     | ✅ 已定：**不走 p3**（rquickjs 的 wasip3 不成熟），做 ticket + join（`execStart`/`execJoin`，留在 p2），见 §9.4  |

### 8.1 Q5 的硬约束（需要在两条路里选一条）

「不 fork llrt」已定，但 `llrt_fs` 的 tokio feature 是 `["fs","io-util","rt"]`，而 tokio 在 wasm 上只允许 `sync,macros,io-util,rt,time`（§3.2 实测 `compile_error`）。于是：

> **「Q1 = JS 在 guest」与「直接使用 `llrt_fs`」不能同时成立。**

两条自洽的路：

| 路             | 做法                                                                                                                                                               | 代价                                                                                                        |
|----------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------|
| **R1（倾向）** | 保持 **Q1 = guest**。`fs` 由我们在 guest 里实现（§4.4 (i)：`std::fs` over preopen），可复用 `llrt_path`/`llrt_encoding`/`llrt_buffer`（不 fork）；`llrt_fs` 不参与 | 自写一个 Node-ish `fs` 子集（readFile/writeFile/readdir/stat/mkdir/rm/rename/exists + 编码），约 200–400 行 |
| **R2**         | 把 **Q1 改回宿主**，这样 `llrt_fs`、`llrt_child_process`、`llrt_stream` 等原样可用                                                                                 | 丢掉 wasm 内存隔离（脚本与宿主同进程），隔离降级为「能力收敛 + 可选独立进程」；guest 构建链也不再需要       |

倾向 **R1**：方向已经定在 guest，而真正要自己写的只有 `fs` 的一个子集——LLRT 那边 12 个文件（access/chmod/open/read_dir/read_file/rename/rm/stats/symlink/write_file/file_handle/mkdir）里，M1 只需要前六个语义。

**另注**：`llrt_os` 虽然能进 guest，但它的语义是「报告宿主信息」——在沙箱里要么返回假值要么泄露宿主信息，建议 M1 不注册。

## 9. 里程碑

| 里程碑 | 内容                                                                                                                                                                                             | 依赖 |
|--------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|------|
| M0     | ✅ 完成：uutils 0.12.0 → wasip2 multicall（零补丁 wrapper；**已扩到 60 util / 11 MB**，§3.5；§6 的坑全部实测）                                                                                   | —    |
| M1.0   | ✅ 完成：rquickjs 0.14 → `wasm32-wasip2` 组件在 wasmtime 上跑起 JS（1.26 MB；栈/trap 已定性，§3.1）                                                                                              | —    |
| M1.1   | ✅ 完成：`wasm/coreutils-wasm` 落地（60 util，7.4 MB，`./generate.sh` 一键重建；argv[0] 分发 + `coreutils <util>` 调试形式；冒烟通过，§3.5）                                                     | —    |
| M1.2   | ✅ 完成：`wasm/rquickjs-sandbox` 落地（rquickjs + WIT `run`/`exec`，796 KB；`./generate.sh` 一键重建，crates.io rquickjs 无需子模块/vendor）                                                     | —    |
| M1.3   | ✅ 完成：`crates/tower-agent-codemode` 宿主落地——`bindgen!` 绑定 + 实现 `exec`（起 coreutils 组件、内存 stdio、`argv[0]`=裸命令名）+ 会话根 preopen 成 guest `/`；**6 个冒烟全过**               | —    |
| M2     | ✅ 完成：`CodemodeTool`（`AgentTool`：schema、超时、`commands[]` 审计、命令级进度流）+ 产物内嵌（`include_bytes!`，测试不再需要 generate.sh）                                                    | M1.3 |
| M3     | ✅ 完成：JS 可见表面——命令函数 + thenable `Cmd` 链式 pipeline、`.result()`/抛异常两种取值、`fs` 门面扩到 `mkdir`/`exists`/`rm`、命令表由宿主注入并加**漂移测试**；**13 个测试全过**（新增 4 个） | M2   |
| M4     | ✅ 完成：挂载策略（会话根可写映射 `/`、额外挂载默认只读、JS 与命令共用同一套 preopen、越界拒绝）+ **CLI 接线**（见 §9.1）                                                                        | M3   |
| M5     | ✅ 完成：超时改**硬取消**（异步链路）+ epoch 兜底；JIT 缓存不再静默降级；命令引擎进程内共享（套件 96 s → 22 s，见 §9.2）                                                                         | M4   |
| M6     | 📋 下一阶段：并发——**不走 p3**，做 ticket + join（`execStart`/`execJoin`，见 §9.4）；另加命令组件 rg / sd                                                                                        | M5   |
| M7     | 🟡 第一层完成：命名会话根跨进程复用（默认即焚）+ 列出/清理（`--codemode-sessions` / `--codemode-session-clear`，见 §9.3）                                                                        | M5   |

M5 补记： **超时已改成「硬取消」（异步链路）**

- 问题：`epoch` / `fuel` 只在 **执行 wasm 指令**时被检查，所以打不断「卡在宿主调用里」的 guest——`sleep`（`wasi:clocks` 阻塞）、阻塞读写都算，上一版表现为「睡满了才失败」。
- 解法：`wasmtime-wasi` 的 `p2` 本身带 `wasmtime/async`，于是把这条链改成异步——
  `bindgen!` 加 `exports: { default: async }`；宿主 import `exec` 用 `func_wrap_async`；
  命令走 `Command::instantiate_async` + `tokio::time::timeout`；`CodemodeHost::run_async`
  给整段脚本再套一层超时（`run` 是同步包装，内部起 current-thread runtime）。
- 语义：超时 = **取消**（future 丢弃、`Store` 随即销毁，不留悬挂线程）。 **没有**「半截结果」。
- 两条硬上限：脚本总超时（默认 30 s）、单条命令（默认 10 s，`exec(..., { timeoutMs })` 可覆盖）。
- `epoch_timeout` 保留为 **次要兜底**（纯 wasm 死循环的更廉价路径）。
- 坑：wasmtime 49 里 `Config::async_support` **已废弃（无效果）**；真正决定行为的是 linker 用
  `add_to_linker_sync` 还是 `add_to_linker_async`（后者才把 WASI 的等待变成 await 点）。

### 9.1 CLI 接线（已完成）

默认布局用 `CodemodeTool::for_workspace_with(workspace, SessionRoot, mounts)`：

- **工作区只读挂到 `/work`**（模型能读、改不到）；
- **会话根**（可写，映射成 guest 的 `/`）按 `SessionRoot` 分两种：`Ephemeral` = 临时目录、 **由工具持有、drop 即删**（会话即焚，默认）；`Persistent(dir)` = 固定目录、跨进程复用（M7）；
- `mounts` 是追加的挂载（`Mount::parse("<dir>[:<guest>][:ro|rw]")`，缺省 `/work` + 只读）。

CLI 三个开关（`crates/tower-agent-cli/src/args.rs`）：

```sh
tower-agent --codemode                                   # 临时会话，产物会话结束即弃
tower-agent --codemode --codemode-session demo           # 会话根 = <config>/tower-agent/codemode/sessions/demo
tower-agent --codemode --codemode-mount /data:/data:ro   # 额外只读挂载；:rw 才可写
```

接线在 `Session::enable_codemode(...)`（`session.rs`）：解析挂载 → 构造工具 → 推入 `base_tools` → `refresh_tools()`（下一轮 turn 生效）；失败只告警、不致命。`--list-tools --codemode` 也会列出它（只读名字与描述， **不构造工具**，否则为了列个名字要编译 7.4 MB）。

**刻意没把 codemode 放进内置工具插件集合**：构造 codemode 要编译组件，放进默认列表会让每个使用标准工具集的宿主都变慢；所以 codemode 只在开关打开时才由装配层追加。

> **插件化（已接线）**：codemode 是一个 **静态插件**（`CodemodePlugin`：`protected` + `provides tool.codemode`），
> 与 `tower-agent-wasmtime` 的 `WasmtimePlugin`（`protected` + 能力 `sandbox.wasmtime`）一起不允许被
> 动态/JIT 插件同名覆盖。两者都由 **组装层** `tower-agent::assembly::HostPlugins` 登记
> （依赖 `tower-agent` 的宿主，如 axum，也用同一入口）；CLI 的 `--codemode` 在运行时通过
> `ModManager::register_static_plugin` 追加。名字与能力都进 `protected_*`。
> 当前完成度与 **剩余待办** 见 [`plugins.md` §16](./plugins.md)。

另外要把「产物怎么随包走」的既定答案用上：两个组件已经内嵌在 `tower-agent-codemode`（`include_bytes!`），所以接线不需要额外的文件分发。

### 9.2 耗时测量与结论（M5 收尾）

```sh
cargo test -p tower-agent-codemode --offline --test measure -- --ignored --nocapture
XDG_CACHE_HOME=/tmp/cm-cache cargo test -p tower-agent-codemode --offline --test measure -- --ignored --nocapture
```

| 项                                          | 耗时                             |
|---------------------------------------------|----------------------------------|
| 编译两个组件（冷，无缓存）                  | ~10 s                            |
| 脚本（不含命令）                            | ~3 ms                            |
| 脚本 + 1 条命令                             | ~7 ms（≈4 ms：实例化 + 跑完）    |
| 脚本 + 5 条命令                             | ~15 ms（≈2.3 ms/条）             |
| 第二个 host（**缓存可写**）                 | 196 ms（vs 不可写 9.4 s）        |
| 第二个 host（进程内共享引擎后，即便无缓存） | **1.3 s**（只剩 guest 组件要编） |

结论：

1. **成本几乎全在「编译组件」**；实例化与每条命令都是毫秒级 → **cwasm 预热 / 实例池化不做**（conch 那条「必须 cwasm」是它每段管道新起进程/组件的架构使然，不是我们这条）。
2. **JIT 缓存本身有效**（196 ms），但 **不可写时是静默降级**——测试在受限环境里跑了很久就是这个原因，所以 `configure_cache` 现在会打一次警告，`CodemodeOptions::with_cache_dir` 也能显式指定。
3. **引擎进程内共享**（`exec.rs` 的 `COMMAND_ENGINES`，键 = coreutils 字节 + 单命令超时）把同进程后续 host 从 9.4 s 降到 1.3 s；会话根/挂载是 per-`Store` 的，不参与缓存。
4. 只有「冷启动必须 <1 s」才需要 **拆组件**（~10 s 与 7.4 MB 基本成正比）。

### 9.3 CLI 开关与会话管理

| 开关                                        | 作用                                                                                  |
|---------------------------------------------|---------------------------------------------------------------------------------------|
| `--codemode`                                | 启用 codemode（工作区只读挂 `/work` + 会话根可写）                                    |
| `--codemode-session <name>`                 | 会话根持久化在 `<config>/tower-agent/codemode/sessions/<name>`，跨进程复用；不传则临时（即焚） |
| `--codemode-mount <dir>[:<guest>][:ro\|rw]` | 追加挂载（可重复）；缺省 guest `/work`、只读                                          |
| `--codemode-sessions`                       | 列出持久化会话后退出（只读目录，不构造工具）                                          |
| `--codemode-session-clear <name>`           | 删掉一个持久化会话（连同产物）后退出                                                  |
| `--codemode-export [dir]`                   | 把脚本仓库投影成每脚本一个文件；缺省目录 `<cwd>/.tower-agent/codemode/`               |
| `--codemode-import [dir]`                   | 从文件导入脚本；缺省按文件内 scope，`--codemode-script-scope` 可覆盖                  |
| `--codemode-script-scope <user\|project>`   | 选择导出作用域或覆盖导入目标作用域                                                    |

会话名会被校验成「一层普通名字」，`/`、`..` 之类一律拒绝（`session_name_is_valid`），所以清理不会跑到基目录外面；`tests/sessions.rs` 守着列表、清理与这条越界防护（不碰真实 config，全在临时目录里做）。

### 9.4 M6 调研：并发要不要 p3

**wasmtime 49 的 p3 状态（本机源码核对）**：`wasmtime-wasi` 49 有 `p3` feature（开出来是 `wasmtime/component-model-async` + `wasmtime/component-model-bytes`）；wasmtime 49 本身有 `component-model-async` feature、`Config::wasm_component_model_async`、`func_wrap_concurrent`，p3 的 bindings 也在。 **我们当前没开**：workspace 的 wasmtime features 只有 `async`，wasmtime-wasi 只开 `p2`。

**rquickjs 的硬约束**：支持矩阵是 `wasm32-wasip1` / `wasm32-wasip2` / `wasm32-unknown-unknown`， **没有 `wasm32-wasip3`** → 「JS guest 直接跑在 p3」目前不可行（除非上游加目标，或把 JS 挪出 guest）。

**但「并发」不等于「必须 p3」。** 脚本真正想要的并行是 **命令级并行**，不用等 p3，在现在的 p2 + 同步 host import 上就能做到——把 `exec` 拆成两步：

```js
const t1 = execStart("cat", ["/a"]);   // 立刻返回 ticket，宿主后台已开跑
const t2 = execStart("cat", ["/b"]);   // 两条命令真正并行
const [a, b] = [execJoin(t1), execJoin(t2)].map((r) => r);  // 或 parallel([t1, t2])
```

- 宿主侧天然可行：`ExecService` 无状态（每次调用新建 `Store`、共享编译好的引擎 `Arc`），所以并发调用是安全的；每命令仍有自己的 deadline（异步取消那条链不变）。
- 要新做的东西：① ticket → 结果/错误表（脚本结束时未 join 的要能回收）；② `commands[]` 审计与进度事件按 ticket 标注；③ 同一会话根的并发写竞态由脚本自负（可在文档里点明）。

**什么时候才真需要 p3**：流式 stdio（边跑边喂/边读，而不是内存缓冲）、结构化并发的取消传播、以及 Go 组件（其运行时要 wasip3）。这些都与「脚本里跑并行命令」正交。

**结论（已定）**：并发 **不走 p3**——rquickjs 的 wasip3 目标不成熟（明确不做），而脚本要的并行是命令级并行，用 ticket + join 留在 p2 就能拿到。p3 只在将来真要「流式 stdio / Go 组件」时另议，那时 JS 大概率要挪回宿主。

## 10. 风险与缓解

| 风险                                                  | 影响                               | 缓解                                                                                     |
|-------------------------------------------------------|------------------------------------|------------------------------------------------------------------------------------------|
| rquickjs 的 wasip2 路径                               | ✅ 已实测跑通（§3.1，1.26 MB）     | 但**深递归会 trap**：限额取 64 KiB（可捕获）+ 宿主把 trap 当脚本失败并丢弃实例           |
| 构建要 wasi-sdk（curl 下载）                          | 离线/CI 失败                       | 预置 `WASI_SDK=<path>`（本机已有 34）或 vendor 一份；另需初始化 quickjs-ng 子模块        |
| 构建要 wasi-sdk（curl 下载）                          | 离线/CI 失败                       | 预置 `WASI_SDK=<path>` 或 vendor 一份；写进构建文档                                      |
| guest 里的 tokio 不含 `fs`/`process`                  | 异步/并发受限；`fs` 必须自写       | 首版同步 import；`fs` 按 §8.1 R1 自写；并发按 Q11：ticket + join（p2），不上 p3          |
| API 全靠自研，模型可能写不顺                          | 一次往返失败率升高                 | 工具描述里放 API 摘要 + 范例；`exec` 作降级路径；错误信息带行号与片段                    |
| rquickjs 是 C 引擎，但**在 wasm 里**                  | 边界强，但引擎自身 bug 会 trap     | 命令实例池 + 复用；trap 归一化成「脚本执行失败」而不是宿主错误                           |
| 组件体积（rquickjs + 60 util，`wasm-opt` 对组件无效） | 内嵌体积增大                       | `opt-level="z"` + lto + strip（11 MB → 7.4 MB）；真要瘦身就把 util 拆成「核心集/扩展集」 |
| 产物已内嵌进仓库（`assets/` 共 ~8.2 MB）              | 仓库与二进制各 +8.2 MB             | 有意为之（免去构建期跑 generate.sh）；若嫌大就走上面的拆组件路线                         |
| preopen / VFS 语义与脚本直觉不一致（§6 坑 3）         | 命令静默失败或越界                 | 会话根 = guest 根；M4 上 VFS 后语义统一；冒烟覆盖 `mv`/`cp`/相对路径                     |
| `llrt_fs` 进不了 guest（tokio 禁用 `fs`）             | 需要自写 fs 子集，或把 JS 挪回宿主 | 按 §8.1 R1：自写 guest `fs`，复用 `llrt_path`/`llrt_encoding`（不 fork）                 |
| llrt_modules 是 `0.9.0-beta`                          | 上游变更带来 rebase 成本           | 只取用到的模块（`fs`/`child_process` 已排除）；升级走 rebase + 冒烟                      |

## 11. 附录

### 11.1 复现（M0 已跑过的命令）

```bash
# uutils wrapper：零补丁 wasip2 组件
cd /tmp/uumulti && cargo build --release --target wasm32-wasip2 --offline
# → target/wasm32-wasip2/release/uumulti.wasm（6.37 MB）

# 按命令名分发（宿主把 argv[0] 设成裸命令名）
wasmtime run --argv0 cat --dir="$SESSION::" uumulti.wasm "$SESSION/data/nonl.txt"

# §6 坑 3：只 preopen 子目录时 mv 失败；preopen 到根就成功
wasmtime run --argv0 mv --dir="$SESSION/data::/data" uumulti.wasm /data/a.txt /data/b.txt  # ENOENT
wasmtime run --argv0 mv --dir="$SESSION::"          uumulti.wasm /data/a.txt /data/b.txt  # OK

# §3.2 的证据：llrt_fs 那组 tokio feature 在 wasm 上被 tokio 自己拒绝
cd /tmp/tokiowasm && cargo build --target wasm32-wasip2 --offline
# error: Only features sync,macros,io-util,rt,time are supported on wasm.
```

### 11.2 引用清单

| 位置                                                                        | 内容                                                                         |
|-----------------------------------------------------------------------------|------------------------------------------------------------------------------|
| `~/Projects/DelSkayn/rquickjs/README.md`（支持矩阵 / wasm 章节）            | wasip1/p2/unknown-unknown 支持；`set_max_stack_size` 必设                    |
| `~/Projects/DelSkayn/rquickjs/core/src/runtime/base.rs:124,158,167,174,200` | interrupt / memory limit / stack size / gc threshold / memory usage          |
| `~/Projects/DelSkayn/rquickjs/sys/build.rs:10,89,228,238`                   | wasi-sdk 24.0 下载、`WASI_SDK` 覆盖、`RQUICKJS_SYS_NO_WASI_SDK`、`--sysroot` |
| `~/Projects/DelSkayn/rquickjs/.github/workflows/ci.yml:216,217`             | wasip1（stable）/ wasip2（nightly）`full-async-wasi` 测试项                  |
| `~/Projects/awslabs/llrt/llrt_modules/src/module_builder.rs`                | `ModuleBuilder` / `with_module` / `with_global`                              |
| `~/Projects/awslabs/llrt/modules/llrt_fs/src/{lib.rs,rename.rs,…}`          | `fs` = `tokio::fs`，无 root jail                                             |
| `~/Projects/awslabs/llrt/libs/llrt_utils/Cargo.toml`                        | `llrt_utils` 自带 `tokio/sync`（连「无 tokio」模块也要验证）                 |
| `~/Projects/rquickjs/rquickjs-extra/modules/{console,url,os,timers,sqlite}` | 各模块的 tokio 依赖（console/url 无，其余有）                                |
| `~/Projects/rquickjs/rquickjs-serde/README.md`                              | serde ↔ Value 双向，纯 Rust                                                  |
| `tokio-1.53.2/src/lib.rs:479`（实测）                                       | wasm 上只允许 `sync,macros,io-util,rt,time`；`fs`/`process` 直接报错         |
| `llrt/modules/llrt_fs/Cargo.toml`、`llrt_child_process/Cargo.toml`          | 两者 tokio feature = `fs,io-util,rt` / `process`（故 guest 里编不过）        |
| 本仓库 `crates/tower-agent-wasmtime/src/engine.rs:11,15,61`                 | `SandboxOptions` / `DEFAULT_MEMORY_LIMIT` / 无 preopen                       |
| 本仓库 `crates/tower-agent-monty/src/manager.rs:84`                         | `ModPermissions` 权限形状                                                    |
| 本仓库 `wasm/monty-sandbox/generate.sh`                                     | 独立 wasm workspace → 组件 → 内嵌的既有流程                                  |
