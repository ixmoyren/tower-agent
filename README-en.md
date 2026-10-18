# tower-agent

> A general-purpose agent framework rooted in the **tower** ecosystem: inspired by
> pi-agent at the agent-capability level, with composable plugins, Wasmtime sandboxes,
> and codemode for agents that an LLM can dynamically reshape.

[中文](README.md)

---

### Overview

`tower-agent` is **not a Rust rewrite or line-by-line port of pi-agent**. It draws on
pi-agent's capability model and practical lessons around agent loops, tool calling,
sessions, compaction, and extensions, while designing its architecture and implementation
around Rust and [tower](https://github.com/tower-rs/tower) as first-class constraints.
pi-agent is a capability reference, not a compatibility target: `tower-agent` makes no
commitment to matching its APIs, module structure, or behavior item by item.

Provider calls, tool execution, RPC, and other request/response boundaries use the
`Service` / `Layer` model, so they can reuse timeout, retry, rate limiting, concurrency,
backpressure, and telemetry facilities from the tower ecosystem. Streaming events,
subscriptions, and session state stay in `Stream`s and the orchestration layer.

The agent is not a closed, fixed workflow. Static plugins, dynamic plugins, hot-reload
scripts, codemode, and Wasmtime sandboxes form an extensible runtime in which both the host
and the LLM can add tools, adjust behavior, contribute UI, or change how the agent works
inside explicit safety boundaries.

Its positioning is similar to [`tower-mcp`](https://crates.io/crates/tower-mcp) /
`tower-llm`: it can be embedded by hosts such as **axum / tonic / CLI / TUI / web**, or
used directly as a library.

### Highlights

- **tower-native**: `Agent` implements `Service<AgentRequest>` and composes with
  `ServiceBuilder` + any `Layer`. Three shapes are provided — **stateful sessions**,
  a **stateless `Service`**, and a **pooled `Service`**.
- **Rich plugin development**: static plugins, dynamic `cdylib` plugins, and hot-reload
  plugins share one registry, resolution, override, and lifecycle model. They can extend
  tools, providers, middleware, event hooks, UI, storage, and sandbox capabilities.
- **Wasmtime sandbox foundation**: component model, WASI, resource limits, JIT cache, and
  instance pooling form a reusable isolation layer for codemode and other untrusted code.
- **codemode makes the agent mutable**: the LLM can write JavaScript or monty/Python
  scripts, execute them in a sandbox, register or replace tools, attach hooks, contribute
  UI, and dynamically reshape agent behavior through hot reload.
- **Multiple providers**: Anthropic Messages, OpenAI Chat Completions (incl. compatible
  endpoints), OpenAI Responses, Google Generative AI, and DeepSeek, behind a unified
  `Models` router with catalog refresh.
- **Seven built-in tools**: `read` / `shell` / `edit` / `write` / `grep` / `find` / `ls`.
  Each tool is itself a composable tower `Layer` (`shell` embeds
  [brush](https://github.com/reubeno/brush), a bash-compatible Rust shell, and interprets
  commands in-process for consistent cross-platform behavior).
- **Frontend separation**: `tower-agent-ui` owns rendering-independent state reduction,
  commands, and host interfaces; `tower-agent-tui` handles only ratatui and terminal
  integration. GUIs and axum services can consume the UI backend directly.
- **Two response shapes**: non-streaming `AgentResponse` (a fold of the stream) and the
  streaming `AgentEventStream` (`BoxStream<AgentEvent>`) — ready for TUI, SSE, and other
  event consumers.
- **Stable protocol**: framing, a strict CBOR subset, and RPC envelopes (`tower-agent-protocol`).

### Architecture

`tower-agent` is a facade that re-exports the other crates layer by layer; all internal
dependencies are declared once through workspace dependencies.

| Crate                    | Responsibility                                                                                                                     |
|--------------------------|------------------------------------------------------------------------------------------------------------------------------------|
| `tower-agent`            | **Facade and assembly layer**: re-exports core / runtime / provider / tools / plugin and assembles host plugins (optional ui/protocol/plugin-sdk) |
| `tower-agent-core`       | **Contracts**: `AgentRequest`/`AgentResponse`/`AgentEventStream`, messages & events, tool boundary, layer extension points, errors |
| `tower-agent-runtime`    | **Runtime**: the `Agent` loop, queues, event bus; stateless / pooled `Service` adapters                                            |
| `tower-agent-provider`   | **Provider protocol**: messages/models/request schemas, stream services, `Models`, HTTP/SSE, plugin contracts                      |
| `tower-agent-provider-builtin` | **Concrete provider implementations**: Anthropic/OpenAI/Google, compatible configs, and sidecars, registered through the SDK  |
| `tower-agent-tools`      | **Built-in tools**: read / shell / edit / write / grep / find / ls, each as an independent static plugin                          |
| `tower-agent-plugin`    | **General plugin host**: registry, resolution, lifecycle, HostApi, permissions, and extension slots; Provider-agnostic              |
| `tower-agent-plugin-sdk` | **Plugin capability SDK**: static / dynamic ABI, host capabilities, Provider registration and wire, dynamic Provider adaptation     |
| `tower-agent-ui`         | **Frontend-neutral UI backend**: event reduction, session view, slash commands, static UI plugins                                  |
| `tower-agent-monty`      | **codemode Python host**: embedded monty (Python subset) sandbox plugins + hot-reload script manager                               |
| `tower-agent-wasmtime`   | **General Wasmtime sandbox base**: component model, WASI, resource limits, JIT cache, and instance pool                           |
| `tower-agent-protocol`   | **Protocol**: framing, strict CBOR subset, RPC envelopes                                                                           |
| `tower-agent-tui`        | **Terminal frontend**: differential-rendering TUI over the agent event stream                                                      |
| `tower-agent-cli`        | **CLI host**: binary named `tower-agent`, assembling the session via `Service` / `Layer`                                           |

**Layering rule**: `tower-agent-core` holds only contracts and types and does **not**
depend on runtime / plugin / frontends; the loop, queues, and event bus live in `runtime`;
tools provide implementations and plugin declarations, while `plugin` owns registry,
resolution, lifecycle, and extension mechanisms. `tower-agent-ui` is rendering-independent,
and `tower-agent-tui` handles only ratatui and terminal integration.

### The three plugin forms

| Aspect      | Static                                   | Dynamic                                | Hot-reload                             |
|-------------|------------------------------------------|----------------------------------------|----------------------------------------|
| Carrier     | Rust crate (compiled in)                 | `cdylib` (`.so` / `.dll`)              | monty script (Python subset)           |
| Composition | `tower::Layer` stacked                   | stabby ABI export entry                | embedded wasm component (wasmtime)     |
| Loading     | compile time                             | `libloading` at runtime                | runtime compile + atomic rebind        |
| Unloading   | none (lives with the process)            | supported                              | hot replacement                        |
| Call cost   | zero (direct call)                       | one indirect call + ABI codec          | near-native after JIT                  |
| Typical use | built-in tools, providers, observability | sandbox, memory, persistence, codemode | codemode Python scripts, light extensions |

A static plugin implements `StaticPlugin` and can contribute **capabilities**, **tools**,
and three middleware layers (`agent_layers()` / `tool_layers()` / `event_handlers()`).
Dynamic plugins are written with `tower-agent-plugin-sdk`, loaded by the host through
`libloading`, and validated against both the SDK ABI and plugin versions. Hot-reload
plugins are written in monty; the host embeds the interpreter as a wasm component (`wasm/monty-sandbox`) and executes it in a sandbox, where crashes / out-of-bounds /
oversized allocations become traps that never take down the host process.

### Quick start

**Prerequisites**: Rust **nightly** (see `rust-toolchain.toml`), edition 2024.

```sh
# Build (binary name: tower-agent)
cargo build --release

# Interactive TUI
cargo run -p tower-agent-cli

# One-shot prompt
cargo run -p tower-agent-cli -- "summarize this repository"

# Non-interactive mode, prompt read from stdin
echo "explain the orchestration loop in crates/tower-agent-runtime" \
  | cargo run -p tower-agent-cli -- -p
```

**Providers and credentials**: pick one with `--provider`; when omitted, the first
provider with credentials in the environment is used.

| Provider           | `--provider` value  | API key environment variable |
|--------------------|---------------------|------------------------------|
| Anthropic Messages | `anthropic`         | `ANTHROPIC_API_KEY`          |
| OpenAI compatible  | `openai`            | `OPENAI_API_KEY`             |
| OpenAI Responses   | `openai-responses`  | `OPENAI_API_KEY`             |
| Google Generative  | `google` / `gemini` | `GEMINI_API_KEY`             |
| DeepSeek           | `deepseek`          | `DEEPSEEK_API_KEY`           |

**Common CLI options**

| Option                              | Description                                                             |
|-------------------------------------|-------------------------------------------------------------------------|
| `<prompt>...`                       | Positional args joined into a one-shot prompt; reads stdin when omitted |
| `-C`, `--cwd <dir>`                 | Working directory for the tools                                         |
| `-p`, `--print`                     | Non-interactive: process the prompt, then exit                          |
| `--list-tools`                      | List the built-in tools, then exit                                      |
| `--provider <name>`                 | Select the provider                                                     |
| `--api-key <key>`                   | One-off API key (overrides the environment variable)                    |
| `--model <id>`                      | Model id (defaults to the provider's default model)                     |
| `--thinking <level>`                | Thinking level: off / minimal / low / medium / high / xhigh / max       |
| `-t`, `--tools <list>`              | Only enable the listed built-in tools (comma-separated)                 |
| `--exclude-tools <list>`            | Exclude the listed built-in tools (comma-separated)                     |
| `--no-tools` / `--no-builtin-tools` | Disable all built-in tools                                              |
| `--codemode`                         | Explicitly enable the JavaScript codemode sandbox                       |
| `--no-codemode`                      | Disable all codemode hosts, tools, and sandboxes                        |
| `--codemode-engine <monty\|js>`      | Set the default codemode engine (script metadata can override it)       |
| `--codemode-session <name>`          | Use a persistent codemode session                                       |
| `--codemode-mount <spec>`            | Add a codemode mount (repeatable)                                       |
| `-h`, `--help` · `-v`, `--version`  | Help / version                                                          |

**Environment variables**

| Variable      | Purpose                                                                 |
|---------------|-------------------------------------------------------------------------|
| `TPA_WORKERS` | Number of runtime workers (defaults to `available_parallelism`)         |
| `TOWER_AGENT_PROVIDERS` | Override the CLI provider configuration file path              |

### Repository layout

```text
tower-agent/
├── crates/                  # workspace crates (see the architecture table above)
├── docs/                    # design docs (design / architecture / plugins / codemode)
├── examples/                # dynamic plugin and dynamic provider (cdylib) examples
└── wasm/                    # independent wasm component workspaces
    ├── monty-sandbox/       #   compiles the monty interpreter into a wasm component
    ├── coreutils-wasm/      #   codemode's command layer (uutils multicall, 60 commands)
    └── rquickjs-sandbox/    #   codemode's JS sandbox (rquickjs -> wasip2)
```

### Documentation

- [`docs/design.md`](docs/design.md): overall design, agent capability model, and key contracts.
- [`docs/tower-agent-architecture.md`](docs/tower-agent-architecture.md): crate layering, extension model, and evolution direction.
- [`docs/plugins.md`](docs/plugins.md): the plugin/extension system (static / dynamic / hot-reload).
- [`docs/codemode.md`](docs/codemode.md): codemode mode (rquickjs + wasmtime + uutils — script and commands both run inside the wasm sandbox).
- [`docs/sdk.md`](docs/sdk.md): plugin author SDK and extension points shared by Rust plugins and sandboxed scripts.
- [`docs/architecture-review.md`](docs/architecture-review.md): current-state review.
- [`wasm/monty-sandbox/README.md`](wasm/monty-sandbox/README.md): wasm component interface and regeneration.

### Development and testing

```sh
cargo check --offline --all-targets      # type-check everything
cargo test  --offline --all --workspace  # run the full test suite
cargo clippy --all-features --all-targets

# Regenerate the embedded monty wasm component (needs cargo-component / wasm-opt / wasm-tools)
./wasm/monty-sandbox/generate.sh
```

### wasm artifacts (not committed — build them locally)

The wasm components embedded into the binaries are **not tracked by git** (see the `# wasm` block in
`.gitignore`), so **before the first `cargo build` / `cargo test` in a fresh clone you must generate
them** — otherwise `include_bytes!` fails with `couldn't read …/assets/*.wasm`.

| Artifact                                             | Embedded by            | How to build                                         |
|------------------------------------------------------|------------------------|------------------------------------------------------|
| `crates/tower-agent-monty/assets/monty-sandbox.wasm` | `tower-agent-monty`    | `./wasm/monty-sandbox/generate.sh`                   |
| `crates/tower-agent-codemode/assets/coreutils.wasm`  | `tower-agent-codemode` | `./wasm/coreutils-wasm/generate.sh`                  |
| `crates/tower-agent-codemode/assets/codemode.wasm`   | `tower-agent-codemode` | `WASI_SDK=<sdk> ./wasm/rquickjs-sandbox/generate.sh` |

Toolchain (needed only when building wasm, not for everyday `cargo build`):

```sh
rustup target add wasm32-wasip2          # all three artifacts
rustup target add wasm32-wasip1          # monty-sandbox: p1 + wasm-opt + componentize
cargo install cargo-component --locked   # monty-sandbox + rquickjs-sandbox
cargo install wasm-tools --locked        # monty-sandbox (embed/new)
# wasm-opt (binaryen)                    # monty-sandbox size
# WASI_SDK (wasi-sdk 24+)               # rquickjs-sandbox: rquickjs compiles quickjs's C sources
```

- **coreutils-wasm** is the simplest: only `wasm32-wasip2` is needed; rustc emits the component
  directly (no wasi-sdk / wasm-opt / wasm-tools).
- **rquickjs-sandbox** needs `WASI_SDK`; the crates.io `rquickjs-sys` ships quickjs's C sources, so **no submodule or vendoring is required**.
- The first build of a lane pulls dependencies from crates.io; afterwards each workspace's
  `Cargo.lock` pins them, so `--offline` works.
- Per-workspace details: `wasm/*/README.md`.

### License

Licensed under **MIT OR Apache-2.0**. See [`LICENSE-MIT`](LICENSE-MIT) and
[`LICENSE-APACHE`](LICENSE-APACHE).
