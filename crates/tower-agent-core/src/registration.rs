//! 类型化工具注册：用 schemars 生成参数 JSON Schema、用 serde 反序列化入参。
//!
//! 手写 [`AgentTool`]（`serde_json::json!` schema + 手工读参）在工具多起来后既啰嗦
//! 又容易和文档漂移。这里提供一个 typed 入口：`P` 同时 derive `Deserialize` 与
//! `JsonSchema`，注册时自动得到参数 schema，调用时自动反序列化；反序列化失败会变成
//! `is_error` 的工具结果（与 pi 的入参校验失败语义一致），而不是走错误通道。

use std::{
    future::Future,
    marker::PhantomData,
    task::{Context, Poll},
};

use futures_util::future::BoxFuture;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use tower::Service;
use tower_agent_provider::JsonValue;

use crate::{AgentError, AgentTool, AgentToolResult, ToolCallRequest, ToolResultStream};

/// 类型化工具的注册入口。
///
/// [`ToolRegistration::new`] 把 `P` 的 schemars schema 装进 [`AgentTool`]，并把每次调用
/// 的 JSON 参数反序列化成 `P` 后交给 handler。
pub struct ToolRegistration;

impl ToolRegistration {
    /// 用参数类型 `P`、工具名、描述与 handler 构造一个 [`AgentTool`]。
    ///
    /// handler 返回 `Result<AgentToolResult, AgentError>`：业务失败既可以编码进
    /// `AgentToolResult::error`，也可以用 `Err` 表示框架级失败。
    // 有意让 `new` 返回 `AgentTool`（与设计文档的 `ToolRegistration::new::<P, _>` 一致）。
    #[allow(clippy::new_ret_no_self)]
    pub fn new<P, F, Fut>(
        name: impl Into<String>,
        description: impl Into<String>,
        handler: F,
    ) -> AgentTool
    where
        P: DeserializeOwned + JsonSchema + Send + 'static,
        F: Fn(P, ToolCallRequest) -> Fut + Clone + Send + Sync + 'static,
        Fut: Future<Output = Result<AgentToolResult, AgentError>> + Send + 'static,
    {
        let name = name.into();
        let parameters = parameters_schema::<P>();
        AgentTool::new(
            name.clone(),
            TypedTool {
                handler,
                _params: PhantomData,
            },
        )
        .label(name)
        .description(description)
        .parameters(parameters)
    }
}

/// `P` 的 JSON Schema；序列化失败时退回一个宽松的空对象 schema。
fn parameters_schema<P: JsonSchema>() -> JsonValue {
    serde_json::to_value(schemars::schema_for!(P))
        .unwrap_or_else(|_| serde_json::json!({ "type": "object" }))
}

/// 由 [`ToolRegistration`] 构造的工具服务。
struct TypedTool<P, F> {
    handler: F,
    _params: PhantomData<fn() -> P>,
}

impl<P, F: Clone> Clone for TypedTool<P, F> {
    fn clone(&self) -> Self {
        Self {
            handler: self.handler.clone(),
            _params: PhantomData,
        }
    }
}

impl<P, F, Fut> Service<ToolCallRequest> for TypedTool<P, F>
where
    P: DeserializeOwned + Send + 'static,
    F: Fn(P, ToolCallRequest) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Result<AgentToolResult, AgentError>> + Send + 'static,
{
    type Response = ToolResultStream;
    type Error = AgentError;
    type Future = BoxFuture<'static, Result<ToolResultStream, AgentError>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: ToolCallRequest) -> Self::Future {
        let handler = self.handler.clone();
        Box::pin(async move {
            let value = JsonValue::Object(request.tool_call.arguments.as_object().clone());
            match serde_json::from_value::<P>(value) {
                Ok(params) => Ok(ToolResultStream::once(handler(params, request).await?)),
                Err(error) => Ok(ToolResultStream::once(AgentToolResult::error(format!(
                    "Invalid arguments: {error}"
                )))),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt as _;
    use tower_agent_provider::ToolCall;

    #[derive(serde::Deserialize, JsonSchema)]
    struct Greet {
        name: String,
        #[serde(default)]
        excited: bool,
    }

    fn greet_tool() -> AgentTool {
        ToolRegistration::new::<Greet, _, _>(
            "greet",
            "Greet someone",
            |params: Greet, _request| async move {
                let suffix = if params.excited { "!" } else { "." };
                Ok(AgentToolResult::text(format!(
                    "hello {}{suffix}",
                    params.name
                )))
            },
        )
    }

    fn request(arguments: serde_json::Value) -> ToolCallRequest {
        ToolCallRequest::new(ToolCall {
            id: "call-1".into(),
            name: "greet".into(),
            arguments: arguments.as_object().cloned().unwrap_or_default().into(),
            thought_signature: None,
            namespace: None,
        })
    }

    fn text_of(result: &AgentToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| match block {
                tower_agent_provider::ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn typed_tool_derives_schema_and_metadata() {
        let tool = greet_tool();
        assert_eq!(tool.name(), "greet");
        assert_eq!(tool.tool_description(), "Greet someone");
        let schema = tool.tool_parameters();
        assert_eq!(
            schema.get("type").and_then(JsonValue::as_str),
            Some("object")
        );
        assert!(
            schema
                .get("properties")
                .and_then(JsonValue::as_object)
                .is_some()
        );
    }

    #[tokio::test]
    async fn typed_tool_deserializes_arguments_and_runs() {
        let tool = greet_tool();
        let mut stream = tool
            .call(request(
                serde_json::json!({ "name": "world", "excited": true }),
            ))
            .await
            .expect("call succeeds");
        let result = stream.next().await.expect("one result");
        assert!(!result.is_error);
        assert_eq!(text_of(&result), "hello world!");
    }

    #[tokio::test]
    async fn typed_tool_reports_invalid_arguments_as_a_tool_result() {
        let tool = greet_tool();
        let mut stream = tool
            .call(request(serde_json::json!({ "nope": 1 })))
            .await
            .expect("call succeeds");
        let result = stream.next().await.expect("one result");
        assert!(result.is_error);
        assert!(text_of(&result).contains("Invalid arguments"));
    }
}
