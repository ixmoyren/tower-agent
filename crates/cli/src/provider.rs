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
//! `faux` 是离线 provider（不联网）；其余四种
//! （anthropic messages / openai chat completions / openai responses / google generative ai）
//! 都是真实实现，凭据来自各家的环境变量。

use std::time::Duration;

use ai::{
    ANTHROPIC_MESSAGES_API, AnthropicProvider, BoxProviderService, BoxStreamService,
    GOOGLE_GENERATIVE_AI_API, GoogleProvider, InputModality, Model, Models, OPENAI_COMPLETIONS_API,
    OPENAI_RESPONSES_API, OpenAiProvider, OpenAiResponsesProvider, ProviderServiceExt,
    RetryTransport, TimeoutService, http::ReqwestTransport,
};

/// CLI 可选的 provider。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProviderKind {
    /// 离线确定性 provider（默认，不联网，不需要 api key）。
    #[default]
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
    pub const ALL: [ProviderKind; 5] = [
        ProviderKind::Faux,
        ProviderKind::Anthropic,
        ProviderKind::OpenAi,
        ProviderKind::OpenAiResponses,
        ProviderKind::Google,
    ];

    /// 解析 `--provider` 的取值（大小写不敏感）。
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "faux" => Some(Self::Faux),
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
            Self::Faux => "faux",
            Self::Anthropic => "anthropic",
            Self::OpenAi | Self::OpenAiResponses => "openai",
            Self::Google => "google",
        }
    }

    /// API 标识（pi `Api`）。
    #[must_use]
    pub fn api(self) -> &'static str {
        match self {
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
            Self::Faux => "faux",
            Self::Anthropic => "claude-opus-4-8",
            Self::OpenAi | Self::OpenAiResponses => "gpt-5.5",
            Self::Google => "gemini-3.1-pro-preview",
        }
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
                let names: Vec<&str> = ProviderKind::ALL.iter().map(|kind| kind.id()).collect();
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
        ProviderKind::Faux => crate::faux::FauxProvider::new().boxed(),
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
    if kind == ProviderKind::Faux {
        return crate::faux::model();
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

    #[test]
    fn parse_accepts_known_names_and_rejects_others() {
        assert_eq!(ProviderKind::parse("faux"), Some(ProviderKind::Faux));
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
    }

    #[test]
    fn default_kind_is_faux() {
        assert_eq!(ProviderKind::default(), ProviderKind::Faux);
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
}
