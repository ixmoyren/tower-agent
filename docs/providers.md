# Provider 插件

Provider 插件把宿主配置转换成 LLM 流式 Service 和模型目录。Agent 本身继续只依赖
`BoxStreamService`，不依赖具体 provider。

依赖边界：

- `tower-agent-provider`：消息、模型、请求/事件 schema、Stream Service、Models、HTTP/SSE、ProviderPlugin 契约。
- `tower-agent-plugin-providers`：Anthropic/OpenAI/Google 等具体协议实现、faux 测试实现，以及 OpenAI-compatible / Sidecar 等具体 Provider 插件。
- `tower-agent-plugin`：与 Provider 无关的通用插件宿主；负责 Static / Dynamic / JIT 的注册、解析、覆盖和生命周期。
- `tower-agent-plugin-sdk/provider`：SDK 提供的 Provider 能力，包括 `ProviderPlugin` 注册适配、Sidecar / 动态 Provider 的 JSONL wire，以及 `DynamicProviderPlugin` 桥接。
- Provider 本身不是特殊插件类型：具体实现就是一个 Static、Dynamic 或 JIT 插件，在其安装过程中通过 SDK 注册 `Arc<dyn ProviderPlugin>` extension。

## 内置 provider

`tower-agent-plugin-providers` 注册五个独立静态插件：

| 插件 | API | 凭据 |
|---|---|---|
| `provider.anthropic` | `anthropic-messages` | `ANTHROPIC_API_KEY` |
| `provider.openai` | `openai-completions` | `OPENAI_API_KEY` |
| `provider.openai-responses` | `openai-responses` | `OPENAI_API_KEY` |
| `provider.google` | `google-generative-ai` | `GEMINI_API_KEY` |
| `provider.deepseek` | `openai-completions` | `DEEPSEEK_API_KEY` |

宿主通过 `HostPlugins::assemble()` 取得 `ProviderRegistry`。CLI 的 provider 选择、`/login`
和 `/model` 都读取该注册表。

## 自定义 Rust provider

开发 Provider 插件时在插件 crate 中启用 SDK 的 `provider` feature：

```toml
tower-agent-plugin-sdk = { version = "0.1.0", features = ["provider"] }
```

它会把 `tower-agent-plugin-sdk::provider`、ProviderPlugin 契约和
`ProviderPluginRegistration` 暴露出来；不使用该 feature 时不引入 Provider 能力适配器。

```rust
use std::sync::Arc;
use semver::Version;
use tower_agent::provider::{
    ProviderBuild, ProviderConfig, ProviderDescriptor, ProviderPlugin,
};

struct MyProvider;

impl ProviderPlugin for MyProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new("my-llm", "My LLM", Version::new(1, 0, 0))
            .api("my-api")
            .default_model("my-model")
    }

    fn build(&self, config: &ProviderConfig) -> Result<ProviderBuild, _> {
        // 构造 Service<StreamRequest, Response = AssistantEventStream>，
        // 返回 API → service 以及支持的模型。
        todo!()
    }
}

let providers = tower_agent::assembly::HostPlugins::new(cwd)
    .with_provider(Arc::new(MyProvider))
    .providers()?;
```

`with_provider` 使用 `provider.<id>` 作为插件名。如果用户插件的 id 与内置 provider 相同，
装配层会跳过同名内置插件，实现单个 provider 的替换。

对于 OpenAI Chat Completions 兼容服务，可直接使用
`OpenAiCompatibleProviderPlugin`，只需提供 provider id、base URL、模型和可选凭据环境变量。

## 配置文件发现

CLI 会加载以下文件，后者覆盖前者：

```text
<config>/tower-agent/providers.json
<cwd>/.tower-agent/providers.json
```

可用 `TOWER_AGENT_PROVIDERS` 指向单个配置文件，便于容器和测试部署。

```json
{
  "providers": {
    "local": {
      "kind": "openai-compatible",
      "name": "Local LLM",
      "base_url": "http://127.0.0.1:8080/v1",
      "default_model": "qwen-local",
      "api_key_env": "LOCAL_LLM_API_KEY",
      "priority": 900
    }
  }
}
```

之后可直接使用：

```sh
tower-agent --provider local --model qwen-local
```

配置中的 provider 会经过同一 `HostPlugins::with_provider` 路径注册，因此也可以用项目配置
按相同 id 覆盖用户级 provider 定义。

Provider 插件与工具插件共享同一个 `tower-agent-plugin::PluginRegistry`：

- 编译期 Rust provider 和 sidecar adapter 以 `PluginKind::Static` 注册。
- `.so` / `.dll` / `.dylib` provider 以 `PluginKind::Dynamic` 注册。
- 同名 Provider 插件仍遵循 `Static < Dynamic < JIT` 的通用覆盖规则。

`ProviderRegistry` 只保存解析完成后的运行时 provider 对象，用于构建 `Models` 和
`BoxStreamService`；插件发现、覆盖、依赖解析和 shadow 记录都由
`tower-agent-plugin` 负责，不存在第二套插件注册表。

当前已完成编译期 Rust provider 插件、OpenAI-compatible 配置 provider，以及原生动态 provider。

## Sidecar provider

配置中也可以使用 `kind: "sidecar"` 启动任意语言的 provider 进程：

```json
{
  "providers": {
    "sidecar": {
      "kind": "sidecar",
      "name": "My Provider",
      "default_model": "my-model",
      "command": ["./my-provider", "--stdio"],
      "api_key_env": "MY_PROVIDER_API_KEY",
      "working_directory": "tools/provider",
      "env": {
        "MY_MODE": "agent"
      },
      "priority": 700
    }
  }
}
```

每次请求会启动一次 command。Host 向 stdin 写一行 JSON：

```json
{
  "protocol_version": 1,
  "model": { "...": "..." },
  "messages": [],
  "temperature": null,
  "max_tokens": null,
  "session_id": null,
  "metadata": null
}
```

Sidecar 向 stdout 输出一行一个 JSON event：

```json
{"type":"start"}
{"type":"text_delta","text":"hello"}
{"type":"thinking_delta","text":"reasoning"}
{"type":"done","text":"final answer"}
{"type":"error","message":"provider failed"}
```

`done` 也可以携带完整 `AssistantMessage`：

```json
{"type":"done","message":{"content":[],"api":"command-sidecar","provider":"sidecar","model":"my-model","response_model":null,"response_id":null,"provider_thinking_level":null,"thinking_level":null,"usage":{"input":0,"output":0,"cache_read":0,"cache_write":0,"cache_write_1h":null,"reasoning":null,"total_tokens":0,"cost":{"input":0.0,"output":0.0,"cache_read":0.0,"cache_write":0.0,"total":0.0}},"stop_reason":"Stop","deferred":null,"error_message":null,"raw_stop_reason":null,"end_turn":null,"timestamp":0}}
```

Host 提供以下环境变量：

```text
TOWER_AGENT_PROVIDER_ID
TOWER_AGENT_PROVIDER_MODEL
TOWER_AGENT_PROVIDER_API_KEY   # 只在解析到凭据时设置
```

## Native dynamic provider

`kind: "dynamic"` 会从配置中的 `library` 加载 `.so` / `.dll` / `.dylib`：

```json
{
  "providers": {
    "dynamic": {
      "kind": "dynamic",
      "library": "/absolute/path/libmy_provider.so",
      "default_model": "my-model",
      "priority": 650,
      "api_key_env": "MY_PROVIDER_API_KEY",
      "metadata": {
        "endpoint": "local"
      }
    }
  }
}
```

动态库执行在进程内，拥有与 Host 相同的权限，只应加载可信二进制。

动态库使用 `tower-agent-plugin-sdk` 的通用动态插件 ABI，不再导出 Provider 专用 symbol。
插件 manifest 必须提供 `provider.<id>`：

```rust
extern "C" fn provider_manifest() -> AbiManifest {
    AbiManifest {
        name: AbiString::from("provider.dynamic"),
        // ...
        provides: [AbiCapability {
            namespace: AbiString::from("provider"),
            name: AbiString::from("dynamic"),
            // ...
        }]
        .into_iter()
        .collect(),
    }
}

declare_dynamic_plugin!(
    provider_manifest,
    provider_init,
    provider_call,
    provider_shutdown
);
```

`provider_call` 收到通用 `AbiToolCall`，其 `args` 包含：

```json
{
  "provider_config": {},
  "request": {}
}
```

插件返回 `AbiToolResult`。成功时 `content` 包含一行一个 provider JSONL event：

```text
{"type":"text_delta","text":"hello"}
{"type":"done","text":"hello"}
```

动态适配器位于 `tower-agent-plugin-sdk` 的 `provider::dynamic`，需要同时启用 `provider`
和 `dynamic` feature。当前动态插件 ABI v1 是调用返回后批量读取 JSONL；Sidecar ABI
支持实时 stdout 流。

示例动态 provider 位于 `examples/provider-dyn`。它同时启用 SDK 的 `dynamic` 和
`provider` feature，并通过 `provider::wire` 的统一类型生成 JSONL 响应，而不是手写
Provider 专用 ABI：

```sh
cargo build --manifest-path examples/provider-dyn/Cargo.toml
TPA_PROVIDER_DYNAMIC_EXAMPLE=examples/provider-dyn/target/debug/libtpa_provider_dynamic_example.so \
  cargo test -p tower-agent-plugin-sdk --features dynamic,provider provider::dynamic::tests
```

## 边界

- provider 插件不能直接读取 CLI 参数或 storage；凭据由宿主解析后通过
  `ProviderConfig::api_key` 注入。
- `Service::Error` 只表示建立流之前的失败；建立成功后的模型错误写入
  `AssistantEventStream`。
- provider 插件声明 API 和模型元数据，运行时由 `Models` 按 `(provider, api)` 路由。
