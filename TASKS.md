# 执行任务清单：codemode v2（mods + codemode 统一）

> **这是可冷启动执行的任务清单。** 下次会话从这里开始：先读「0. 上下文摘要」，再从
> 第一个未勾选任务往下做；每完成一项就勾掉，并在文末「进度日志」写一行。
>
> 设计依据：[`docs/codemode.md`](docs/codemode.md) §0（v2 提案 + 决策）、
> [`docs/sdk.md`](docs/sdk.md)（扩展点）、[`docs/roadmap.md`](docs/roadmap.md)（全局未完成项）。

| 项目       | 值                                             |
|------------|------------------------------------------------|
| 创建       | 2026-10-17                                     |
| 当前阶段   | **Phase 3（合并 feature / CLI / 词汇）**       |
| 下一个任务 | **T3.3**                                       |
| 代码状态   | **T1–T3.2 已完成**：能力层、feature 与 CLI 开关已收敛 |

---

## 0. 上下文摘要（新会话必读）

### 0.1 要做什么

把 `mods`（monty/Python）和 `codemode`（rquickjs/JS）合并成 **同一个能力**：`codemode`。
定位是 **agent 可以编程**：LLM 生成脚本 → 沙箱执行 → 脚本既能组合工具，也能扩展 agent 行为。
保留两个解释器，由宿主选（默认引擎 + 脚本可覆盖）。

### 0.2 已定决策（不要再改）

| #  | 决策                                                                                                                              |
|----|-----------------------------------------------------------------------------------------------------------------------------------|
| Q1 | 引擎选择**两者都支持**：进程级默认（`--codemode-engine` / 配置 `codemode.engine`）+ 脚本可声明语言覆盖                            |
| Q2 | **两个引擎都保留**：`monty`（Python 子集）与 `rquickjs-sandbox`（JS/rquickjs）                                                    |
| Q3 | **`mods` 一词彻底消失**：不再有顶层 mods 模式；`mod_persist` 等降级为 codemode 的"持久化脚本"子概念；CLI / feature / 环境变量改名 |
| Q4 | 扩展点 v0 = **工具注册 + 工具前后钩子**（`tool_register` / `tool_call` / `on("tool_call")` / `on("tool_result")`）                |
| Q5 | 权限**沿用并扩展**：`ModPermissions` → `ScriptPermissions`（增加 `exec` / `hooks` / `commands` 等），不做两级大改                 |
| Q6 | 脚本仓库 = **(c) storage 为主 + 导出/导入**：SQLite 是真源，文件是显式投影；元数据永远在 storage                                  |
| Q7 | 扩展点**合并到 `tower-agent-plugin-sdk`**，不新增 `tower-agent-sdk` crate                                                         |

### 0.3 工作约定

- 版本未稳定， **不做向后兼容**（旧 DB / 旧 CLI 标志可直接废弃）。
- 行为对齐 pi 时以 `~/Projects/earendil-works/pi` 为准。
- 每完成一个任务：更新本清单勾选 + 在「进度日志」写一行（日期 / 任务 / 一句话结果）。
- 改代码后必须跑（全绿才算完成）：
  ```bash
  cargo fmt
  cargo clippy --offline --all --workspace --all-targets
  cargo test --offline --all --workspace
  ```
  涉及 feature 合并时额外跑：
  ```bash
  cargo check --offline --no-default-features -p tower-agent-cli
  cargo check --offline --no-default-features -p tower-agent
  ```
- 设计文档与代码同批更新（`docs/codemode.md` / `docs/sdk.md` / `docs/plugins.md`）。
- 不建议顺手做本清单之外的大改；发现的额外问题记到 `docs/roadmap.md`。

### 0.4 现有代码落点（改之前先读）

| 区域              | 文件                                                                                                                |
|-------------------|---------------------------------------------------------------------------------------------------------------------|
| 沙箱基础设施      | `crates/tower-agent-wasmtime/src/{engine,host,sandbox,pool,plugin}.rs`                                              |
| 命令集            | `wasm/coreutils-wasm/src/main.rs`（60 个命令的 `utils!` 表）、`crates/tower-agent-codemode/src/exec.rs`             |
| JS 引擎           | `crates/tower-agent-codemode/src/{lib,tool,exec,plugin}.rs`、`wasm/rquickjs-sandbox/wit/codemode.wit`               |
| monty/Python 引擎 | `crates/tower-agent-monty/src/{manager,sandbox,host,tool,store}.rs`、`wasm/monty-sandbox/wit/sandbox.wit`           |
| 插件/扩展点       | `crates/tower-agent-plugins/src/{layer,static_plugin,host,registry}.rs`、`crates/tower-agent-plugin-sdk/src/lib.rs` |
| 工具钩子          | `crates/tower-agent-core/src/layer.rs`（`BeforeToolCallLayer` / `AfterToolResultLayer`，runtime 再导出）            |
| 存储              | `crates/tower-agent-storage/src/lib.rs`、`crates/tower-agent-turso/src/lib.rs`                                      |
| CLI               | `crates/tower-agent-cli/src/{main,args,session,store}.rs`                                                           |
| 组装层            | `crates/tower-agent/src/assembly.rs`                                                                                |

---

## 1. Phase 1 — 统一能力层（P1）

目标：两个引擎面对脚本时是 **同一张能力表**；执行模型保持各自原生（monty 的挂起/resume、JS 的同步 import）。

### [x] T1.1 新增 `tower-agent-codemode-core`

- **依赖**：无（第一个任务）
- **为什么**：能力层要被两个引擎共用，而 `tower-agent-codemode`（宿主）以后会依赖 `tower-agent-monty`（引擎），不能再让 monty 反向依赖 codemode（会成环）。所以把引擎无关部分下沉到新 crate。
- **做什么**：
    1. 新建 `crates/tower-agent-codemode-core`，加入 workspace。
    2. 定义 `ScriptLanguage`（`Monty` / `JavaScript`）、`ScriptPermissions`（从 monty 的 `ModPermissions` 扩展：`exec` / `fs` / `tools` / `register_tools` / `hooks` / `commands` / `ui` / `state`）。
    3. 定义对象安全的 `CapabilitySurface`（用 `BoxFuture`，与 runtime 风格一致）：
       `exec(name,args,opts)` / `fs_*` / `tool_call` / `tool_list` / `tool_register` / `tool_unregister` /
       `state_get` / `state_set` / `log` / `ui_*`。
    4. 能力调用的统一错误类型（未授权 / 未知能力 / 引擎错误 / 超时 / 取消）。
    5. 单元测试：权限矩阵、未知能力报错、错误可序列化回脚本。
- **验收**：`cargo test -p tower-agent-codemode-core`；crate 不依赖 monty / codemode / wasmtime。

### [x] T1.2 monty 接上 `exec`（coreutils-wasm）

- **依赖**：T1.1
- **做什么**：
    1. `crates/tower-agent-monty/src/manager.rs` 的 host dispatch 增加 `exec`（参数/返回值 JSON，字节用 base64 或 UTF-8 字符串，选一种并写进 WIT/文档）。
    2. 命令执行通过注入的 `CapabilitySurface`（由宿主实现，内部复用 `tower-agent-codemode::ExecService`），monty crate 不直接依赖 codemode。
    3. 权限 `ScriptPermissions.exec` 强制；未授权返回明确错误。
    4. 测试：monty 脚本跑 `cat` / `wc`，读到 coreutils 输出。
- **验收**：`cargo test -p tower-agent-monty`；新增端到端用例覆盖 1 个命令 + 1 个权限拒绝。

### [x] T1.3 JS 引擎接上工具能力

- **依赖**：T1.1
- **做什么**：
    1. 扩展 `wasm/rquickjs-sandbox/wit/codemode.wit` 的 host interface：`tool-call` / `tool-list` / `tool-register` / `tool-unregister`（与 T1.1 的 `CapabilitySurface` 一一对应）。
    2. JS 门面（预载 JS 层）暴露 `tool_call` / `tool_list` / `tool_register` / `tool_unregister`。
    3. 宿主 `tower-agent-codemode` 实现这些 import，落到 `HostApi` / 工具注册表。
    4. 权限强制（`tools` / `register_tools`）。
    5. 重新生成内嵌组件（`wasm/rquickjs-sandbox/generate.sh`）。
- **验收**：JS 脚本能 `tool_call("read", …)`；`tool_register` 后工具在下一轮出现；权限拒绝有测试。

### [x] T1.4 能力一致性测试

- **依赖**：T1.2、T1.3
- **做什么**：同一份"能力矩阵"测试表，两个引擎各跑一遍（exec / fs / tool_call / tool_register / state / log / ui）；把差异（语言语法）隔离在测试脚本里。
- **验收**：新增 `tests/capability_parity.rs`（或两 crate 各一组同名用例）；矩阵里每一行两引擎都过。

---

## 2. Phase 2 — 统一提示词 + 引擎选择（P2）

### [x] T2.1 `PromptBuilder`：由能力集生成提示词

- **依赖**：T1.1（类型）、T1.2/T1.3（能力真实存在）
- **做什么**：
    1. 在 `tower-agent-codemode-core` 增加 `PromptBuilder`：输入 `CapabilitySurface` 的能力清单 + 挂载表 + 限额 + `ScriptLanguage`，输出工具 description / instructions。
    2. 能力表是唯一来源：开了哪些权限、挂了哪些目录，提示词就写哪些；引擎差异只影响"语法示例"段。
    3. 快照测试：同一能力集下 Python / JS 两份提示词；关掉某能力后提示词同步变化。
- **验收**：`cargo test -p tower-agent-codemode-core`；快照可审阅。

### [x] T2.2 引擎选择（全局默认 + 脚本覆盖）

- **依赖**：T2.1
- **做什么**：
    1. `tower-agent-cli` 增加 `--codemode-engine <monty|js>`；配置键 `codemode.engine`（storage ConfigStore，user ⊕ project 有效值）。
    2. 脚本元数据带 `language`；脚本声明语言时覆盖全局默认。
    3. 未指定时默认值（建议 `js`，与现有 codemode 行为一致；在文档里写明）。
    4. CLI / 配置 / 脚本三层优先级测试。
- **验收**：`cargo test -p tower-agent-cli`；三种优先级各一条用例。

---

## 3. Phase 3 — 合并 feature / CLI / 词汇（P3 + Q3）

### [x] T3.1 feature 合并

- **依赖**：T1.2、T1.3（两个引擎都能独立编）
- **做什么**：
    1. `tower-agent` 门面：`mods` + `codemode` → `codemode`（宿主）+ `codemode-python`（monty）+ `codemode-js`（rquickjs）。
    2. `tower-agent-cli` 同样；默认 feature 组合保持"两个引擎都开"。
    3. 删除 `mods` feature（不做别名，版本未稳定）。
- **验收**：feature 矩阵可编译（见 0.3 的两条 `--no-default-features` 命令 + 各自单开）。

### [x] T3.2 CLI 开关收敛

- **依赖**：T3.1
- **做什么**：
    1. `--codemode` / `--codemode-engine` / `--codemode-session*` / `--no-codemode`。
    2. `--no-mods` / `TPA_NO_MODS` 删除（或直接不识别）。
    3. 帮助文本更新 + 二进制测试。
- **验收**：`cargo test -p tower-agent-cli`；`--help` 快照/断言包含新开关且不含 `mods`。

### [ ] T3.3 作者工具改名

- **依赖**：T3.1、T4.1（脚本仓库）——也可以先改名字、后接仓库
- **做什么**：`mod_write` / `mod_reload` / `mod_list` / `mod_remove` → `codemode_write` / `codemode_reload` / `codemode_list` / `codemode_remove`；保留名列表、prompt、测试同步。
- **验收**：两个引擎都能用同一组作者工具；旧名字不再出现（`rg "mod_write"` 只剩历史文档或清零）。

### [ ] T3.4 目录收敛

- **依赖**：T3.2
- **做什么**：统一到 `<config>/tower-agent/codemode/`（`scripts/` + `sessions/`）；不再使用 `.../mods` 与 `.../codemode/<name>` 两套。旧目录不做迁移（版本未稳定）。
- **验收**：CLI 会话列出/清理走新目录；文档同步。

---

## 4. Phase 4 — 脚本仓库（Q6 = c）（P4）

### [ ] T4.1 `ScriptStore`（storage 扩展）

- **依赖**：无（可与 Phase 1 并行）
- **做什么**：
    1. `tower-agent-storage` 增加 `ScriptStore` 子接口：`upsert_script` / `get_script` / `list_scripts` / `delete_script`，以及 `ScriptRecord { id, name, language, scope, source, enabled, permissions, metadata, created_at, updated_at, source_hash }`。
    2. user / project 作用域（复用 `ConfigScope` 的语义；project id = 规范化工作目录）。
    3. `tower-agent-turso` 实现：新表 `script`，主键 `(scope_kind, scope_id, name)`；写入走同一写队列。
    4. 测试：往返、项目隔离、按 updated_at 排序、删除。
- **验收**：`cargo test -p tower-agent-storage -p tower-agent-turso`。

### [ ] T4.2 codemode 用 `ScriptStore` 作为真源

- **依赖**：T4.1、T3.3
- **做什么**：
    1. `codemode_write/reload/list/remove` 读写 `ScriptStore`；不再写 `mods` 文件。
    2. 脚本启用状态 / 权限 / 语言随记录持久化。
    3. 启动时加载启用的脚本（按作用域：project 覆盖 user 同名）。
- **验收**：脚本写入 → 重启 → 仍可加载并运行；作用域覆盖有测试。

### [ ] T4.3 导出 / 导入

- **依赖**：T4.2
- **做什么**：
    1. `codemode export <dir>` / `codemode import <dir>`（CLI 参数或子命令）。
    2. 每脚本一个文件：源码 + 元数据头（语言 / 权限 / 启用 / 哈希）。
    3. 项目级默认导出到 `.tower-agent/codemode/`；导入按作用域恢复。
    4. 冲突策略：同名同作用域 — 导入覆盖 / 拒绝，二选一并在文档写死。
- **验收**：write → export → 删库 → import → run 全链路测试。

### [ ] T4.4 权限持久化 + 端到端

- **依赖**：T4.2
- **做什么**：`ScriptPermissions` 随记录持久化；运行时首次触发权限提示（复用 `plugins.md` §12 的机制）；无人值守默认拒绝。
- **验收**：两种引擎各一条"未授权 → 拒绝 / 授权 → 成功"的端到端用例。

---

## 5. Phase 5 — 扩展点 v0（P5 / sdk S1–S2）

### [ ] T5.1 `tower-agent-plugin-sdk`：tool 模块

- **依赖**：T1.1
- **做什么**：
    1. 在 plugin-sdk 增加 `tool` 模块：工具注册（名字 / description / 参数 schema / `exposure` / `namespace` / `annotations` / `output_schema`）。
    2. 把现有 `ToolRegistration` 收敛进来（或再导出），保持 Rust 插件可用。
    3. 工具元数据模型补到 `AgentTool` / 工具目录（先做最小字段，不要求 provider 全用上）。
- **验收**：`cargo test -p tower-agent-plugin-sdk`；Rust 插件注册一个带元数据的工具。

### [ ] T5.2 plugin-sdk：tool hooks

- **依赖**：T5.1
- **做什么**：
    1. `hook` 模块：`on(ToolCall, handler)` / `on(ToolResult, handler)`，异步 `BoxFut`，返回"放行 / 改写入参 / 拦截 / 改写结果"。
    2. 接到 runtime 既有 `BeforeToolCallLayer` / `AfterToolResultLayer`。
    3. 错误语义：hook 失败 → 工具失败（fail-safe），不让错误被静默吞掉。
- **验收**：Rust 侧测试：拦截一个工具调用、改写一次结果；原有 runtime 测试不回归。

### [ ] T5.3 codemode host function：hooks

- **依赖**：T5.2、T1.1
- **做什么**：
    1. monty：增加 `on(event, handler)`（用 `function-call` 挂起机制回调脚本函数）。
    2. JS：WIT 增加 `register-hook`，JS 门面暴露 `on(...)`。
    3. 权限 `hooks`；超时 / 取消；脚本卸载时注销钩子。
- **验收**：两个引擎各一条"脚本挂钩子"测试。

### [ ] T5.4 端到端：脚本改 agent 行为

- **依赖**：T5.3
- **做什么**：写一个脚本：`on("tool_call")` 阻止危险命令 / 改写参数；`on("tool_result")` 脱敏；`tool_register` 增加一个工具。两种引擎各跑一遍。
- **验收**：agent 实际行为被改变（不是只发通知）；有正反用例。

---

## 6. Backlog（本清单之外，不阻塞）

- 扩展点 S3–S5：`command` / `ui` 扩展点、runtime 事件面补齐（`context` / `message_end` / `turn_end`）、provider / MCP / resources。
- 分支摘要（依赖会话树）、`/resume` / `/export` / `/fork`。
- coreutils 命令补充（`sed` / `awk` / `grep` / `find` / `xargs`）。
- 见 [`docs/roadmap.md`](docs/roadmap.md)。

---

## 7. 进度日志

> 每完成一个任务追加一行：`YYYY-MM-DD · T1.1 · 一句话结果（含验证命令）`。

| 日期       | 任务 | 结果                                                                                                                                 |
|------------|------|--------------------------------------------------------------------------------------------------------------------------------------|
| 2026-10-17 | T0   | 决策写入 `docs/codemode.md` §0.9 / 0.10；`docs/sdk.md` 改为落在 plugin-sdk；本清单创建                                               |
| 2026-10-18 | T1.1 | 新增引擎无关的 `tower-agent-codemode-core`：语言/权限/对象安全能力面/结构化错误；6 个单测通过；全 workspace fmt + clippy + test 全绿 |
| 2026-10-18 | T1.2 | monty 通过注入的 `CapabilitySurface` 接入 coreutils：UTF-8 JSON `exec`、预载包装、`exec` 权限拒绝；monty 22 测试及真实 `cat`/`wc` 端到端通过 |
| 2026-10-18 | T1.3 | JS WIT/门面/宿主接入 `tool_call`、`tool_list`、`tool_register`、`tool_unregister`；宿主工具注入、权限门禁、注册工具后续调用测试通过；组件已重新生成 |
| 2026-10-18 | T1.4 | 新增 `capability_parity` 双引擎矩阵：exec / fs / tool_call / tool_register / state / log / ui 全部通过；补齐 JS state/log/ui 与 fs 权限 |
| 2026-10-18 | T2.1 | `codemode-core` 新增 `PromptBuilder`：由能力集、挂载、限额与语言生成 description / instructions；Python / JS / 能力关闭快照通过 |
| 2026-10-18 | T2.2 | 新增 `--codemode-engine monty\|js`、`codemode.engine` 有效配置与 `ScriptMetadata.language`；脚本 > CLI > 配置 > 默认 JS，CLI 50 测试通过 |
| 2026-10-18 | T3.1 | 删除 `mods` feature；门面与 CLI 统一为 `codemode` + `codemode-python` + `codemode-js`；默认、无默认及单引擎组合全部编译通过 |
| 2026-10-18 | T3.2 | CLI 新增 `--no-codemode` 并与 `--codemode` 互斥；删除 `TPA_NO_MODS`，`--no-mods` 不再识别；帮助/二进制测试通过 |
