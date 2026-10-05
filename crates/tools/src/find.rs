//! `find` 工具（对应 pi `coding-agent` 的 `core/tools/find.ts`）。
//!
//! pi 的默认实现调用外部 `fd`；这里用 `ignore` + `globset` 在进程内实现同一语义：
//! 同样的 glob 规则（无 `/` 时按文件名匹配，含 `/` 时按整条相对路径匹配，且 `*` 不跨
//! 目录分隔符）、同样尊重 `.gitignore`（不在 git 仓库内时也生效）、同样包含隐藏文件。

use std::{
    path::Path,
    sync::Arc,
    task::{Context, Poll},
};

use agent::{AgentError, AgentToolResult, ToolCallRequest, ToolResultStream};
use ai::Arguments;
use globset::{GlobBuilder, GlobMatcher};
use ignore::WalkBuilder;
use tokio_util::sync::CancellationToken;
use tower::{Layer, Service};

use super::{
    layer::ToolNotFound,
    path_utils::resolve_to_cwd,
    support::{ToolFuture, error_result, once, text_result},
    truncate::{DEFAULT_MAX_BYTES, TruncationOptions, format_size, truncate_head},
};

/// 默认最大结果数（pi `DEFAULT_LIMIT`）。
const DEFAULT_LIMIT: u64 = 1000;

/// 按 glob 查找文件的工具。
#[derive(Clone)]
pub struct FindTool {
    cwd: Arc<Path>,
}

impl FindTool {
    /// 用工作目录构造。
    pub fn new(cwd: impl AsRef<Path>) -> Self {
        Self {
            cwd: Arc::from(cwd.as_ref()),
        }
    }

    /// 工具名。
    pub const NAME: &'static str = "find";

    /// 发给模型的工具描述（与 pi 一致）。
    pub fn description() -> String {
        format!(
            "Search for files by glob pattern. Returns matching file paths relative to the search \
             directory. Respects .gitignore. Output is truncated to {DEFAULT_LIMIT} results or {}KB \
             (whichever is hit first).",
            DEFAULT_MAX_BYTES / 1024
        )
    }

    /// 参数的 JSON Schema。
    pub fn parameters() -> ai::JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'"
                },
                "path": { "type": "string", "description": "Directory to search in (default: current directory)" },
                "limit": { "type": "number", "description": "Maximum number of results (default: 1000)" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(
        &self,
        args: &Arguments,
        signal: &Option<CancellationToken>,
    ) -> AgentToolResult {
        let pattern = match args.arg_str("pattern") {
            Ok(pattern) => pattern,
            Err(message) => return error_result(message),
        };
        let search_dir = match args.opt_str("path") {
            Ok(dir) => dir.unwrap_or_else(|| ".".into()),
            Err(message) => return error_result(message),
        };
        let limit = match args.opt_u64("limit") {
            Ok(limit) => limit.unwrap_or(DEFAULT_LIMIT),
            Err(message) => return error_result(message),
        };

        if cancelled(signal) {
            return error_result("Operation aborted");
        }

        let search_path = resolve_to_cwd(&search_dir, &self.cwd);
        if !search_path.exists() {
            return error_result(format!("Path not found: {}", search_path.display()));
        }

        let matcher = match build_matcher(&pattern) {
            Ok(matcher) => matcher,
            Err(message) => return error_result(message),
        };
        let match_full_path = pattern.contains('/');

        // pi 在非 git 仓库里加 `--no-require-git`，让 .gitignore 也生效。
        let inside_git_repo = inside_git_repo(&search_path);

        let mut results: Vec<String> = Vec::new();
        let walker = WalkBuilder::new(&search_path)
            .hidden(false)
            .require_git(!inside_git_repo)
            .build();

        for entry in walker {
            if cancelled(signal) {
                return error_result("Operation aborted");
            }
            // 与 `fd --max-results` 相同：收满 limit 条就停。
            if results.len() as u64 >= limit {
                break;
            }
            let Ok(entry) = entry else {
                continue;
            };
            let path = entry.path();
            if path == search_path {
                continue;
            }
            let relative = relative_posix(path, &search_path);
            let candidate = if match_full_path {
                relative.as_str()
            } else {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
            };
            if !matcher.is_match(candidate) {
                continue;
            }
            results.push(relative);
        }

        // pi 用 `results.length >= limit` 判断是否触顶（fd 会在 limit 处截断）。
        let result_limit_reached = !results.is_empty() && results.len() as u64 >= limit;

        if results.is_empty() {
            return text_result("No files found matching pattern");
        }

        let raw_output = results.join("\n");
        let truncation = truncate_head(
            &raw_output,
            TruncationOptions {
                max_lines: Some(usize::MAX),
                max_bytes: None,
            },
        );
        let mut output = truncation.content.clone();

        let mut notices: Vec<String> = Vec::new();
        if result_limit_reached {
            notices.push(format!(
                "{limit} results limit reached. Use limit={} for more, or refine pattern",
                limit * 2
            ));
        }
        if truncation.truncated {
            notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        }
        if !notices.is_empty() {
            output.push_str(&format!("\n\n[{}]", notices.join(". ")));
        }

        let mut result = text_result(output);
        if result_limit_reached || truncation.truncated {
            result.details = Some(serde_json::json!({
                "resultLimitReached": result_limit_reached.then_some(limit),
                "truncation": truncation.truncated.then(|| super::support::truncation_json(&truncation)),
            }));
        }
        result
    }
}

impl Service<ToolCallRequest> for FindTool {
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

/// 构造 glob 匹配器：含 `/` 时按整条路径匹配，并在需要时补 `**/` 前缀。
fn build_matcher(pattern: &str) -> Result<GlobMatcher, String> {
    let effective = if pattern.contains('/') {
        if pattern.starts_with('/') || pattern.starts_with("**/") || pattern == "**" {
            pattern.to_string()
        } else {
            format!("**/{pattern}")
        }
    } else {
        pattern.to_string()
    };
    GlobBuilder::new(&effective)
        // fd/glob 语义：`*` 不跨目录分隔符。
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|error| format!("Invalid glob pattern: {error}"))
}

/// 相对路径并统一成 posix 分隔符（pi `relativizeFindResultPath`）。
fn relative_posix(path: &Path, base: &Path) -> String {
    let relative = path.strip_prefix(base).unwrap_or(path);
    relative.to_string_lossy().replace('\\', "/")
}

/// 从 `search_path` 向上查找 `.git`（pi 判断是否在 git 仓库内的逻辑）。
fn inside_git_repo(search_path: &Path) -> bool {
    let mut current = if search_path.is_dir() {
        search_path.to_path_buf()
    } else {
        search_path
            .parent()
            .map_or_else(|| search_path.to_path_buf(), Path::to_path_buf)
    };
    loop {
        if current.join(".git").exists() {
            return true;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => return false,
        }
    }
}

/// 构造 `find` 工具。
pub fn create_find_tool(cwd: impl AsRef<Path>) -> agent::AgentTool {
    let tool = FindTool::new(cwd);
    agent::AgentTool::new(FindTool::NAME, tool.layer(ToolNotFound))
        .label(FindTool::NAME)
        .description(FindTool::description())
        .parameters(FindTool::parameters())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_patterns_are_matched_against_the_file_name_by_the_caller() {
        let matcher = build_matcher("*.ts").expect("valid glob");
        assert!(matcher.is_match("main.ts"));
        assert!(!matcher.is_match("main.rs"));
        // `*` 不跨目录分隔符：整条路径只有在 caller 传文件名时才会命中。
        assert!(!matcher.is_match("deep/nested/main.ts"));
    }

    #[test]
    fn path_patterns_get_a_leading_double_star() {
        let matcher = build_matcher("src/**/*.spec.ts").expect("valid glob");
        assert!(matcher.is_match("src/a/b.spec.ts"));
        assert!(matcher.is_match("src/b.spec.ts"));
        // `**/` 可以匹配任意前缀（与 fd 的整路径匹配一致）。
        assert!(matcher.is_match("other/src/a.spec.ts"));
        assert!(!matcher.is_match("other/a.spec.ts"));
    }
}
