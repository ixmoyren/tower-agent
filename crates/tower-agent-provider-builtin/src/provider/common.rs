//! Helpers shared by concrete provider implementations.

use serde_json::{Value as JsonValue, json};

use crate::{
    AssistantEventStream, AssistantMessage, AssistantMessageEvent, ContentBlock, ErrorReason,
    Message, Model, StopReason, Tool, Usage, http::HttpHeaders,
};

/// 当前 Unix 毫秒时间戳（pi `Date.now()`）。
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// 一个空的 assistant 消息（`stop_reason` 为 pending）。
pub(crate) fn empty_message(model: &Model) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        provider_thinking_level: None,
        thinking_level: None,
        usage: Usage::default(),
        stop_reason: StopReason::Pending,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: now_ms(),
    }
}

/// 按模型费率计算成本（pi `calculateCost`）。
pub(crate) fn calculate_cost(model: &Model, usage: &mut Usage) {
    let mut rates = (
        model.cost.input,
        model.cost.output,
        model.cost.cache_read,
        model.cost.cache_write,
    );
    // 计价档位：命中阈值最高的一档作用于整个请求。
    let mut threshold: Option<u64> = None;
    for tier in &model.cost.tiers {
        if usage.input >= tier.input_tokens_above
            && threshold.is_none_or(|current| tier.input_tokens_above >= current)
        {
            threshold = Some(tier.input_tokens_above);
            rates = (tier.input, tier.output, tier.cache_read, tier.cache_write);
        }
    }
    let per_million = |tokens: u64, rate: f64| (tokens as f64) / 1_000_000.0 * rate;
    usage.cost.input = per_million(usage.input, rates.0);
    usage.cost.output = per_million(usage.output, rates.1);
    usage.cost.cache_read = per_million(usage.cache_read, rates.2);
    usage.cost.cache_write = per_million(usage.cache_write, rates.3);
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
}

/// 重算总 token 与成本。
pub(crate) fn update_usage_totals(model: &Model, usage: &mut Usage) {
    usage.total_tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
    calculate_cost(model, usage);
}

/// 从 transcript 重放 system prompt 与工具声明（pi `getCurrentSystemPrompt` / `getCurrentTools`）。
pub(crate) fn system_and_tools(messages: &[Message]) -> (String, Vec<Tool>) {
    let mut parts: Vec<String> = Vec::new();
    let mut tools: Vec<Tool> = Vec::new();
    for message in messages {
        let Message::System(system) = message else {
            continue;
        };
        let text = content_text(&system.content);
        if !text.is_empty() {
            parts.push(text);
        }
        for section in system.sections.values().flatten() {
            parts.push(section.clone());
        }
        for removed in &system.tools_removed {
            tools.retain(|tool| tool.name != removed.name);
        }
        for added in &system.tools_added {
            match tools.iter_mut().find(|tool| tool.name == added.name) {
                Some(existing) => *existing = added.clone(),
                None => tools.push(added.clone()),
            }
        }
    }
    (parts.join("\n\n"), tools)
}

/// 内容块里的文本拼接。
pub(crate) fn content_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 截断错误信息（pi 用 500 字符上限）。
pub(crate) fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}...")
}

/// 哪些 HTTP 状态值得重试（交给 retry 层）。
pub(crate) fn retryable_status(status: u16) -> bool {
    status == 408 || status == 429 || (500..600).contains(&status)
}

/// 注入/覆盖一个请求头（大小写不敏感）；`None` 表示删除。
pub(crate) fn set_header(headers: &mut HttpHeaders, name: &str, value: Option<&str>) {
    if let Some(existing) = headers
        .keys()
        .find(|key| key.eq_ignore_ascii_case(name))
        .cloned()
    {
        headers.remove(&existing);
    }
    if let Some(value) = value {
        headers.insert(name.to_string(), value.to_string());
    }
}

/// 只含单条 `error` 事件的流。
pub(crate) fn error_stream(model: &Model, message: String) -> AssistantEventStream {
    let mut error = empty_message(model);
    error.stop_reason = StopReason::Error;
    error.error_message = Some(message);
    AssistantEventStream::new(futures_util::stream::iter(vec![
        AssistantMessageEvent::Error {
            reason: ErrorReason::Error,
            error,
        },
    ]))
}

/// 一条 tool_result 的文本内容（OpenAI 的 `role: tool` 只接受字符串）。
pub(crate) fn tool_result_text(content: &[ContentBlock]) -> String {
    content_text(content)
}

/// 把内容块转成 OpenAI 的 data URI 图片部分。
pub(crate) fn openai_image_part(image: &crate::ImageContent) -> JsonValue {
    json!({
        "type": "image_url",
        "image_url": { "url": format!("data:{};base64,{}", image.mime_type, image.data) },
    })
}
