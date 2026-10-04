//! 把 CLI 参数映射到 `ai` crate 的真实 provider 栈（design.md 4.3）。
//!
//! 一条 provider 的运行时栈是：
//!
//! ```text
//! Session ── StreamFn ──▶ [TimeoutService] ──▶ provider ──▶ RetryTransport ──▶ ReqwestTransport
//! ```
//!
//! - provider 模块自己负责构造请求、注入 auth（api key）并把响应流翻成事件；
//! - [`RetryTransport`] 负责 429 / 5xx 与可重试传输错误（`Retry-After` 超过上限立即失败）；
//! - [`TimeoutService`] 只覆盖「建立请求」阶段（连接 + 响应头），响应流本身由上层处理；
//! - [`Models`] 注册 `(provider, api) → 服务` 的路由，并把本次模型写进目录，方便检查与刷新。
//!
//! 四种真实 provider（anthropic messages / openai chat completions / openai responses /
//! google generative ai）凭据来自各家的环境变量。`faux` 只是**测试 provider**：它在
//! `ai::provider::faux`（`test-util` feature）里，不会被编进二进制。

use std::time::Duration;

use ai::{
    ANTHROPIC_MESSAGES_API, AnthropicProvider, BoxProviderService, BoxStreamService,
    GOOGLE_GENERATIVE_AI_API, GoogleProvider, InputModality, Model, Models, OPENAI_COMPLETIONS_API,
    OPENAI_RESPONSES_API, OpenAiProvider, OpenAiResponsesProvider, ProviderServiceExt,
    RetryTransport, TimeoutService, http::ReqwestTransport,
};

/// CLI 可选的 provider。
///
/// 这里只有**用户可选**的真实 provider；测试用的 `faux` 变体在 `cfg(test)` 下才存在。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    /// 离线确定性 provider。**仅测试**（`ai::provider::faux` 不编进二进制）。
    #[cfg(test)]
    Faux,
    /// Anthropic Messages（`api.anthropic.com`）。
    Anthropic,
    /// OpenAI Chat Completions（也覆盖兼容端点）。
    OpenAi,
    /// OpenAI Responses（事件类型化流）。
    OpenAiResponses,
    /// Google Generative AI（`generativelanguage.googleapis.com`）。
    Google,
}

impl ProviderKind {
    /// 全部可选 provider（用于帮助文案与校验）。
    pub const ALL: [ProviderKind; 4] = [
        ProviderKind::Anthropic,
        ProviderKind::OpenAi,
        ProviderKind::OpenAiResponses,
        ProviderKind::Google,
    ];

    /// 没显式给 `--provider` 时，按 pi `defaultModelPerProvider` 的顺序挑默认 provider。
    ///
    /// `openai` 与 `openai-responses` 是同一家的两条 API，默认只保留前者。
    pub const DEFAULT_ORDER: [ProviderKind; 3] = [
        ProviderKind::Anthropic,
        ProviderKind::OpenAi,
        ProviderKind::Google,
    ];

    /// 解析 `--provider` 的取值（大小写不敏感）。`faux` 不是可选项，返回 `None`。
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "anthropic" => Some(Self::Anthropic),
            "openai" | "openai-completions" => Some(Self::OpenAi),
            "openai-responses" => Some(Self::OpenAiResponses),
            "google" | "gemini" => Some(Self::Google),
            _ => None,
        }
    }

    /// provider 标识（pi `ProviderId`）。
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            #[cfg(test)]
            Self::Faux => "faux",
            Self::Anthropic => "anthropic",
            Self::OpenAi | Self::OpenAiResponses => "openai",
            Self::Google => "google",
        }
    }

    /// `--provider` 的规范取值。
    ///
    /// 一个 provider 标识可能对应多条 API（`openai` 与 `openai-responses`），
    /// 所以错误提示按取值而不是按 provider 标识来列。
    #[must_use]
    pub fn canonical_name(self) -> &'static str {
        match self {
            #[cfg(test)]
            Self::Faux => "faux",
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::OpenAiResponses => "openai-responses",
            Self::Google => "google",
        }
    }

    /// API 标识（pi `Api`）。
    #[must_use]
    pub fn api(self) -> &'static str {
        match self {
            #[cfg(test)]
            Self::Faux => "faux",
            Self::Anthropic => ANTHROPIC_MESSAGES_API,
            Self::OpenAi => OPENAI_COMPLETIONS_API,
            Self::OpenAiResponses => OPENAI_RESPONSES_API,
            Self::Google => GOOGLE_GENERATIVE_AI_API,
        }
    }

    /// 该 provider 读取凭据的环境变量（faux 不需要，返回 `None`）。
    #[must_use]
    pub fn api_key_env(self) -> Option<&'static str> {
        match self {
            #[cfg(test)]
            Self::Faux => None,
            Self::Anthropic => Some("ANTHROPIC_API_KEY"),
            Self::OpenAi | Self::OpenAiResponses => Some("OPENAI_API_KEY"),
            Self::Google => Some("GEMINI_API_KEY"),
        }
    }

    /// 未显式给 `--model` 时使用的默认模型 id（对齐 pi `defaultModelPerProvider`）。
    #[must_use]
    pub fn default_model_id(self) -> &'static str {
        match self {
            #[cfg(test)]
            Self::Faux => "faux",
            Self::Anthropic => "claude-opus-4-8",
            Self::OpenAi | Self::OpenAiResponses => "gpt-5.5",
            Self::Google => "gemini-3.1-pro-preview",
        }
    }

    /// 环境里是否有这个 provider 的凭据（空串视为没有）。
    #[must_use]
    pub fn has_api_key(self) -> bool {
        self.api_key_env().is_some_and(env_has_api_key)
    }
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id())
    }
}

/// provider 组装失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// `--provider` 的值不认识。
    Unknown(String),
    /// 缺少真实 provider 需要的 API key。
    MissingApiKey {
        /// provider 标识。
        provider: &'static str,
        /// 应该设置的环境变量名。
        env: &'static str,
    },
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(value) => {
                let names: Vec<&str> = ProviderKind::ALL
                    .iter()
                    .map(|kind| kind.canonical_name())
                    .collect();
                write!(f, "未知 provider：{value}（可用：{}）", names.join(" / "))
            }
            Self::MissingApiKey { provider, env } => write!(
                f,
                "provider `{provider}` 缺少 API key：请设置环境变量 {env}"
            ),
        }
    }
}

impl std::error::Error for ProviderError {}

/// provider 栈的可调参数（对齐 pi `StreamOptions` 的 `timeoutMs` / `maxRetries` /
/// `maxRetryDelayMs`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderStackOptions {
    /// 建立请求的超时时长；`None` 表示不加超时层。
    pub timeout: Option<Duration>,
    /// 可重试错误的最大重试次数。
    pub max_retries: u32,
    /// 服务器要求的长延迟超过该值时立即失败（不再重试）。
    pub max_retry_delay: Duration,
}

impl Default for ProviderStackOptions {
    fn default() -> Self {
        Self {
            // OpenAI / Anthropic 的官方 SDK 默认 10 分钟。
            timeout: Some(Duration::from_secs(600)),
            // 两家 SDK 的默认重试次数。
            max_retries: 2,
            // pi `maxRetryDelayMs` 默认 60 秒。
            max_retry_delay: Duration::from_secs(60),
        }
    }
}

/// 一次组装好的 provider：注册表 + 模型 + 已装箱的服务。
pub struct ProviderSetup {
    /// `(provider, api) → 服务` 的路由与模型目录。
    pub models: Models,
    /// 本次会话使用的模型。
    pub model: Model,
    /// 本次会话使用的（已装箱）provider 服务。
    pub provider: BoxStreamService,
}

impl ProviderSetup {
    /// 交给 [`crate::session::Session`] 的流服务。
    #[must_use]
    pub fn stream_fn(&self) -> BoxStreamService {
        self.provider.clone()
    }
}

/// 按 `--provider` / `--model` 组装 provider 栈。
///
/// 真实 provider 的 api key 从各自的环境变量读取；缺失时返回
/// [`ProviderError::MissingApiKey`]。
pub fn build(
    kind: ProviderKind,
    model_id: Option<&str>,
    options: ProviderStackOptions,
) -> Result<ProviderSetup, ProviderError> {
    let api_key = api_key_from_env(kind);
    build_with_api_key(kind, model_id, options, api_key.as_deref())
}

/// 从环境变量读取某个 provider 的 api key（空串视为未设置）。
fn api_key_from_env(kind: ProviderKind) -> Option<String> {
    let name = kind.api_key_env()?;
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 环境变量是否有非空取值。
fn env_has_api_key(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| !value.trim().is_empty())
}

/// 没显式给 `--provider` 时的默认 provider：按 [`ProviderKind::DEFAULT_ORDER`] 挑第一个在
/// 环境里有凭据的；一个都没有时返回 `None`，由调用方给出提示。
#[must_use]
pub fn default_kind_from_env() -> Option<ProviderKind> {
    ProviderKind::DEFAULT_ORDER
        .into_iter()
        .find(|kind| kind.has_api_key())
}

/// 组装 provider 栈，api key 由调用方注入（便于无环境依赖地测试）。
///
/// 真实 provider 收到 `None` / 空串时返回 [`ProviderError::MissingApiKey`]；
/// faux 忽略这个参数。
pub fn build_with_api_key(
    kind: ProviderKind,
    model_id: Option<&str>,
    options: ProviderStackOptions,
    api_key: Option<&str>,
) -> Result<ProviderSetup, ProviderError> {
    let model_id = model_id.unwrap_or_else(|| kind.default_model_id());
    let model = model_for(kind, model_id);

    let mut models = Models::new();
    models.register_models(kind.id(), vec![model.clone()]);

    let provider = match kind {
        #[cfg(test)]
        ProviderKind::Faux => ai::provider::faux::FauxProvider::new().boxed(),
        kind => {
            let env = kind.api_key_env().expect("真实 provider 一定有环境变量名");
            let api_key = api_key
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or(ProviderError::MissingApiKey {
                    provider: kind.id(),
                    env,
                })?;
            let transport = RetryTransport::new(
                ReqwestTransport::new(),
                options.max_retries,
                options.max_retry_delay,
            );
            let service: BoxProviderService = match kind {
                ProviderKind::Anthropic => AnthropicProvider::new(transport)
                    .with_api_key(api_key)
                    .boxed_provider(),
                ProviderKind::OpenAi => OpenAiProvider::new(transport)
                    .with_api_key(api_key)
                    .boxed_provider(),
                ProviderKind::OpenAiResponses => OpenAiResponsesProvider::new(transport)
                    .with_api_key(api_key)
                    .boxed_provider(),
                ProviderKind::Google => GoogleProvider::new(transport)
                    .with_api_key(api_key)
                    .boxed_provider(),
                #[cfg(test)]
                ProviderKind::Faux => unreachable!("faux 在上一层已处理"),
            };
            match options.timeout {
                Some(duration) => TimeoutService::new(service, duration).boxed_provider(),
                None => service,
            }
        }
    };

    models.register_boxed(kind.id(), kind.api(), provider.clone());

    Ok(ProviderSetup {
        models,
        model,
        provider,
    })
}

/// 为一个 provider 构造模型元数据。
///
/// 模型目录刷新（`Models::refresh`）尚未接入各家的目录接口，所以这里先给出保守的
/// 占位元数据：文本 + 图片输入、支持 reasoning、200k 上下文 / 8192 输出。
#[must_use]
pub fn model_for(kind: ProviderKind, id: &str) -> Model {
    #[cfg(test)]
    if kind == ProviderKind::Faux {
        return ai::provider::faux::model();
    }
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: kind.api().to_string(),
        provider: kind.id().to_string(),
        input: vec![InputModality::Text, InputModality::Image],
        reasoning: true,
        context_window: 200_000,
        max_tokens: 8_192,
        ..Model::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPTIONS: ProviderStackOptions = ProviderStackOptions {
        timeout: Some(std::time::Duration::from_secs(600)),
        max_retries: 2,
        max_retry_delay: std::time::Duration::from_secs(60),
    };

    #[test]
    fn parse_accepts_known_names_and_rejects_others() {
        assert_eq!(
            ProviderKind::parse("Anthropic"),
            Some(ProviderKind::Anthropic)
        );
        assert_eq!(ProviderKind::parse("openai"), Some(ProviderKind::OpenAi));
        assert_eq!(ProviderKind::parse("gemini"), Some(ProviderKind::Google));
        assert_eq!(
            ProviderKind::parse("openai-responses"),
            Some(ProviderKind::OpenAiResponses)
        );
        assert_eq!(ProviderKind::parse("nope"), None);
        // `faux` 只是测试替身，不是用户可选的 provider。
        assert_eq!(ProviderKind::parse("faux"), None);
    }

    #[test]
    fn all_and_default_order_only_list_real_providers() {
        assert_eq!(ProviderKind::ALL.len(), 4);
        assert!(!ProviderKind::ALL.contains(&ProviderKind::Faux));
        assert_eq!(
            ProviderKind::DEFAULT_ORDER,
            [
                ProviderKind::Anthropic,
                ProviderKind::OpenAi,
                ProviderKind::Google
            ]
        );
    }

    #[test]
    fn faux_setup_registers_route_and_model() {
        let setup = build_with_api_key(
            ProviderKind::Faux,
            None,
            ProviderStackOptions::default(),
            None,
        )
        .expect("faux 不需要 api key");
        assert!(setup.models.has_route("faux", "faux"));
        assert_eq!(setup.model.id, "faux");
        assert_eq!(
            setup.models.get_model("faux", "faux").map(|m| &m.id),
            Some(&setup.model.id)
        );
    }

    #[test]
    fn real_provider_requires_api_key() {
        let result = build_with_api_key(
            ProviderKind::Anthropic,
            None,
            ProviderStackOptions::default(),
            None,
        );
        match result {
            Ok(_) => panic!("缺少 api key 时不应组装成功"),
            Err(error) => assert_eq!(
                error,
                ProviderError::MissingApiKey {
                    provider: "anthropic",
                    env: "ANTHROPIC_API_KEY",
                }
            ),
        }
    }

    #[test]
    fn real_provider_registers_route_and_uses_custom_model() {
        let setup = build_with_api_key(
            ProviderKind::OpenAiResponses,
            Some("gpt-test"),
            ProviderStackOptions::default(),
            Some("sk-test"),
        )
        .expect("带 api key 即可组装");
        assert!(setup.models.has_route("openai", "openai-responses"));
        assert_eq!(setup.model.id, "gpt-test");
        assert_eq!(setup.model.api, "openai-responses");
        assert_eq!(
            setup.models.get_model("openai", "gpt-test").map(|m| &m.id),
            Some(&setup.model.id)
        );
    }

    #[test]
    fn faux_setup_uses_the_offline_provider() {
        let setup =
            build_with_api_key(ProviderKind::Faux, None, OPTIONS, None).expect("faux 不需要凭据");
        assert_eq!(setup.model.id, "faux");
        assert!(setup.models.has_route("faux", "faux"));
        assert_eq!(setup.models.model_count(), 1);
        // 流服务可以取出来交给会话（能克隆即说明装箱成功）。
        let _stream = setup.stream_fn();
    }

    #[test]
    fn unknown_provider_is_rejected_with_the_available_list() {
        assert_eq!(ProviderKind::parse("mistral"), None);
        let message = ProviderError::Unknown("mistral".into()).to_string();
        assert!(message.contains("未知 provider：mistral"), "got: {message}");
        // 错误里的可用列表只列真实 provider（不含测试用的 faux）。
        for name in ["anthropic", "openai", "google"] {
            assert!(message.contains(name), "缺少 `{name}`：{message}");
        }
        assert!(
            !message.contains("faux"),
            "不应该列出测试 provider：{message}"
        );
    }

    #[test]
    fn missing_api_key_is_reported_per_provider() {
        let cases = [
            (ProviderKind::Anthropic, "anthropic", "ANTHROPIC_API_KEY"),
            (ProviderKind::OpenAi, "openai", "OPENAI_API_KEY"),
            (ProviderKind::OpenAiResponses, "openai", "OPENAI_API_KEY"),
            (ProviderKind::Google, "google", "GEMINI_API_KEY"),
        ];
        for (kind, provider, env) in cases {
            let result = build_with_api_key(kind, None, OPTIONS, None);
            match result {
                Ok(_) => panic!("{kind:?} 缺少 api key 时不应组装成功"),
                Err(error) => assert_eq!(
                    error,
                    ProviderError::MissingApiKey { provider, env },
                    "{kind:?} 的错误信息不符"
                ),
            }
        }
    }

    #[test]
    fn real_providers_register_the_route_and_use_the_requested_model() {
        let cases = [
            (ProviderKind::Anthropic, "anthropic", "anthropic-messages"),
            (ProviderKind::OpenAi, "openai", "openai-completions"),
            (ProviderKind::OpenAiResponses, "openai", "openai-responses"),
            (ProviderKind::Google, "google", "google-generative-ai"),
        ];
        for (kind, provider, api) in cases {
            let setup = build_with_api_key(kind, Some("my-model"), OPTIONS, Some("test-key"))
                .expect("带 api key 即可组装");
            assert!(setup.models.has_route(provider, api), "{kind:?} 缺少路由");
            assert_eq!(setup.model.id, "my-model");
            assert_eq!(setup.model.provider, provider);
            assert_eq!(setup.model.api, api);
            assert_eq!(
                setup
                    .models
                    .get_model(provider, "my-model")
                    .map(|m| m.api.as_str()),
                Some(api),
                "{kind:?} 的模型目录没写到正确的 (provider, id)"
            );
        }
    }

    #[test]
    fn default_model_falls_back_to_the_provider_default() {
        let setup = build_with_api_key(ProviderKind::Google, None, OPTIONS, Some("test-key"))
            .expect("带 api key 即可组装");
        assert_eq!(setup.model.id, ProviderKind::Google.default_model_id());
        assert!(
            setup
                .models
                .get_model("google", setup.model.id.as_str())
                .is_some()
        );
    }

    #[test]
    fn model_metadata_is_conservative_for_real_providers() {
        let model = model_for(ProviderKind::Anthropic, "claude-x");
        assert_eq!(model.provider, "anthropic");
        assert!(model.reasoning);
        assert_eq!(model.context_window, 200_000);
        assert_eq!(model.max_tokens, 8_192);
        assert_eq!(model.input.len(), 2);

        let faux = model_for(ProviderKind::Faux, "ignored");
        assert_eq!(faux.id, "faux");
        assert_eq!(faux.api, "faux");
    }
}
