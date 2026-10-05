//! `read` 工具（对应 pi `coding-agent` 的 `core/tools/read.ts`）。
//!
//! 行为对齐说明：文本路径与 pi 完全一致（offset/limit、行与字节双重截断、续读提示）。
//! 图片路径按 pi 的 MIME 嗅探规则识别并作为附件送回；**未实现** pi 的自动缩放，
//! 因此超出 provider 限制的图片不会被压缩（见 design.md 第 9 节第 4.ii 步说明）。

use std::{
    path::Path,
    sync::Arc,
    task::{Context, Poll},
};

use agent::{AgentError, AgentToolResult, ToolCallRequest, ToolResultStream};
use ai::{Arguments, ContentBlock, ImageContent};
use base64::Engine as _;
use tokio_util::sync::CancellationToken;
use tower::Service;

use super::{
    path_utils::resolve_read_path,
    support::{ToolFuture, error_result, once, text_content, text_result},
    truncate::{DEFAULT_MAX_BYTES, format_size, truncate_head},
};

/// 图片嗅探读取的字节数（pi `IMAGE_TYPE_SNIFF_BYTES`）。
const IMAGE_TYPE_SNIFF_BYTES: usize = 4100;

/// `read` 工具。
#[derive(Clone)]
pub struct ReadTool {
    cwd: Arc<Path>,
}

impl ReadTool {
    /// 用工作目录构造。
    pub fn new(cwd: impl AsRef<Path>) -> Self {
        Self {
            cwd: Arc::from(cwd.as_ref()),
        }
    }

    /// 工具名。
    pub const NAME: &'static str = "read";

    /// 发给模型的工具描述（与 pi 一字不差）。
    pub fn description() -> String {
        format!(
            "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). \
             Images are sent as attachments. For text files, output is truncated to {} lines or {}KB \
             (whichever is hit first). Use offset/limit for large files. When you need the full file, \
             continue with offset until complete.",
            super::truncate::DEFAULT_MAX_LINES,
            DEFAULT_MAX_BYTES / 1024
        )
    }

    /// 参数的 JSON Schema。
    pub fn parameters() -> ai::JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
                "offset": { "type": "number", "description": "Line number to start reading from (1-indexed)" },
                "limit": { "type": "number", "description": "Maximum number of lines to read" }
            },
            "required": ["path"]
        })
    }

    async fn execute(
        &self,
        args: &Arguments,
        signal: &Option<CancellationToken>,
    ) -> AgentToolResult {
        let path = match args.arg_str("path") {
            Ok(path) => path,
            Err(message) => return error_result(message),
        };
        let offset = match args.opt_u64("offset") {
            Ok(offset) => offset,
            Err(message) => return error_result(message),
        };
        let limit = match args.opt_u64("limit") {
            Ok(limit) => limit,
            Err(message) => return error_result(message),
        };

        let absolute = resolve_read_path(&path, &self.cwd);
        if cancelled(signal) {
            return error_result("Operation aborted");
        }
        if tokio::fs::metadata(&absolute).await.is_err() {
            return error_result(format!("Could not read file: {path}."));
        }

        let sniff = match read_sniff(&absolute).await {
            Ok(bytes) => bytes,
            Err(error) => return error_result(format!("Could not read file: {path}. {error}")),
        };
        if let Some(mime_type) = detect_supported_image_mime_type(&sniff) {
            return self.read_image(&absolute, &path, mime_type, signal).await;
        }

        let bytes = match tokio::fs::read(&absolute).await {
            Ok(bytes) => bytes,
            Err(error) => return error_result(format!("Could not read file: {path}. {error}")),
        };
        if cancelled(signal) {
            return error_result("Operation aborted");
        }
        let content = String::from_utf8_lossy(&bytes).into_owned();
        let all_lines: Vec<&str> = content.split('\n').collect();
        let total_file_lines = all_lines.len();

        let start_line = offset.map_or(0, |offset| offset.saturating_sub(1) as usize);
        let start_line_display = start_line + 1;
        if start_line >= all_lines.len() {
            return error_result(format!(
                "Offset {} is beyond end of file ({total_file_lines} lines total)",
                offset.unwrap_or(0)
            ));
        }

        let mut user_limited_lines = None;
        let selected = match limit {
            Some(limit) => {
                let end_line = (start_line + limit as usize).min(all_lines.len());
                user_limited_lines = Some(end_line - start_line);
                all_lines[start_line..end_line].join("\n")
            }
            None => all_lines[start_line..].join("\n"),
        };

        let truncation = truncate_head(&selected, super::truncate::TruncationOptions::default());
        let output_text = if truncation.first_line_exceeds_limit {
            let first_line_size = format_size(all_lines[start_line].len());
            format!(
                "[Line {start_line_display} is {first_line_size}, exceeds {} limit. Use shell: \
                 sed -n '{start_line_display}p' {path} | head -c {DEFAULT_MAX_BYTES}]",
                format_size(DEFAULT_MAX_BYTES)
            )
        } else if truncation.truncated {
            let end_line_display = start_line_display + truncation.output_lines - 1;
            let next_offset = end_line_display + 1;
            let mut text = truncation.content.clone();
            if truncation.truncated_by == Some(super::truncate::TruncatedBy::Lines) {
                text.push_str(&format!(
                    "\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines}. \
                     Use offset={next_offset} to continue.]"
                ));
            } else {
                text.push_str(&format!(
                    "\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines} \
                     ({} limit). Use offset={next_offset} to continue.]",
                    format_size(DEFAULT_MAX_BYTES)
                ));
            }
            text
        } else if let Some(user_limited_lines) = user_limited_lines
            && start_line + user_limited_lines < all_lines.len()
        {
            let remaining = all_lines.len() - (start_line + user_limited_lines);
            let next_offset = start_line + user_limited_lines + 1;
            format!(
                "{}\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]",
                truncation.content
            )
        } else {
            truncation.content.clone()
        };

        let mut result = text_result(output_text);
        if truncation.truncated {
            result.details = Some(
                serde_json::json!({ "truncation": super::support::truncation_json(&truncation) }),
            );
        }
        result
    }

    async fn read_image(
        &self,
        absolute: &std::path::Path,
        path: &str,
        mime_type: &str,
        signal: &Option<CancellationToken>,
    ) -> AgentToolResult {
        let bytes = match tokio::fs::read(absolute).await {
            Ok(bytes) => bytes,
            Err(error) => return error_result(format!("Could not read file: {path}. {error}")),
        };
        if cancelled(signal) {
            return error_result("Operation aborted");
        }
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        AgentToolResult {
            content: vec![
                text_content(format!("Read image file [{mime_type}]")),
                ContentBlock::Image(ImageContent {
                    data,
                    mime_type: mime_type.to_string(),
                }),
            ],
            details: None,
            structured_content: None,
            usage: None,
            is_error: false,
            terminate: None,
        }
    }
}

impl Service<ToolCallRequest> for ReadTool {
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

async fn read_sniff(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;
    let mut file = tokio::fs::File::open(path).await?;
    let mut buffer = vec![0u8; IMAGE_TYPE_SNIFF_BYTES];
    let read = file.read(&mut buffer).await?;
    buffer.truncate(read);
    Ok(buffer)
}

/// pi `detectSupportedImageMimeType`。
fn detect_supported_image_mime_type(buffer: &[u8]) -> Option<&'static str> {
    if buffer.starts_with(&[0xff, 0xd8, 0xff]) {
        return if buffer.get(3) == Some(&0xf7) {
            None
        } else {
            Some("image/jpeg")
        };
    }
    if buffer.starts_with(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]) {
        return (is_png(buffer) && !is_animated_png(buffer)).then_some("image/png");
    }
    if buffer.starts_with(b"GIF87a") || buffer.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if buffer.starts_with(b"RIFF") && buffer.get(8..12) == Some(b"WEBP") {
        return Some("image/webp");
    }
    if buffer.starts_with(b"BM") && is_bmp(buffer) {
        return Some("image/bmp");
    }
    None
}

fn read_u32_be(buffer: &[u8], offset: usize) -> Option<u32> {
    let slice = buffer.get(offset..offset + 4)?;
    Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_u32_le(buffer: &[u8], offset: usize) -> Option<u32> {
    let slice = buffer.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn is_png(buffer: &[u8]) -> bool {
    buffer.len() >= 16 && read_u32_be(buffer, 8) == Some(13) && buffer.get(12..16) == Some(b"IHDR")
}

fn is_animated_png(buffer: &[u8]) -> bool {
    let mut offset = 8usize;
    while offset + 8 <= buffer.len() {
        let Some(length) = read_u32_be(buffer, offset) else {
            return false;
        };
        let chunk_type = buffer.get(offset + 4..offset + 8);
        if chunk_type == Some(b"acTL") {
            return true;
        }
        if chunk_type == Some(b"IDAT") {
            return false;
        }
        let next = offset + 8 + length as usize + 4;
        if next <= offset || next > buffer.len() {
            return false;
        }
        offset = next;
    }
    false
}

fn is_bmp(buffer: &[u8]) -> bool {
    buffer.len() >= 26 && read_u32_le(buffer, 2).is_some_and(|size| size as usize >= 26)
}

/// 构造 `read` 工具。
pub fn create_read_tool(cwd: impl AsRef<Path>) -> agent::AgentTool {
    agent::AgentTool::new(ReadTool::NAME, ReadTool::new(cwd))
        .label(ReadTool::NAME)
        .description(ReadTool::description())
        .parameters(ReadTool::parameters())
}
