# tower-agent

> 立足 **tower** 生态的通用 Agent 框架：在 agent 能力模型上参考 pi-agent，
> 通过可组合插件、Wasmtime 沙箱与 codemode，构建能够由 LLM 动态调整的 agent。

[English](README-en.md)

---

### 项目简介

`tower-agent` **不是 pi-agent 的 Rust 重写或逐项移植**。它参考 pi-agent 在 agent loop、
tool calling、session、compaction、extension 等方面积累的能力模型与交互经验，但架构和实现
以 Rust 与 [tower](https://github.com/tower-rs/tower) 为第一等约束重新设计。
pi-agent 在这里是能力设计参考，不是兼容性目标；`tower-agent` 不承诺与其 API、模块结构或
行为逐项对齐。

Provider 调用、工具执行、RPC 等「请求 → 响应」边界统一使用 `Service` / `Layer`，因此可以
复用 tower 生态的超时、重试、限流、并发、背压和遥测能力；流式事件、订阅通知与会话状态则由
`Stream` 和编排层承载。Agent 本身不是封闭的固定流程：静态插件、动态插件、热重载脚本、
codemode 和 Wasmtime 沙箱共同构成一套可扩展运行时，让宿主和 LLM 都能在受控边界内增加工具、
调整行为、扩展 UI 或改变 agent 的工作方式。

定位类似 [`tower-mcp`](https://crates.io/crates/tower-mcp) / `tower-llm`：可被 **Axum / Tonic / CLI / TUI / WebUI** 等宿主集成，也能作为库单独使用。

### 特性亮点

- **tower-native**：`Agent` 实现 `Service<AgentRequest>`，可用 `ServiceBuilder` 与任意
  `Layer` 组合；同时提供 **有状态会话**、**无状态 Service** 和 **池化 Service**。
- **丰富的插件开发手段**：静态插件、动态 `cdylib` 插件和热重载插件共享统一注册、解析、
  覆盖与生命周期模型；可扩展工具、Provider、中间件、事件 Hook、UI、存储和沙箱能力。
- **Wasmtime 沙箱底座**：组件模型、WASI、资源上限、JIT 缓存和实例池独立成层，为 codemode
  与其他不可信代码提供一致的隔离边界。
- **codemode：让 agent 可变**：LLM 可以编写 JavaScript 或 monty/Python 脚本，在沙箱中执行、
  注册或替换工具、挂接 Hook、贡献 UI，并通过热重载动态调整 agent 的能力与行为。
- **多 Provider**：Anthropic Messages、OpenAI Chat Completions（含兼容端点）、OpenAI
  Responses、Google Generative AI、DeepSeek 以独立 Provider 插件注册；宿主也可实现
  `ProviderPlugin` 注入自定义 LLM。
- **六个内置工具**：`read` / `edit` / `write` / `grep` / `find` / `ls`，以及独立的动态
  `shell` 插件；六个静态工具本身都是可组合的 tower `Layer`。`shell` 插件内嵌
  [brush](https://github.com/reubeno/brush)，通过动态插件 ABI 提供 `tool.shell`。
- **前后端解耦**：`tower-agent-ui` 提供与渲染无关的状态归约、命令和宿主接口；
  `tower-agent-tui` 只负责 ratatui/终端，GUI 与 axum 服务可以直接消费 UI 后端。
- **双响应形态**：非流式 `AgentResponse`（流式事件的 fold）与流式 `AgentEventStream`
  （`BoxStream<AgentEvent>`），可直接对接 TUI、SSE 或其他事件消费者。
- **稳定协议**：帧格式 / 严格 CBOR 子集 / RPC 信封（`tower-agent-protocol`）。

### 架构

`tower-agent` 是一个 facade，按层再导出其余 crate；所有内部依赖统一走 workspace 依赖。

| Crate                    | 职责                                                                                                  |
|--------------------------|-------------------------------------------------------------------------------------------------------|
| `tower-agent`            | **门面与装配层**：再导出 core / runtime / provider / tools / plugin，并装配宿主插件（可选 ui/protocol/plugin-sdk） |
| `tower-agent-core`       | **契约**：`AgentRequest`/`AgentResponse`/`AgentEventStream`、消息与事件、工具边界、Layer 扩展点、错误 |
| `tower-agent-runtime`    | **运行时**：`Agent` 编排循环、队列、事件总线；无状态 / 池化 Service 适配                              |
| `tower-agent-provider`   | **Provider 协议**：消息/模型/请求/事件、Stream Service、Models、HTTP/SSE、插件契约；不含具体厂商实现 |
| `tower-agent-plugin-providers` | **具体 Provider 实现与插件集合**：Anthropic/OpenAI/Google、兼容配置与 sidecar，通过 SDK 注册 Provider 能力 |
| `tower-agent-tools`      | **内置工具与插件合集**：六个静态工具实现及独立的插件声明                                               |
| `tower-agent-plugin-shell` | **动态 shell 插件**：brush 持久会话，通过 stabby `cdylib` ABI 提供 `tool.shell`                    |
| `tower-agent-plugin`    | **通用插件宿主**：注册表、解析、生命周期、HostApi、权限和 extension slot；不依赖 Provider 领域 |
| `tower-agent-plugin-sdk` | **插件能力 SDK**：静态 / 动态 ABI、Host 能力、Provider、可选 `storage` 契约 |
| `tower-agent-plugin-turso` | **默认存储插件**：实现 SDK storage 契约的嵌入式 Turso 后端 |
| `tower-agent-ui`         | **前端无关 UI 后端**：事件归约、会话视图、斜杠命令、静态 UI 插件；可被 GUI / axum 直接消费              |
| `tower-agent-monty`      | **codemode Python 宿主**：内嵌 monty（Python 子集）沙箱插件 + 热重载脚本管理器                       |
| `tower-agent-wasmtime`   | **通用 Wasmtime 沙箱底座**：组件模型、WASI、资源上限、JIT 缓存与实例池                               |
| `tower-agent-protocol`   | **协议**：帧、严格 CBOR 子集、RPC 信封                                                                |
| `tower-agent-tui`        | **终端渲染前端**：消费 `tower-agent-ui`，负责 ratatui 页面、编辑器与终端事件循环                       |
| `tower-agent-cli`        | **CLI 宿主**：二进制名 `tower-agent`，以 `Service` / `Layer` 组合装配会话                             |

**分层原则**：`tower-agent-core` 只放契约与类型，**不依赖** runtime / plugin / 前端；
编排循环、队列、事件总线在 `runtime`；`tools` 提供工具实现与插件声明，`plugin` 提供注册表、
解析、生命周期与扩展机制；`tower-agent-ui` 承载前端无关的会话状态和命令，
`tower-agent-tui` 只做 ratatui 渲染。门面 crate 不依赖终端前端。

### 三种插件形态

| 维度     | 静态插件                    | 动态插件                    | 热重载插件                      |
|----------|-----------------------------|-----------------------------|---------------------------------|
| 载体     | Rust crate（编译进宿主）    | `cdylib`（`.so` / `.dll`）  | monty 脚本（Python 子集）       |
| 组合方式 | `tower::Layer` 叠加         | stabby ABI 导出入口         | 内嵌 wasm 组件（wasmtime 执行） |
| 加载     | 编译期                      | `libloading` 运行时加载     | 运行时编译 + 原子换绑           |
| 卸载     | 不卸载（随进程）            | 支持卸载                    | 支持热替换                      |
| 调用开销 | 零（直接函数调用）          | 一次间接调用 + ABI 编解码   | 编译后接近原生                  |
| 典型用途 | 内置 tool、provider、观测层 | 沙箱、记忆、持久化、codemode | codemode Python 脚本、轻量扩展  |

静态插件实现 `StaticPlugin`，可贡献 **能力**、 **工具** 以及三层中间件
（`agent_layers()` / `tool_layers()` / `event_handlers()`）。动态插件用
`tower-agent-plugin-sdk` 编写，经宿主 `libloading` 加载并做 ABI + 版本双重校验。
热重载插件用 monty 编写，宿主把解释器编译成 wasm 组件（`wasm/monty-sandbox`）内嵌执行，
崩溃 / 越界 / 内存超限被转成 trap，不波及宿主进程。

### 快速开始

**前置要求**：Rust **nightly**（见 `rust-toolchain.toml`）、edition 2024。

```sh
# 构建（二进制名 tower-agent）
cargo build --release

# 交互式 TUI
cargo run -p tower-agent-cli

# 一次性提示词
cargo run -p tower-agent-cli -- "帮我看看这个仓库的结构"

# 非交互模式，提示词从 stdin 读取
echo "解释一下 crates/tower-agent-runtime 的编排循环" \
  | cargo run -p tower-agent-cli -- -p
```

**provider 与凭据**：用 `--provider` 选择，缺省时按环境里已有的凭据自动挑选。

| Provider           | `--provider` 取值   | API Key 环境变量    |
|--------------------|---------------------|---------------------|
| Anthropic Messages | `anthropic`         | `ANTHROPIC_API_KEY` |
| OpenAI 兼容        | `openai`            | `OPENAI_API_KEY`    |
| OpenAI Responses   | `openai-responses`  | `OPENAI_API_KEY`    |
| Google Generative  | `google` / `gemini` | `GEMINI_API_KEY`    |
| DeepSeek           | `deepseek`          | `DEEPSEEK_API_KEY`  |

**常用命令行选项**

| 选项                                | 说明                                                        |
|-------------------------------------|-------------------------------------------------------------|
| `<prompt>...`                       | 位置参数拼成一次性提示词；省略时从 stdin 读取               |
| `-C`, `--cwd <dir>`                 | 工具工作目录                                                |
| `-p`, `--print`                     | 非交互：处理完提示词后退出                                  |
| `--list-tools`                      | 列出内置工具后退出                                          |
| `--provider <name>`                 | 选择 provider                                               |
| `--api-key <key>`                   | 一次性 API key（覆盖环境变量）                              |
| `--model <id>`                      | 模型 id（默认取 provider 默认模型）                         |
| `--thinking <level>`                | 思考级别：off / minimal / low / medium / high / xhigh / max |
| `-t`, `--tools <list>`              | 只启用列出的内置工具（逗号分隔）                            |
| `--exclude-tools <list>`            | 排除列出的内置工具（逗号分隔）                              |
| `--no-tools` / `--no-builtin-tools` | 禁用全部内置工具                                            |
| `--codemode`                         | 显式启用 JavaScript codemode 沙箱                           |
| `--no-codemode`                      | 关闭全部 codemode 宿主、工具与沙箱                          |
| `--codemode-engine <monty\|js>`      | 设置 codemode 默认引擎（脚本 language 可覆盖）              |
| `--codemode-session <name>`          | 使用持久化 codemode 会话                                    |
| `--codemode-mount <spec>`            | 额外 codemode 挂载（可重复）                                 |
| `--storage <name>`                   | 首次有多个 storage backend 时选择默认项                      |
| `-h`, `--help` · `-v`, `--version`  | 帮助 / 版本                                                 |

多 storage backend 的查看、迁移切换与停用：

```sh
tower-agent storage list
tower-agent storage status
tower-agent storage switch <name>
tower-agent storage disable <name>
tower-agent storage enable <name>
```

运行时只路由到一个活跃 backend；切换会做完整快照复制和校验，失败时保持
原 backend。选择状态独立保存在 `~/.config/tower-agent/storage.toml`，
详见 [`docs/storage.md`](docs/storage.md)。

**环境变量**

| 变量          | 作用                                              |
|---------------|---------------------------------------------------|
| `TPA_WORKERS` | runtime worker 数（默认 `available_parallelism`） |
| `TOWER_AGENT_PROVIDERS` | 覆盖 CLI 的用户 provider 配置文件路径 |
| `TOWER_AGENT_STORAGE` | 覆盖默认 Turso 数据文件路径 |
| `TOWER_AGENT_STORAGE_CONTROL` | 覆盖 storage 选择控制文件路径 |

### 仓库结构

```text
tower-agent/
├── crates/                  # 工作区各 crate（见上方架构表）
├── docs/                    # 设计文档（总设计 / 架构 / 插件 / codemode）
├── examples/                # 动态插件与动态 provider（cdylib）示例
└── wasm/                    # 独立的 wasm 组件 workspace
    ├── monty-sandbox/       #   把 monty 解释器编译成 wasm 组件
    ├── coreutils-wasm/      #   codemode 的命令层（uutils multicall，60 命令）
    └── rquickjs-sandbox/    #   codemode 的 JS 沙箱（rquickjs → wasip2）
```

### 文档

- [`docs/design.md`](docs/design.md)：整体设计、agent 能力模型与关键契约。
- [`docs/tower-agent-architecture.md`](docs/tower-agent-architecture.md)：crate 分层、扩展模型与演进方向。
- [`docs/plugins.md`](docs/plugins.md)：插件 / 扩展体系（静态 / 动态 / JIT 脚本）。
- [`docs/storage.md`](docs/storage.md)：多 storage 插件、单活跃 backend 与迁移切换。
- [`docs/codemode.md`](docs/codemode.md)：codemode —— **agent 可以编程**（LLM 生成脚本、沙箱执行；解释器可选 monty/Python 或 rquickjs/JS）。§0 是 v2 统一提案，§1–§11 是 JS 引擎细节。
- [`docs/sdk.md`](docs/sdk.md)：插件作者 SDK 与扩展点设计；Rust 插件和沙箱脚本共享能力、Hook 与注册语义。
- [`docs/roadmap.md`](docs/roadmap.md)：**未完成事项**的单一入口（以及建议顺序）。
- [`docs/architecture-review.md`](docs/architecture-review.md)：现状评审。
- [`wasm/monty-sandbox/README.md`](wasm/monty-sandbox/README.md)：wasm 组件接口与再生成流程。

### 开发与测试

```sh
cargo check --offline --all-targets      # 全量类型检查
cargo test  --offline --all --workspace  # 全量测试
cargo clippy --all-features --all-targets

# 重新生成内嵌的 monty wasm 组件（需要 cargo-component / wasm32-wasip2）
./wasm/monty-sandbox/generate.sh
```

### wasm 产物（不入库，需本地编译）

内嵌进二进制的 wasm 组件 **不进 git**（见 `.gitignore` 的 `# wasm` 段），所以 **全新克隆后第一次
`cargo build` / `cargo test` 之前，必须先把产物编出来**（否则 `include_bytes!` 报
`couldn't read …/assets/*.wasm`）。三个产物、三条生成脚本：

| 产物                                                 | 谁内嵌                 | 怎么编                                               |
|------------------------------------------------------|------------------------|------------------------------------------------------|
| `crates/tower-agent-monty/assets/monty-sandbox.wasm` | `tower-agent-monty`    | `./wasm/monty-sandbox/generate.sh`                   |
| `crates/tower-agent-codemode/assets/coreutils.wasm`  | `tower-agent-codemode` | `./wasm/coreutils-wasm/generate.sh`                  |
| `crates/tower-agent-codemode/assets/codemode.wasm`   | `tower-agent-codemode` | `WASI_SDK=<sdk> ./wasm/rquickjs-sandbox/generate.sh` |

工具链（只在编 wasm 时要用，日常 `cargo build` 不需要）：

```sh
rustup target add wasm32-wasip2          # 三个产物都要
cargo install cargo-component --locked   # monty-sandbox 与 rquickjs-sandbox
# WASI_SDK（wasi-sdk 24+ 的根目录）       # rquickjs-sandbox：rquickjs 要编 quickjs 的 C 源码
```

- **monty-sandbox** 与 **coreutils-wasm** 都使用 `wasm32-wasip2`，由 rustc 直出组件；
  monty-sandbox 额外需要 `cargo-component` 生成 WIT 绑定，并依靠 release profile 压缩体积。
- **rquickjs-sandbox** 需要 `WASI_SDK`（本机可在 `~/.cache/wasi-sdk-*`）；crates.io 上的 `rquickjs-sys`
  自带 quickjs 的 C 源码，所以 **不需要子模块或 vendor**。
- 首次构建某条链路要从 crates.io 拉依赖；之后各 workspace 里的 `Cargo.lock` 已固定版本，可加 `--offline`。
- 细节（接口、上限、重生成流程）见各 workspace 的 README：`wasm/*/README.md`。

### 许可证

采用 **MIT OR Apache-2.0** 双许可。详见 [`LICENSE-MIT`](LICENSE-MIT) 与
[`LICENSE-APACHE`](LICENSE-APACHE)。
