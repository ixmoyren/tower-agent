//! `edit` 工具（对应 pi `coding-agent` 的 `core/tools/edit.ts`）。

use std::{
    path::Path,
    sync::Arc,
    task::{Context, Poll},
};

use agent::{AgentError, ToolCallRequest, ToolResultStream};
use ai::{JsonObject, JsonValue};
use tokio_util::sync::CancellationToken;
use tower::Service;

use super::{
    edit_diff::{
        Edit, apply_edits_to_normalized_content, detect_line_ending, generate_diff_string,
        generate_unified_patch, normalize_to_lf, restore_line_endings, split_bom,
    },
    path_utils::resolve_to_cwd,
    queue::FileMutationQueue,
    support::{ToolFuture, arg_str, error_result, once, text_result},
};

/// `edit` 工具的选项。
#[derive(Debug, Clone, Default)]
pub struct EditToolOptions {
    /// 覆盖文件修改队列。
    pub queue: Option<FileMutationQueue>,
}

/// 精确文本替换工具。
#[derive(Clone)]
pub struct EditTool {
    cwd: Arc<Path>,
    queue: FileMutationQueue,
}

impl EditTool {
    /// 用工作目录构造。
    pub fn new(cwd: impl AsRef<Path>) -> Self {
        Self::with_options(cwd, EditToolOptions::default())
    }

    /// 用工作目录与选项构造。
    pub fn with_options(cwd: impl AsRef<Path>, options: EditToolOptions) -> Self {
        Self {
            cwd: Arc::from(cwd.as_ref()),
            queue: options.queue.unwrap_or_default(),
        }
    }

    /// 工具名。
    pub const NAME: &'static str = "edit";

    /// 发给模型的工具描述。
    pub const DESCRIPTION: &'static str = "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, \
         non-overlapping region of the original file. If two changes affect the same block or nearby \
         lines, merge them into one edit instead of emitting overlapping edits. Do not include large \
         unchanged regions just to connect distant changes.";

    /// 参数的 JSON Schema。
    pub fn parameters() -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
                "edits": {
                    "type": "array",
                    "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": {
                                "type": "string",
                                "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."
                            },
                            "newText": { "type": "string", "description": "Replacement text for this targeted edit." }
                        },
                        "required": ["oldText", "newText"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }

    async fn execute(
        &self,
        args: &JsonObject,
        signal: &Option<CancellationToken>,
    ) -> agent::AgentToolResult {
        let path = match arg_str(args, "path") {
            Ok(path) => path,
            Err(message) => return error_result(message),
        };
        let edits = match parse_edits(args) {
            Ok(edits) => edits,
            Err(message) => return error_result(message),
        };
        if edits.is_empty() {
            return error_result(
                "Edit tool input is invalid. edits must contain at least one replacement.",
            );
        }

        let absolute = resolve_to_cwd(&path, &self.cwd);
        let signal = signal.clone();
        self.queue
            .run(&absolute, || async {
                if cancelled(&signal) {
                    return error_result("Operation aborted");
                }
                if let Err(error) = tokio::fs::metadata(&absolute).await {
                    return error_result(format!(
                        "Could not edit file: {path}. Error code: {}.",
                        error_code(&error)
                    ));
                }
                let raw = match tokio::fs::read(&absolute).await {
                    Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                    Err(error) => {
                        return error_result(format!("Could not edit file: {path}. {error}"));
                    }
                };
                if cancelled(&signal) {
                    return error_result("Operation aborted");
                }

                let (bom, content) = split_bom(&raw);
                let original_ending = detect_line_ending(content);
                let normalized = normalize_to_lf(content);
                let (base, new_content) =
                    match apply_edits_to_normalized_content(&normalized, &edits, &path) {
                        Ok(result) => result,
                        Err(message) => return error_result(message),
                    };

                let final_content = format!(
                    "{bom}{}",
                    restore_line_endings(&new_content, original_ending)
                );
                if let Err(error) = tokio::fs::write(&absolute, final_content.as_bytes()).await {
                    return error_result(format!("Could not edit file: {path}. {error}"));
                }
                if cancelled(&signal) {
                    return error_result("Operation aborted");
                }

                let (diff, first_changed_line) = generate_diff_string(&base, &new_content);
                let patch = generate_unified_patch(&path, &base, &new_content);
                let mut result = text_result(format!(
                    "Successfully replaced {} block(s) in {path}.",
                    edits.len()
                ));
                result.details = Some(serde_json::json!({
                    "diff": diff,
                    "patch": patch,
                    "firstChangedLine": first_changed_line,
                }));
                result
            })
            .await
    }
}

impl Service<ToolCallRequest> for EditTool {
    type Response = ToolResultStream;
    type Error = AgentError;
    type Future = ToolFuture;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: ToolCallRequest) -> Self::Future {
        let tool = self.clone();
        Box::pin(async move {
            Ok(once(
                tool.execute(&request.tool_call.arguments, &request.signal)
                    .await,
            ))
        })
    }
}

fn cancelled(signal: &Option<CancellationToken>) -> bool {
    signal.as_ref().is_some_and(CancellationToken::is_cancelled)
}

fn error_code(error: &std::io::Error) -> String {
    error
        .raw_os_error()
        .map_or_else(|| error.kind().to_string(), |code| code.to_string())
}

/// 从入参读取 `edits`（pi `validateEditInput`）。
fn parse_edits(args: &JsonObject) -> Result<Vec<Edit>, String> {
    let Some(JsonValue::Array(entries)) = args.get("edits") else {
        return Err(
            "Edit tool input is invalid. edits must contain at least one replacement.".into(),
        );
    };
    let mut edits = Vec::with_capacity(entries.len());
    for entry in entries {
        let JsonValue::Object(entry) = entry else {
            return Err("Each edit must be an object with oldText and newText.".into());
        };
        let old_text = match entry.get("oldText") {
            Some(JsonValue::String(value)) => value.clone(),
            _ => return Err("Each edit must have a string oldText.".into()),
        };
        let new_text = match entry.get("newText") {
            Some(JsonValue::String(value)) => value.clone(),
            _ => return Err("Each edit must have a string newText.".into()),
        };
        edits.push(Edit { old_text, new_text });
    }
    Ok(edits)
}

/// 兼容模型发来的非标准 `edits` 形状（pi `prepareEditArguments`）。
///
/// - `edits` 是 JSON 字符串（数组或单个对象）
/// - `edits` 是单个对象而不是数组
/// - 旧式顶层 `oldText` / `newText`
#[must_use]
pub fn prepare_edit_arguments(mut args: JsonObject) -> JsonObject {
    if let Some(value) = args.get_mut("edits") {
        if let JsonValue::String(raw) = value {
            if let Ok(parsed) = serde_json::from_str::<JsonValue>(raw) {
                *value = match parsed {
                    JsonValue::Array(_) => parsed,
                    JsonValue::Object(_) => JsonValue::Array(vec![parsed]),
                    other => other,
                };
            }
        } else if matches!(value, JsonValue::Object(_)) {
            let taken = value.clone();
            *value = JsonValue::Array(vec![taken]);
        }
    }

    let legacy_old = args.remove("oldText");
    let legacy_new = args.remove("newText");
    if let (Some(JsonValue::String(old_text)), Some(JsonValue::String(new_text))) =
        (legacy_old, legacy_new)
    {
        let mut edits = match args.remove("edits") {
            Some(JsonValue::Array(entries)) => entries,
            _ => Vec::new(),
        };
        edits.push(serde_json::json!({ "oldText": old_text, "newText": new_text }));
        args.insert("edits".into(), JsonValue::Array(edits));
    }
    args
}

/// 构造 `edit` 工具。
pub fn create_edit_tool(cwd: impl AsRef<Path>) -> agent::AgentTool {
    agent::AgentTool::new(EditTool::NAME, EditTool::new(cwd))
        .label(EditTool::NAME)
        .description(EditTool::DESCRIPTION)
        .parameters(EditTool::parameters())
        .prepare_arguments(std::sync::Arc::new(prepare_edit_arguments))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn prepare_arguments_normalizes_single_object_edits() {
        let prepared = prepare_edit_arguments(
            match json!({ "path": "a", "edits": { "oldText": "x", "newText": "y" } }) {
                JsonValue::Object(map) => map,
                _ => unreachable!(),
            },
        );
        assert!(
            matches!(prepared.get("edits"), Some(JsonValue::Array(entries)) if entries.len() == 1)
        );
    }

    #[test]
    fn prepare_arguments_parses_stringified_edits() {
        let prepared = prepare_edit_arguments(
            match json!({ "path": "a", "edits": "[{\"oldText\":\"x\",\"newText\":\"y\"}]" }) {
                JsonValue::Object(map) => map,
                _ => unreachable!(),
            },
        );
        let edits = parse_edits(&prepared).expect("edits parse");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "y");
    }

    #[test]
    fn prepare_arguments_merges_legacy_fields() {
        let prepared = prepare_edit_arguments(
            match json!({ "path": "a", "oldText": "x", "newText": "y" }) {
                JsonValue::Object(map) => map,
                _ => unreachable!(),
            },
        );
        assert!(!prepared.contains_key("oldText"));
        let edits = parse_edits(&prepared).expect("edits parse");
        assert_eq!(edits.len(), 1);
    }
}
