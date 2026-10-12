# tower-agent

> 一个 **tower-native** 的通用 Agent 框架 —— pi（TypeScript agent 框架）的 Rust 重写。

[English](README-en.md)

---

### 项目简介

`tower-agent` 是一个用 Rust 编写、以 [tower](https://github.com/tower-rs/tower) 的
`Service` / `Layer` 模型为骨架的通用 Agent 框架。它把 **pi**（TypeScript 的 agent 框架）
的可观察行为搬到 Rust，并在此基础上做 tower-native 重构：provider 调用、工具执行、RPC
等「请求 → 响应」边界共享同一套中间件（超时、重试、限流、并发、背压、遥测）；而流式事件、
订阅通知与会话状态走 `Stream` 与编排层，不硬塞进通用中间件。

定位类似 [`tower-mcp`](https://crates.io/crates/tower-mcp) / `tower-llm`：可被 **Axum / Tonic / CLI / TUI / WebUI** 等宿主集成，也能作为库单独使用。

### 特性亮点

- **tower-native**：`Agent` 实现 `Service<AgentRequest>`，可用 `ServiceBuilder` + 任意
  `Layer` 组合；同时提供 **有状态会话**、 **无状态 Service** 与 **池化 Service**三种形态。
- **双响应形态**：非流式 `AgentResponse`（= 流式事件的 fold）与流式 `AgentEventStream`
  （`BoxStream<AgentEvent>`），直接对接 TUI / SSE。
- **多 provider**：Anthropic Messages、OpenAI Chat Completions（含兼容端点）、
  OpenAI Responses、Google Generative AI、DeepSeek；统一的 `Models` 路由与目录刷新。
- **七个内置工具**：`read` / `shell` / `edit` / `write` / `grep` / `find` / `ls`，
  每个工具本身就是一个可组合的 tower `Layer`（`shell` 内嵌 [brush](https://github.com/reubeno/brush)
  ——bash 兼容的 Rust shell，在进程内解释执行，跨平台行为一致）。
- **三种插件形态**：静态（编译期 tower `Layer`）、动态（stabby `cdylib`，运行时加载 / 卸载）、
  热重载（monty 脚本，跑在 wasmtime 沙箱里），覆盖从「零开销内置」到「热加载 mods」的整条光谱。
- **mods 模式**：让 LLM 自己编写、热重载、卸载 monty 脚本，从而动态扩展宿主能力、注册工具、贡献 UI。
- **稳定协议**：帧格式 / 严格 CBOR 子集 / RPC 信封（`tower-agent-protocol`）。

### 架构

`tower-agent` 是一个 facade，按层再导出其余 crate；所有内部依赖统一走 workspace 依赖。

| Crate                    | 职责                                                                                                  |
|--------------------------|-------------------------------------------------------------------------------------------------------|
| `tower-agent`            | **门面**：再导出 core / runtime / provider / tools / plugins（可选 tui/protocol/plugin-sdk）          |
| `tower-agent-core`       | **契约**：`AgentRequest`/`AgentResponse`/`AgentEventStream`、消息与事件、工具边界、Layer 扩展点、错误 |
| `tower-agent-runtime`    | **运行时**：`Agent` 编排循环、队列、事件总线；无状态 / 池化 Service 适配                              |
| `tower-agent-provider`   | **provider**：多 provider LLM Service、`Models` 路由、认证、模型目录                                  |
| `tower-agent-tools`      | **内置工具**：read / shell / edit / write / grep / find / ls                                          |
| `tower-agent-plugins`    | **插件宿主**：注册表、生命周期、HostApi、权限、三类加载器                                             |
| `tower-agent-plugin-sdk` | **插件作者 SDK**：`declare_plugin!`、静态 / 动态 ABI 契约                                             |
| `tower-agent-monty`      | **热重载后端**：内嵌 monty（Python 子集）沙箱插件 + mods 管理器                                       |
| `tower-agent-wasmtime`   | **wasm 沙箱底座**：wasmtime 引擎 / 组件、WASI、资源上限、JIT 缓存与实例池                             |
| `tower-agent-protocol`   | **协议**：帧、严格 CBOR 子集、RPC 信封                                                                |
| `tower-agent-tui`        | **终端前端**：基于 Agent 事件流的差分渲染 TUI                                                         |
| `tower-agent-cli`        | **CLI 宿主**：二进制名 `tower-agent`，以 `Service` / `Layer` 组合装配会话                             |

**分层原则**：`tower-agent-core` 只放契约与类型， **不依赖** runtime / plugins / 前端；
编排循环、队列、事件总线在 `runtime`；provider / 工具 / 插件各自独立成 crate。

### 三种插件形态

| 维度     | 静态插件                    | 动态插件                    | 热重载插件                      |
|----------|-----------------------------|-----------------------------|---------------------------------|
| 载体     | Rust crate（编译进宿主）    | `cdylib`（`.so` / `.dll`）  | monty 脚本（Python 子集）       |
| 组合方式 | `tower::Layer` 叠加         | stabby ABI 导出入口         | 内嵌 wasm 组件（wasmtime 执行） |
| 加载     | 编译期                      | `libloading` 运行时加载     | 运行时编译 + 原子换绑           |
| 卸载     | 不卸载（随进程）            | 支持卸载                    | 支持热替换                      |
| 调用开销 | 零（直接函数调用）          | 一次间接调用 + ABI 编解码   | 编译后接近原生                  |
| 典型用途 | 内置 tool、provider、观测层 | 沙箱、记忆、持久化、codemod | `mods`、用户脚本、轻量扩展      |

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
| `-h`, `--help` · `-v`, `--version`  | 帮助 / 版本                                                 |

**环境变量**

| 变量          | 作用                                              |
|---------------|---------------------------------------------------|
| `TPA_WORKERS` | runtime worker 数（默认 `available_parallelism`） |
| `TPA_NO_MODS` | 关闭 mods 宿主（monty 解释器默认内嵌启用）        |

### 仓库结构

```text
tower-agent/
├── crates/                  # 工作区各 crate（见上方架构表）
├── docs/                    # 设计文档（总设计 / 架构 / 插件 / codemode）
├── examples/plugin-example/ # 动态插件（cdylib）示例
└── wasm/                    # 独立的 wasm 组件 workspace
    ├── monty-sandbox/       #   把 monty 解释器编译成 wasm 组件
    ├── coreutils-wasm/      #   codemode 的命令层（uutils multicall，60 命令）
    └── codemode-sandbox/    #   codemode 的 JS 沙箱（rquickjs → wasip2）
```

### 文档

- [`docs/design.md`](docs/design.md)：整体设计 —— tower 模型与 pi 概念的映射、移植路线。
- [`docs/tower-agent-architecture.md`](docs/tower-agent-architecture.md)：目标架构与迁移计划。
- [`docs/plugins.md`](docs/plugins.md)：插件 / 扩展体系（静态 / 动态 / 热重载）。
- [`docs/codemode.md`](docs/codemode.md)：codemode 模式（rquickjs + wasmtime + uutils：脚本与命令都在 wasm 沙箱里跑）。
- [`docs/architecture-review.md`](docs/architecture-review.md)：现状评审。
- [`wasm/monty-sandbox/README.md`](wasm/monty-sandbox/README.md)：wasm 组件接口与再生成流程。

### 开发与测试

```sh
cargo check --offline --all-targets      # 全量类型检查
cargo test  --offline --all --workspace  # 全量测试
cargo clippy --all-features --all-targets

# 重新生成内嵌的 monty wasm 组件（需要 cargo-component / wasm-opt / wasm-tools）
./wasm/monty-sandbox/generate.sh
```

### wasm 产物（不入库，需本地编译）

内嵌进二进制的 wasm 组件 **不进 git**（见 `.gitignore` 的 `# wasm` 段），所以 **全新克隆后第一次
`cargo build` / `cargo test` 之前，必须先把产物编出来**（否则 `include_bytes!` 报
`couldn't read …/assets/*.wasm`）。三个产物、两条链路：

| 产物                                                 | 谁内嵌                 | 怎么编                                               |
|------------------------------------------------------|------------------------|------------------------------------------------------|
| `crates/tower-agent-monty/assets/monty-sandbox.wasm` | `tower-agent-monty`    | `./wasm/monty-sandbox/generate.sh`                   |
| `crates/tower-agent-codemode/assets/coreutils.wasm`  | `tower-agent-codemode` | `./wasm/coreutils-wasm/generate.sh`                  |
| `crates/tower-agent-codemode/assets/codemode.wasm`   | `tower-agent-codemode` | `WASI_SDK=<sdk> ./wasm/codemode-sandbox/generate.sh` |

工具链（只在编 wasm 时要用，日常 `cargo build` 不需要）：

```sh
rustup target add wasm32-wasip2          # 三个产物都要
rustup target add wasm32-wasip1          # monty-sandbox 走 p1 + wasm-opt + 组件化
cargo install cargo-component --locked   # monty-sandbox 与 codemode-sandbox
cargo install wasm-tools --locked        # monty-sandbox（embed/new）
# wasm-opt（binaryen）                    # monty-sandbox 的体积优化
# WASI_SDK（wasi-sdk 24+ 的根目录）       # codemode-sandbox：rquickjs 要编 quickjs 的 C 源码
```

- **coreutils-wasm** 最简单：只要 `wasm32-wasip2`，rustc 直出组件（不需要 wasi-sdk / wasm-opt / wasm-tools）。
- **codemode-sandbox** 需要 `WASI_SDK`（本机可在 `~/.cache/wasi-sdk-*`）；crates.io 上的 `rquickjs-sys`
  自带 quickjs 的 C 源码，所以 **不需要子模块或 vendor**。
- 首次构建某条链路要从 crates.io 拉依赖；之后各 workspace 里的 `Cargo.lock` 已固定版本，可加 `--offline`。
- 细节（接口、上限、重生成流程）见各 workspace 的 README：`wasm/*/README.md`。

### 许可证

采用 **MIT OR Apache-2.0** 双许可。详见 [`LICENSE-MIT`](LICENSE-MIT) 与
[`LICENSE-APACHE`](LICENSE-APACHE)。
