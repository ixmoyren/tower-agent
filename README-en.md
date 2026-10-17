# tower-agent

> A **tower-native** general-purpose agent framework — a Rust rewrite of pi (a TypeScript agent framework).

---

### Overview

`tower-agent` is a general-purpose agent framework written in Rust and built around the
[tower](https://github.com/tower-rs/tower) `Service` / `Layer` model. It ports the
observable behavior of **pi** (a TypeScript agent framework) to Rust, then reshapes the
architecture to be tower-native: request/response boundaries — provider calls, tool
execution, RPC — share one middleware stack (timeout, retry, rate limiting, concurrency,
backpressure, telemetry), while streaming events, subscriptions and session state stay as
`Stream`s and orchestration logic instead of being forced into generic middleware.

Its positioning is similar to [`tower-mcp`](https://crates.io/crates/tower-mcp) /
`tower-llm`: it can be embedded by hosts such as **axum / tonic / CLI / TUI / web**, or
used directly as a library.

### Highlights

- **tower-native**: `Agent` implements `Service<AgentRequest>` and composes with
  `ServiceBuilder` + any `Layer`. Three shapes are provided — **stateful sessions**,
  a **stateless `Service`**, and a **pooled `Service`**.
- **Two response shapes**: non-streaming `AgentResponse` (a fold of the stream) and the
  streaming `AgentEventStream` (`BoxStream<AgentEvent>`) — ready for TUI and SSE.
- **Multiple providers**: Anthropic Messages, OpenAI Chat Completions (incl. compatible
  endpoints), OpenAI Responses, Google Generative AI, and DeepSeek, behind a unified
  `Models` router with catalog refresh.
- **Seven built-in tools**: `read` / `shell` / `edit` / `write` / `grep` / `find` / `ls`.
  Each tool is itself a composable tower `Layer` (`shell` embeds
  [brush](https://github.com/reubeno/brush), a bash-compatible Rust shell, and interprets
  commands in-process for consistent cross-platform behavior).
- **Three plugin forms**: static (compile-time tower `Layer`), dynamic (stabby `cdylib`
  loaded/unloaded at runtime), and hot-reload (monty scripts running inside a wasmtime
  sandbox) — spanning the whole spectrum from zero-overhead built-ins to hot-loaded mods.
- **mods mode**: let the LLM write, hot-reload, and unload monty scripts that extend host
  capabilities, register tools, and contribute UI.
- **Stable protocol**: framing, a strict CBOR subset, and RPC envelopes (`tower-agent-protocol`).

### Architecture

`tower-agent` is a facade that re-exports the other crates layer by layer; all internal
dependencies are declared once through workspace dependencies.

| Crate                    | Responsibility                                                                                                                     |
|--------------------------|------------------------------------------------------------------------------------------------------------------------------------|
| `tower-agent`            | **Facade**: re-exports core / runtime / provider / tools / plugins (optional tui/protocol/plugin-sdk)                              |
| `tower-agent-core`       | **Contracts**: `AgentRequest`/`AgentResponse`/`AgentEventStream`, messages & events, tool boundary, layer extension points, errors |
| `tower-agent-runtime`    | **Runtime**: the `Agent` loop, queues, event bus; stateless / pooled `Service` adapters                                            |
| `tower-agent-provider`   | **Providers**: multi-provider LLM `Service`s, the `Models` router, auth, model catalog                                             |
| `tower-agent-tools`      | **Built-in tools**: read / shell / edit / write / grep / find / ls                                                                 |
| `tower-agent-plugins`    | **Plugin host**: registry, lifecycle, HostApi, permissions, three loaders                                                          |
| `tower-agent-plugin-sdk` | **Plugin author SDK**: `declare_plugin!`, static / dynamic ABI contracts                                                           |
| `tower-agent-monty`      | **Hot-reload backend**: embedded monty (Python subset) sandbox plugins + mods manager                                              |
| `tower-agent-wasmtime`   | **wasm sandbox base**: wasmtime engine/component, WASI, resource limits, JIT cache and instance pool                               |
| `tower-agent-protocol`   | **Protocol**: framing, strict CBOR subset, RPC envelopes                                                                           |
| `tower-agent-tui`        | **Terminal frontend**: differential-rendering TUI over the agent event stream                                                      |
| `tower-agent-cli`        | **CLI host**: binary named `tower-agent`, assembling the session via `Service` / `Layer`                                           |

**Layering rule**: `tower-agent-core` holds only contracts and types and does **not**
depend on runtime / plugins / frontends; the loop, queues and event bus live in `runtime`;
providers, tools and plugins each live in their own crate.

### The three plugin forms

| Aspect      | Static                                   | Dynamic                                | Hot-reload                             |
|-------------|------------------------------------------|----------------------------------------|----------------------------------------|
| Carrier     | Rust crate (compiled in)                 | `cdylib` (`.so` / `.dll`)              | monty script (Python subset)           |
| Composition | `tower::Layer` stacked                   | stabby ABI export entry                | embedded wasm component (wasmtime)     |
| Loading     | compile time                             | `libloading` at runtime                | runtime compile + atomic rebind        |
| Unloading   | none (lives with the process)            | supported                              | hot replacement                        |
| Call cost   | zero (direct call)                       | one indirect call + ABI codec          | near-native after JIT                  |
| Typical use | built-in tools, providers, observability | sandbox, memory, persistence, codemode | `mods`, user scripts, light extensions |

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
| `-h`, `--help` · `-v`, `--version`  | Help / version                                                          |

**Environment variables**

| Variable      | Purpose                                                                 |
|---------------|-------------------------------------------------------------------------|
| `TPA_WORKERS` | Number of runtime workers (defaults to `available_parallelism`)         |
| `TPA_NO_MODS` | Disable the mods host (the embedded monty interpreter is on by default) |

### Repository layout

```text
tower-agent/
├── crates/                  # workspace crates (see the architecture table above)
├── docs/                    # design docs (design / architecture / plugins / codemode)
├── examples/plugin-example/ # dynamic plugin (cdylib) example
└── wasm/                    # independent wasm component workspaces
    ├── monty-sandbox/       #   compiles the monty interpreter into a wasm component
    ├── coreutils-wasm/      #   codemode's command layer (uutils multicall, 60 commands)
    └── rquickjs-sandbox/    #   codemode's JS sandbox (rquickjs -> wasip2)
```

### Documentation

- [`docs/design.md`](docs/design.md): overall design — mapping tower concepts to pi, migration path.
- [`docs/tower-agent-architecture.md`](docs/tower-agent-architecture.md): target architecture and migration plan.
- [`docs/plugins.md`](docs/plugins.md): the plugin/extension system (static / dynamic / hot-reload).
- [`docs/codemode.md`](docs/codemode.md): codemode mode (rquickjs + wasmtime + uutils — script and commands both run inside the wasm sandbox).
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
