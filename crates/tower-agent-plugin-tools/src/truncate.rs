//! 工具输出的截断规则（对应 pi `coding-agent` 的 `core/tools/truncate.ts`）。
//!
//! 截断由两个互相独立的限制决定，先命中者生效：行数（默认 2000）与字节数（默认 50KB）。
//! 头截断（`read`）永不产出半行；尾截断（`shell`）在唯一一行就超过字节上限时保留该行尾部。

/// 默认最大行数（pi `DEFAULT_MAX_LINES`）。
pub const DEFAULT_MAX_LINES: usize = 2000;
/// 默认最大字节数（pi `DEFAULT_MAX_BYTES`，50KB）。
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// `grep` 单行最大字符数（pi `GREP_MAX_LINE_LENGTH`）。
pub const GREP_MAX_LINE_LENGTH: usize = 500;

/// 哪一侧的限制被命中。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncatedBy {
    /// 行数限制。
    Lines,
    /// 字节限制。
    Bytes,
}

/// 截断结果（pi `TruncationResult`）。
#[derive(Debug, Clone, PartialEq)]
pub struct TruncationResult {
    /// 截断后的内容。
    pub content: String,
    /// 是否发生了截断。
    pub truncated: bool,
    /// 命中的限制；未截断时为 `None`。
    pub truncated_by: Option<TruncatedBy>,
    /// 原始内容总行数。
    pub total_lines: usize,
    /// 原始内容总字节数。
    pub total_bytes: usize,
    /// 截断输出中的完整行数。
    pub output_lines: usize,
    /// 截断输出的字节数。
    pub output_bytes: usize,
    /// 最后一行是否被部分截断（仅尾截断的边界情况）。
    pub last_line_partial: bool,
    /// 第一行是否单独超过字节上限（仅头截断）。
    pub first_line_exceeds_limit: bool,
    /// 生效的最大行数。
    pub max_lines: usize,
    /// 生效的最大字节数。
    pub max_bytes: usize,
}

/// 截断选项（pi `TruncationOptions`）。
#[derive(Debug, Clone, Copy, Default)]
pub struct TruncationOptions {
    /// 最大行数，默认 [`DEFAULT_MAX_LINES`]。
    pub max_lines: Option<usize>,
    /// 最大字节数，默认 [`DEFAULT_MAX_BYTES`]。
    pub max_bytes: Option<usize>,
}

/// 把字节数格式化成人类可读大小（pi `formatSize`）。
pub fn format_size(bytes: usize) -> String {
    #[allow(clippy::cast_precision_loss)]
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// 统计行数：空内容为 0 行；以换行结尾时不计最后那个空片段（pi `splitLinesForCounting`）。
fn count_lines(content: &str) -> usize {
    if content.is_empty() {
        return 0;
    }
    let mut lines = content.split('\n').count();
    if content.ends_with('\n') {
        lines -= 1;
    }
    lines
}

fn split_lines_for_counting(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn not_truncated(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let total_lines = count_lines(content);
    TruncationResult {
        content: content.to_string(),
        truncated: false,
        truncated_by: None,
        total_lines,
        total_bytes: content.len(),
        output_lines: total_lines,
        output_bytes: content.len(),
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// 从头部截断，保留前 N 行/字节（pi `truncateHead`）。
pub fn truncate_head(content: &str, options: TruncationOptions) -> TruncationResult {
    let max_lines = options.max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let max_bytes = options.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);

    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return not_truncated(content, max_lines, max_bytes);
    }

    if lines.first().map_or(0, |line| line.len()) > max_bytes {
        return TruncationResult {
            content: String::new(),
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
            max_lines,
            max_bytes,
        };
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut kept_bytes = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    for (index, line) in lines.iter().enumerate().take(max_lines) {
        let line_bytes = line.len() + usize::from(index > 0);
        if kept_bytes + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        kept.push(line);
        kept_bytes += line_bytes;
    }
    if kept.len() >= max_lines && kept_bytes <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }

    let output = kept.join("\n");
    let output_bytes = output.len();
    TruncationResult {
        content: output,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines: kept.len(),
        output_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// 从尾部截断，保留后 N 行/字节（pi `truncateTail`）。
pub fn truncate_tail(content: &str, options: TruncationOptions) -> TruncationResult {
    let max_lines = options.max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let max_bytes = options.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);

    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return not_truncated(content, max_lines, max_bytes);
    }

    let mut kept: Vec<String> = Vec::new();
    let mut kept_bytes = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    let mut last_line_partial = false;

    for line in lines.iter().rev() {
        if kept.len() >= max_lines {
            break;
        }
        let line_bytes = line.len() + usize::from(!kept.is_empty());
        if kept_bytes + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            if kept.is_empty() {
                // 唯一一行就超限：保留该行尾部，可能出现半行。
                let partial = truncate_string_to_bytes_from_end(line, max_bytes);
                kept_bytes = partial.len();
                kept.push(partial);
                last_line_partial = true;
            }
            break;
        }
        kept.push((*line).to_string());
        kept_bytes += line_bytes;
    }
    kept.reverse();
    if kept.len() >= max_lines && kept_bytes <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }

    let output = kept.join("\n");
    let output_bytes = output.len();
    TruncationResult {
        content: output,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines: kept.len(),
        output_bytes,
        last_line_partial,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// 保留字符串尾部至多 `max_bytes` 字节，且不切断 UTF-8 字符（pi `truncateStringToBytesFromEnd`）。
fn truncate_string_to_bytes_from_end(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut start = value.len() - max_bytes;
    while start < value.len() && !value.is_char_boundary(start) {
        start += 1;
    }
    value[start..].to_string()
}

/// 单行截断到 `max_chars` 个字符并追加 `... [truncated]`（pi `truncateLine`）。
pub fn truncate_line(line: &str, max_chars: usize) -> (String, bool) {
    if line.chars().count() <= max_chars {
        return (line.to_string(), false);
    }
    let head: String = line.chars().take(max_chars).collect();
    (format!("{head}... [truncated]"), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_matches_pi() {
        assert_eq!(format_size(0), "0B");
        assert_eq!(format_size(1023), "1023B");
        assert_eq!(format_size(1024), "1.0KB");
        assert_eq!(format_size(50 * 1024), "50.0KB");
        assert_eq!(format_size(1024 * 1024), "1.0MB");
    }

    #[test]
    fn short_content_is_returned_unchanged() {
        let result = truncate_head("one\ntwo\n", TruncationOptions::default());
        assert!(!result.truncated);
        assert_eq!(result.content, "one\ntwo\n");
        assert_eq!(result.total_lines, 2);
        assert_eq!(result.total_bytes, 8);
        assert_eq!(result.truncated_by, None);
    }

    #[test]
    fn head_truncation_stops_at_the_line_limit() {
        let content = "a\nb\nc\nd";
        let result = truncate_head(
            content,
            TruncationOptions {
                max_lines: Some(2),
                max_bytes: None,
            },
        );
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(result.content, "a\nb");
        assert_eq!(result.output_lines, 2);
        assert_eq!(result.total_lines, 4);
    }

    #[test]
    fn head_truncation_reports_first_line_over_byte_limit() {
        let content = "0123456789\nsecond";
        let result = truncate_head(
            content,
            TruncationOptions {
                max_lines: None,
                max_bytes: Some(4),
            },
        );
        assert!(result.first_line_exceeds_limit);
        assert_eq!(result.content, "");
        assert_eq!(result.output_lines, 0);
    }

    #[test]
    fn head_truncation_stops_at_the_byte_limit_without_partial_lines() {
        let content = "aaaa\nbbbb\ncccc";
        let result = truncate_head(
            content,
            TruncationOptions {
                max_lines: None,
                max_bytes: Some(9),
            },
        );
        assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
        assert_eq!(result.content, "aaaa\nbbbb");
        assert!(!result.last_line_partial);
    }

    #[test]
    fn tail_truncation_keeps_the_end() {
        let result = truncate_tail(
            "a\nb\nc\nd",
            TruncationOptions {
                max_lines: Some(2),
                max_bytes: None,
            },
        );
        assert_eq!(result.content, "c\nd");
        assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
    }

    #[test]
    fn tail_truncation_can_keep_a_partial_line() {
        let result = truncate_tail(
            "0123456789",
            TruncationOptions {
                max_lines: None,
                max_bytes: Some(4),
            },
        );
        assert_eq!(result.content, "6789");
        assert!(result.last_line_partial);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    }

    #[test]
    fn tail_truncation_respects_utf8_boundaries() {
        // "é" is two bytes; keeping 3 bytes must not split it.
        let result = truncate_tail(
            "aéé",
            TruncationOptions {
                max_lines: None,
                max_bytes: Some(3),
            },
        );
        assert_eq!(result.content, "é");
        assert!(result.content.is_char_boundary(result.content.len()));
    }

    #[test]
    fn truncate_line_appends_a_marker() {
        let (text, truncated) = truncate_line("abcdefghij", 4);
        assert_eq!(text, "abcd... [truncated]");
        assert!(truncated);
        let (text, truncated) = truncate_line("abc", 4);
        assert_eq!(text, "abc");
        assert!(!truncated);
    }
}
