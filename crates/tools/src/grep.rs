//! `grep` 工具（对应 pi `coding-agent` 的 `core/tools/grep.ts`）。
//!
//! pi 的默认实现调用 `rg --json`；这里用 `regex` + `ignore` 在进程内实现同一输出形状：
//! `path:line: text`（带上下文时非命中行用 `path-line- text`）、长行截断到 500 字符、
//! 命中数 / 字节数双重上限，以及同样的提示文案。文件遍历沿用 `ignore`，尊重 `.gitignore`
//! 且包含隐藏文件，并跳过二进制文件（对应 rg 的默认行为）。

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    task::{Context, Poll},
};

use agent::{AgentError, AgentToolResult, ToolCallRequest, ToolResultStream};
use ai::Arguments;
use globset::{GlobBuilder, GlobMatcher};
use ignore::WalkBuilder;
use regex::RegexBuilder;
use tokio_util::sync::CancellationToken;
use tower::{Layer, Service};

use super::{
    layer::ToolNotFound,
    path_utils::resolve_to_cwd,
    support::{ToolFuture, error_result, once, text_result},
    truncate::{
        DEFAULT_MAX_BYTES, GREP_MAX_LINE_LENGTH, TruncationOptions, format_size, truncate_head,
        truncate_line,
    },
};

/// 默认最大命中数（pi `DEFAULT_LIMIT`）。
const DEFAULT_LIMIT: u64 = 100;

/// 在文件内容里搜索的工具。
#[derive(Clone)]
pub struct GrepTool {
    cwd: Arc<Path>,
}

impl GrepTool {
    /// 用工作目录构造。
    pub fn new(cwd: impl AsRef<Path>) -> Self {
        Self {
            cwd: Arc::from(cwd.as_ref()),
        }
    }

    /// 工具名。
    pub const NAME: &'static str = "grep";

    /// 发给模型的工具描述（与 pi 一致）。
    pub fn description() -> String {
        format!(
            "Search file contents for a pattern. Returns matching lines with file paths and line \
             numbers. Respects .gitignore. Output is truncated to {DEFAULT_LIMIT} matches or {}KB \
             (whichever is hit first). Long lines are truncated to {GREP_MAX_LINE_LENGTH} chars.",
            DEFAULT_MAX_BYTES / 1024
        )
    }

    /// 参数的 JSON Schema。
    pub fn parameters() -> ai::JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Search pattern (regex or literal string)" },
                "path": { "type": "string", "description": "Directory or file to search (default: current directory)" },
                "glob": { "type": "string", "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'" },
                "ignoreCase": { "type": "boolean", "description": "Case-insensitive search (default: false)" },
                "literal": { "type": "boolean", "description": "Treat pattern as literal string instead of regex (default: false)" },
                "context": { "type": "number", "description": "Number of lines to show before and after each match (default: 0)" },
                "limit": { "type": "number", "description": "Maximum number of matches to return (default: 100)" }
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
        let glob = match args.opt_str("glob") {
            Ok(glob) => glob,
            Err(message) => return error_result(message),
        };
        let ignore_case = matches!(args.get("ignoreCase"), Some(ai::JsonValue::Bool(true)));
        let literal = matches!(args.get("literal"), Some(ai::JsonValue::Bool(true)));
        let context = match args.opt_u64("context") {
            Ok(context) => context.unwrap_or(0),
            Err(message) => return error_result(message),
        };
        let limit = match args.opt_u64("limit") {
            Ok(limit) => limit.unwrap_or(DEFAULT_LIMIT),
            Err(message) => return error_result(message),
        }
        // pi: `Math.max(1, limit ?? DEFAULT_LIMIT)`
        .max(1);

        if cancelled(signal) {
            return error_result("Operation aborted");
        }

        let source = if literal {
            regex::escape(&pattern)
        } else {
            pattern
        };
        let regex = match RegexBuilder::new(&source)
            .case_insensitive(ignore_case)
            .build()
        {
            Ok(regex) => regex,
            Err(error) => return error_result(format!("Invalid regex pattern: {error}")),
        };

        let search_path = resolve_to_cwd(&search_dir, &self.cwd);
        let metadata = match tokio::fs::metadata(&search_path).await {
            Ok(metadata) => metadata,
            Err(_) => return error_result(format!("Path not found: {}", search_path.display())),
        };
        let is_directory = metadata.is_dir();
        let glob_filter = match glob.as_deref().map(build_glob_filter).transpose() {
            Ok(filter) => filter,
            Err(message) => return error_result(message),
        };

        let files = match collect_files(&search_path, is_directory, glob_filter.as_ref()) {
            Ok(files) => files,
            Err(message) => return error_result(message),
        };

        let mut matches: Vec<Match> = Vec::new();
        'files: for file in files {
            if cancelled(signal) {
                return error_result("Operation aborted");
            }
            let Ok(bytes) = tokio::fs::read(&file).await else {
                continue;
            };
            // 与 rg 的默认行为一致：跳过二进制文件。
            if bytes.contains(&0) {
                continue;
            }
            let content = String::from_utf8_lossy(&bytes)
                .replace("\r\n", "\n")
                .replace('\r', "\n");
            let lines: Vec<&str> = content.split('\n').collect();
            for (index, line) in lines.iter().enumerate() {
                if !regex.is_match(line) {
                    continue;
                }
                matches.push(Match {
                    file: file.clone(),
                    line_number: index + 1,
                    line_text: Some((*line).to_string()),
                });
                // 与 `rg` 在命中数达到 limit 时被杀掉一致。
                if matches.len() as u64 >= limit {
                    break 'files;
                }
            }
        }
        let match_limit_reached = !matches.is_empty() && matches.len() as u64 >= limit;

        if matches.is_empty() {
            return text_result("No matches found");
        }

        let mut lines_truncated = false;
        let mut output_lines: Vec<String> = Vec::new();
        for matched in &matches {
            let display = display_path(&matched.file, &search_path, is_directory);
            if context == 0 {
                let (text, truncated) = truncate_line(
                    matched.line_text.as_deref().unwrap_or(""),
                    GREP_MAX_LINE_LENGTH,
                );
                lines_truncated |= truncated;
                output_lines.push(format!("{display}:{}: {text}", matched.line_number));
            } else {
                let block = format_block(matched, &display, context, &mut lines_truncated);
                output_lines.extend(block);
            }
        }

        let raw_output = output_lines.join("\n");
        // 命中数已经单独设限，这里只做字节截断。
        let truncation = truncate_head(
            &raw_output,
            TruncationOptions {
                max_lines: Some(usize::MAX),
                max_bytes: None,
            },
        );
        let mut output = truncation.content.clone();

        let mut notices: Vec<String> = Vec::new();
        if match_limit_reached {
            notices.push(format!(
                "{limit} matches limit reached. Use limit={} for more, or refine pattern",
                limit * 2
            ));
        }
        if truncation.truncated {
            notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        }
        if lines_truncated {
            notices.push(format!(
                "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
            ));
        }
        if !notices.is_empty() {
            output.push_str(&format!("\n\n[{}]", notices.join(". ")));
        }

        let mut result = text_result(output);
        if match_limit_reached || truncation.truncated || lines_truncated {
            result.details = Some(serde_json::json!({
                "matchLimitReached": match_limit_reached.then_some(limit),
                "truncation": truncation.truncated.then(|| super::support::truncation_json(&truncation)),
                "linesTruncated": lines_truncated.then_some(true),
            }));
        }
        result
    }
}

impl Service<ToolCallRequest> for GrepTool {
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

/// 一条命中（已附带命中行文本，避免格式化时重复读文件）。
struct Match {
    file: PathBuf,
    line_number: usize,
    /// 命中行原文；上下文模式需要重新读整文件，所以只在无上下文时缓存。
    line_text: Option<String>,
}

fn cancelled(signal: &Option<CancellationToken>) -> bool {
    signal.as_ref().is_some_and(CancellationToken::is_cancelled)
}

/// rg/`--glob` 的语义：模式不含 `/` 时只匹配文件名，含 `/` 时匹配相对路径。
struct GlobFilter {
    matcher: GlobMatcher,
    match_full_path: bool,
}

impl GlobFilter {
    fn is_match(&self, relative: &str, file_name: &str) -> bool {
        let candidate = if self.match_full_path {
            relative
        } else {
            file_name
        };
        self.matcher.is_match(candidate)
    }
}

fn build_glob_filter(pattern: &str) -> Result<GlobFilter, String> {
    let matcher = GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|error| format!("Invalid glob pattern: {error}"))?;
    Ok(GlobFilter {
        matcher,
        match_full_path: pattern.contains('/'),
    })
}

/// 收集要搜索的文件：目录则递归遍历（尊重 .gitignore、含隐藏文件），否则就是该文件本身。
fn collect_files(
    search_path: &Path,
    is_directory: bool,
    glob_filter: Option<&GlobFilter>,
) -> Result<Vec<PathBuf>, String> {
    if !is_directory {
        return Ok(vec![search_path.to_path_buf()]);
    }
    let mut files = Vec::new();
    for entry in WalkBuilder::new(search_path).hidden(false).build() {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        if let Some(filter) = glob_filter {
            let relative = path.strip_prefix(search_path).unwrap_or(path);
            let candidate = relative.to_string_lossy().replace('\\', "/");
            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            if !filter.is_match(&candidate, file_name) {
                continue;
            }
        }
        files.push(path.to_path_buf());
    }
    Ok(files)
}

/// 输出里的文件路径：目录搜索时用相对路径，否则只用文件名（pi `formatPath`）。
fn display_path(file: &Path, search_path: &Path, is_directory: bool) -> String {
    if is_directory && let Ok(relative) = file.strip_prefix(search_path) {
        let relative = relative.to_string_lossy().replace('\\', "/");
        if !relative.is_empty() && !relative.starts_with("..") {
            return relative;
        }
    }
    file.file_name().map_or_else(
        || file.to_string_lossy().into_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// 生成带上下文的输出块（pi `formatBlock`）。
fn format_block(
    matched: &Match,
    display: &str,
    context: u64,
    lines_truncated: &mut bool,
) -> Vec<String> {
    let Ok(bytes) = std::fs::read(&matched.file) else {
        return vec![format!(
            "{display}:{}: (unable to read file)",
            matched.line_number
        )];
    };
    let content = String::from_utf8_lossy(&bytes)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let lines: Vec<&str> = content.split('\n').collect();
    if lines.is_empty() {
        return vec![format!(
            "{display}:{}: (unable to read file)",
            matched.line_number
        )];
    }

    let context = context as usize;
    let start = matched.line_number.saturating_sub(context).max(1);
    let end = (matched.line_number + context).min(lines.len());
    let mut block = Vec::new();
    for current in start..=end {
        let line = lines.get(current - 1).copied().unwrap_or("");
        let (text, truncated) = truncate_line(line, GREP_MAX_LINE_LENGTH);
        *lines_truncated |= truncated;
        if current == matched.line_number {
            block.push(format!("{display}:{current}: {text}"));
        } else {
            block.push(format!("{display}-{current}- {text}"));
        }
    }
    block
}

/// 构造 `grep` 工具。
pub fn create_grep_tool(cwd: impl AsRef<Path>) -> agent::AgentTool {
    let tool = GrepTool::new(cwd);
    agent::AgentTool::new(GrepTool::NAME, tool.layer(ToolNotFound))
        .label(GrepTool::NAME)
        .description(GrepTool::description())
        .parameters(GrepTool::parameters())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_patterns_are_escaped() {
        let source = regex::escape("a.b");
        let regex = regex::Regex::new(&source).expect("escaped pattern compiles");
        assert!(regex.is_match("a.b"));
        assert!(!regex.is_match("axb"));
    }

    #[test]
    fn glob_filter_matches_basenames() {
        let filter = build_glob_filter("*.ts").expect("valid glob");
        assert!(filter.is_match("src/main.ts", "main.ts"));
        assert!(!filter.is_match("src/main.rs", "main.rs"));

        let filter = build_glob_filter("src/**/*.spec.ts").expect("valid glob");
        assert!(filter.is_match("src/a/b.spec.ts", "b.spec.ts"));
        assert!(!filter.is_match("other/a.spec.ts", "a.spec.ts"));
    }

    #[test]
    fn display_path_is_relative_inside_a_directory_search() {
        let file = Path::new("/tmp/root/src/main.rs");
        assert_eq!(
            display_path(file, Path::new("/tmp/root"), true),
            "src/main.rs"
        );
        assert_eq!(display_path(file, Path::new("/tmp/root"), false), "main.rs");
    }
}
