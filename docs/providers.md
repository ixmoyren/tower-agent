# Provider 插件

Provider 插件把宿主配置转换成 LLM 流式 Service 和模型目录。Agent 本身继续只依赖
`BoxStreamService`，不依赖具体 provider。

## 内置 provider

`tower-agent-provider-builtin` 注册五个独立静态插件：

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

当前落地的是编译期静态 provider 插件：用户在自己的 CLI/GUI/axum 组合根中注册插件。
从配置文件发现 `.so` / `.dll` 动态 provider，以及独立 sidecar provider，属于后续阶段。

## 边界

- provider 插件不能直接读取 CLI 参数或 storage；凭据由宿主解析后通过
  `ProviderConfig::api_key` 注入。
- `Service::Error` 只表示建立流之前的失败；建立成功后的模型错误写入
  `AssistantEventStream`。
- provider 插件声明 API 和模型元数据，运行时由 `Models` 按 `(provider, api)` 路由。
