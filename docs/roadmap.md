# 未完成事项（Roadmap）

| 项目 | 值                                                                                                                                                                    |
|------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 状态 | 快照 2026-10-14；**本文档是"还没做"的单一入口**，做完就勾掉并把细节写回对应设计文档                                                                                   |
| 关联 | [`design.md`](./design.md)（总体设计）、[`plugins.md`](./plugins.md)（插件体系）、[`codemode.md`](./codemode.md)（§0 是 v2 提案）、[`sdk.md`](./sdk.md)（扩展点提案） |

> **正在执行的任务清单**见根目录 [`TASKS.md`](../TASKS.md)（codemode v2 的 T1.1–T5.4）。本文档保持为全局未完成事项的索引，不跟踪单任务进度。

图例：✅ 已完成（留痕） · ◐ 部分完成 · ❌ 未开始； **优先级** P0 阻塞后续 / P1 影响体验 / P2 锦上添花。

---

## 1. 编码能力（codemode / 扩展点）

**目标**：`mods` 与 `codemode` 合并为统一的 `codemode`（解释器可选），扩展点落在 `tower-agent-plugin-sdk`。
详见 [`codemode.md`](./codemode.md) §0（决策 Q1–Q7 **已全部确定**）与 [`sdk.md`](./sdk.md)。

- ◐ **统一能力层**（`CapabilitySurface`）：`tower-agent-codemode-core` 已落语言 / 权限 / 能力面 / 结构化错误；两个引擎已通过 exec / fs / tool_call / tool_register / state / log / ui 能力矩阵；待钩子与其余能力补齐。 **P0**
- ◐ **统一提示词 + 引擎选择**：`PromptBuilder` 已由能力集生成并有双语言快照；待 **全局默认 + 脚本可覆盖**（Q1）接入 CLI / 配置。 **P0**
- ❌ **合并 feature / CLI**：`mods` + `codemode` → `codemode` + `codemode-python` + `codemode-js`（两个引擎都保留，Q2）；`mod_*` 作者工具并入。 **P0**
- ❌ **脚本仓库**（Q6 = c）：storage 为真源 + 显式导出/导入（每脚本一个文件，元数据永远在 storage）。 **P0**
- ❌ **统一会话与存储**：`ScriptStore`、会话根、快照、权限一套模型。 **P1**
- ❌ **扩展点 v0**（落 `tower-agent-plugin-sdk`）：工具注册 + `tool_call` / `tool_result` 钩子（Q4）；Rust 侧先可用，脚本侧 host function 跟上。 **P0**
- ◐ 脚本权限已扩展为 `ScriptPermissions` 并接入 monty `exec`；待与 `plugins.md` §12 的运行时授权提示打通。 **P1**

## 2. 上下文管理（compaction 剩余项）

✅ 已完成：核心压缩器（阈值自动 / 手动 / 恢复路径）、钩子（`before` / `after` / `failed`）、持久化与续接恢复。
（见 [`design.md`](./design.md) §5 的 `tower_agent_runtime::Compactor` 条目。）

- ❌ **分支摘要**（pi 的 branch summarization）：`/tree` 离开分支时把旧分支总结后注入新分支。 **依赖 §3 的会话树**。 **P1**
- ❌ **真实 token 用量**：`tokens_before` 目前是"字符数 / 4"的估算；应改用 provider 回传的 `usage`（最后一个 assistant 消息的 `input` 更接近真实值）。 **P1**
- ❌ **压缩钩子接入插件体系**：现在是 `tower-agent-runtime::CompactionHook`（运行时 trait），插件作者要用得自己接线；应在 SDK 里统一。 **P1**
- ◐ **压缩条目语义**：pi 把 `CompactionEntry` 作为 transcript 的一个条目（可导出、可回放、`usage` 计入总量）；我们存在 `session.metadata.compaction`（不污染 transcript，但导出/回放看不到"何时压缩过"）。两者取舍待定。 **P2**
- ❌ **扩展钩子对齐**：pi 的 `session_before_compact` 能拿到 `branchEntries` / `willRetry` 等；我们的 `BeforeCompact` 字段更少。 **P2**

## 3. 会话

✅ 已完成：transcript 持久化（`HistoryStore`）、按项目归属（`metadata.cwd`）、`--continue` / `--session <id>` / `--no-session`、`/new` 开新会话、`/session` 显示 id、压缩状态随会话持久化。

- ❌ **会话树**：现在 `message` 表是线性（`AUTOINCREMENT`），pi 是 `id` / `parentId` 的树。 **这是 `/tree`、`/fork`、`/clone`、分支摘要的共同前置**。 **P1**
- ❌ `/resume`（会话选择器：搜索 / 重命名 / 删除 / 排序）。 **P1**
- ❌ `/export` / `/import` / `/share`（会话导出为可读格式 / 导入 / 分享）。 **P2**
- ❌ `/name`（会话重命名）：`SessionRecord.title` 已存在，缺命令入口。 **P2**
- ❌ `/fork` / `/clone`（依赖会话树）。 **P2**
- ◐ 会话删除：storage 有 `delete_session`（连历史一起删），CLI 无入口。 **P2**
- ❌ 每项目一个会话目录（pi 是 `sessions/--<path>--/`）：我们靠 `metadata.cwd` 过滤，行为等价但没有独立目录，导出/清理按项目不方便。 **P2**

## 4. 扩展与协议

✅ 已完成：静态 / 动态 / JIT 三种插件形态、能力注册表（依赖 / 版本 / 覆盖 / `protected`）、生命周期与热更、权限提示、`HostApi` 中介调用、内置工具插件化。

- ❌ **pi extensions 对齐**：见 [`sdk.md`](./sdk.md) §3 的对照表（工具元数据、命令 / 快捷键 / flag、provider / virtual model、MCP、会话控制、UI 交互、资源发现）。 **P1（按 sdk.md 的分期）**
- ❌ **runtime 事件面**：现在 10 个 `AgentEvent`、纯通知；pi 41 个事件且多数可 transform / block / continue。 **P0（限制"脚本改行为"）**
- ❌ **MCP**：客户端、`mcp.json`、`registerMcpServer` 扩展点。 **P2**
- ◐ **跨形态调用**：动态 / JIT 经 `HostApi` 走中介；存储这类"非工具能力"还没有跨形态取值路径（`StoragePlugin::storage()` 只在 Rust 侧）。 **P2**

## 5. 存储

✅ 已完成：`ConfigStore`（user / project 作用域 + 解析优先级）、`ProviderStore`、`SessionStore`、`HistoryStore`、`MemoryStore`；Turso 默认后端（中文 FTS、写队列、`flush`）。

- ❌ **存储插件进组装层**：`StoragePlugin` 已存在但 `HostPlugins` 没注册它，插件也拿不到 `persist.*` / `memory.*` 能力。 **P1**
- ❌ 配置的导出 / 导入（JSON）：便于人工编辑 / 提交 / 迁移。 **P2**
- ◐ 迁移：明确"不做向后兼容"，schema 改动直接重建库（见 `design.md` 相关结论）。 **——**

## 6. provider

✅ 已完成：anthropic / openai（兼容）/ openai-responses / google / deepseek；`Models` 路由 + 目录刷新；认证 / 重试 / 超时 / 观测层。

- ❌ bedrock-converse（需要 SigV4 与事件流签名）、vertex、mistral-conversations、pi-messages、cloudflare。 **P2**
- ❌ 成本 / 计价：pi 有 `token/cost` 目录，我们的 `model_for` 是占位元数据（200k / 8192，无价格）。 **P2**

## 7. 工具与沙箱

✅ 已完成：7 个内置工具（`read` / `shell` / `edit` / `write` / `grep` / `find` / `ls`），`shell` 内嵌 brush；coreutils-wasm **60 个命令**（`COMMANDS`）；monty / JS 两个沙箱。

- ◐ `coreutils-wasm` 缺"管道里的重活"：`sed` / `awk` / `grep` / `find` / `xargs` 都不在 60 个命令里（见 `codemode.md` §3.5 的取舍）。 **P1**
- ❌ 沙箱网络能力（当前无网络，符合"默认安全"）。 **——**
- ❌ 沙箱内的 `fs` 三档位（真实目录 / 内存 VFS / overlay）还没按 `codemode.md` §5 落地到 JS 引擎。 **P2**

## 8. 工程与文档

- ❌ **ratatui 依赖**：根 `Cargo.toml` 里的 `[patch.crates-io]`（把 `ratatui-core` / `ratatui-widgets` / `ratatui-termina` 重定向到 git）是临时手段。 **正解是让 `ixmoyren/ratatui-textarea` 的 fork 依赖同一份 git ratatui**，然后删掉 patch。 **P1**
- ◐ 文档索引：`README.md` 的"文档"一节需要补 `roadmap.md` / `sdk.md`，并把 `codemode.md` 的描述改成"统一后的 codemode（解释器可选）"。 **P1**
- ❌ CI：目前靠本地 `cargo fmt` / `clippy --all --workspace --all-targets` / `test --all --workspace`。 **P2**
- ❌ 压测 / 长会话：compaction 与 transcript 的边界（几千轮之后）没有基准。 **P2**

---

## 9. 建议顺序

| 顺序 | 事项                                                         | 理由                                      |
|------|--------------------------------------------------------------|-------------------------------------------|
| 1    | `codemode.md` §0 / `sdk.md` 评审定稿（含 Q1–Q7 / S-Q1–S-Q5） | 后面所有编码工作都依赖这几个决定          |
| 2    | runtime 事件面补齐（§4 的 10 → "够用"）+ SDK v0              | 没有它，"脚本改行为"无从谈起              |
| 3    | 统一 codemode 能力层 + 提示词（P1/P2）                       | 立刻能验证"两个引擎同一套能力"这层抽象    |
| 4    | 合并 feature / CLI（P3）+ 存储插件进组装层                   | 把新旧路径收敛，避免两套会话/权限长期并存 |
| 5    | 会话树 → `/resume` → 分支摘要                                | 一串强依赖，做完才能对齐 pi 的会话体验    |
| 6    | 其余扩展点与 provider（S3–S5、§6）                           | 增量补齐                                  |
