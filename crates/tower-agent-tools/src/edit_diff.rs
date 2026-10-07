//! `edit` 的文本匹配与替换算法（对应 pi `coding-agent` 的 `core/tools/edit-diff.ts`）。
//!
//! 规则：所有 `edits[].oldText` 都相对**原始文件**匹配；先精确匹配，失败后退化为
//! 模糊匹配（NFKC、行尾空白、弯引号/短破折号/特殊空格规范化）；每个 `oldText` 必须唯一；
//! 匹配区间不得重叠。命中模糊匹配时，替换在规范化内容上完成，再把未改动的行按原样回填。

use unicode_normalization::UnicodeNormalization as _;

/// 文件的行尾风格。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    /// `\n`
    Lf,
    /// `\r\n`
    Crlf,
}

/// 一次替换。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// 必须精确（或模糊）匹配的原文。
    pub old_text: String,
    /// 替换文本。
    pub new_text: String,
}

/// 把一个已匹配的替换记录下来。
#[derive(Debug, Clone)]
struct MatchedEdit {
    edit_index: usize,
    match_index: usize,
    match_length: usize,
    new_text: String,
}

/// 拆分 BOM（pi `splitBom`）。
pub fn split_bom(content: &str) -> (&str, &str) {
    match content.strip_prefix('\u{FEFF}') {
        Some(rest) => ("\u{FEFF}", rest),
        None => ("", content),
    }
}

/// 检测文件主要使用的行尾（pi `detectLineEnding`）。
pub fn detect_line_ending(content: &str) -> LineEnding {
    let crlf = content.find("\r\n");
    let lf = content.find('\n');
    match (crlf, lf) {
        (Some(crlf), Some(lf)) if crlf < lf => LineEnding::Crlf,
        _ => LineEnding::Lf,
    }
}

/// 归一化到 LF（pi `normalizeToLF`）。
pub fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// 还原行尾（pi `restoreLineEndings`）。
pub fn restore_line_endings(text: &str, ending: LineEnding) -> String {
    match ending {
        LineEnding::Crlf => text.replace('\n', "\r\n"),
        LineEnding::Lf => text.to_string(),
    }
}

/// 用于模糊匹配的规范化（pi `normalizeForFuzzyMatch`）。
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    let nfkc: String = text.nfkc().collect();
    let trimmed = nfkc
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    trimmed
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

/// 模糊匹配结果。
#[derive(Debug, Clone)]
pub struct FuzzyMatch {
    /// 匹配起点（在被替换的内容中的字节偏移）。
    pub index: usize,
    /// 匹配长度（字节）。
    pub match_length: usize,
    /// 是否使用了模糊匹配。
    pub used_fuzzy: bool,
    /// 应当用于替换的内容：精确匹配时为原文，模糊匹配时为规范化内容。
    pub content_for_replacement: String,
}

/// 精确优先、失败后退化为模糊匹配（pi `fuzzyFindText`）。
pub fn fuzzy_find_text(content: &str, old_text: &str) -> Option<FuzzyMatch> {
    if let Some(index) = content.find(old_text) {
        return Some(FuzzyMatch {
            index,
            match_length: old_text.len(),
            used_fuzzy: false,
            content_for_replacement: content.to_string(),
        });
    }

    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old = normalize_for_fuzzy_match(old_text);
    let index = fuzzy_content.find(&fuzzy_old)?;
    Some(FuzzyMatch {
        index,
        match_length: fuzzy_old.len(),
        used_fuzzy: true,
        content_for_replacement: fuzzy_content,
    })
}

fn count_occurrences(content: &str, old_text: &str) -> usize {
    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old = normalize_for_fuzzy_match(old_text);
    if fuzzy_old.is_empty() {
        return fuzzy_content.chars().count() + 1;
    }
    fuzzy_content.matches(&fuzzy_old).count()
}

fn not_found_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        format!(
            "Could not find the exact text in {path}. The old text must match exactly including all \
             whitespace and newlines."
        )
    } else {
        format!(
            "Could not find edits[{edit_index}] in {path}. The oldText must match exactly including \
             all whitespace and newlines."
        )
    }
}

fn duplicate_error(
    path: &str,
    edit_index: usize,
    total_edits: usize,
    occurrences: usize,
) -> String {
    if total_edits == 1 {
        format!(
            "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please \
             provide more context to make it unique."
        )
    } else {
        format!(
            "Found {occurrences} occurrences of edits[{edit_index}] in {path}. Each oldText must be \
             unique. Please provide more context to make it unique."
        )
    }
}

fn empty_old_text_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        format!("oldText must not be empty in {path}.")
    } else {
        format!("edits[{edit_index}].oldText must not be empty in {path}.")
    }
}

fn no_change_error(path: &str, total_edits: usize) -> String {
    if total_edits == 1 {
        format!(
            "No changes made to {path}. The replacement produced identical content. This might \
             indicate an issue with special characters or the text not existing as expected."
        )
    } else {
        format!("No changes made to {path}. The replacements produced identical content.")
    }
}

/// 把一组替换应用到 LF 规范化后的内容（pi `applyEditsToNormalizedContent`）。
pub fn apply_edits_to_normalized_content(
    normalized_content: &str,
    edits: &[Edit],
    path: &str,
) -> Result<(String, String), String> {
    let normalized_edits: Vec<Edit> = edits
        .iter()
        .map(|edit| Edit {
            old_text: normalize_to_lf(&edit.old_text),
            new_text: normalize_to_lf(&edit.new_text),
        })
        .collect();
    let total = normalized_edits.len();

    for (index, edit) in normalized_edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(empty_old_text_error(path, index, total));
        }
    }

    let used_fuzzy = normalized_edits.iter().any(|edit| {
        fuzzy_find_text(normalized_content, &edit.old_text).is_some_and(|m| m.used_fuzzy)
    });
    let replacement_base = if used_fuzzy {
        normalize_for_fuzzy_match(normalized_content)
    } else {
        normalized_content.to_string()
    };

    let mut matched: Vec<MatchedEdit> = Vec::new();
    for (index, edit) in normalized_edits.iter().enumerate() {
        let Some(found) = fuzzy_find_text(&replacement_base, &edit.old_text) else {
            return Err(not_found_error(path, index, total));
        };
        let occurrences = count_occurrences(&replacement_base, &edit.old_text);
        if occurrences > 1 {
            return Err(duplicate_error(path, index, total, occurrences));
        }
        matched.push(MatchedEdit {
            edit_index: index,
            match_index: found.index,
            match_length: found.match_length,
            new_text: edit.new_text.clone(),
        });
    }

    matched.sort_by_key(|edit| edit.match_index);
    for pair in matched.windows(2) {
        let previous = &pair[0];
        let current = &pair[1];
        if previous.match_index + previous.match_length > current.match_index {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target \
                 disjoint regions.",
                previous.edit_index, current.edit_index
            ));
        }
    }

    let base_content = normalized_content.to_string();
    let new_content = if used_fuzzy {
        apply_replacements_preserving_unchanged_lines(
            normalized_content,
            &replacement_base,
            &matched,
        )?
    } else {
        apply_replacements(&replacement_base, &matched)
    };

    if base_content == new_content {
        return Err(no_change_error(path, total));
    }
    Ok((base_content, new_content))
}

fn apply_replacements(content: &str, replacements: &[MatchedEdit]) -> String {
    let mut result = content.to_string();
    for replacement in replacements.iter().rev() {
        result.replace_range(
            replacement.match_index..replacement.match_index + replacement.match_length,
            &replacement.new_text,
        );
    }
    result
}

fn split_lines_with_endings(content: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, _) in content.match_indices('\n') {
        lines.push(&content[start..=index]);
        start = index + 1;
    }
    if start < content.len() {
        lines.push(&content[start..]);
    }
    lines
}

fn get_line_spans(content: &str) -> (Vec<&str>, Vec<(usize, usize)>) {
    let lines = split_lines_with_endings(content);
    let mut spans = Vec::with_capacity(lines.len());
    let mut offset = 0;
    for line in &lines {
        spans.push((offset, offset + line.len()));
        offset += line.len();
    }
    (lines, spans)
}

fn replacement_line_range(
    spans: &[(usize, usize)],
    replacement: &MatchedEdit,
) -> Result<(usize, usize), String> {
    let start_offset = replacement.match_index;
    let end_offset = replacement.match_index + replacement.match_length;

    let start_line = spans
        .iter()
        .position(|(start, end)| start_offset >= *start && start_offset < *end)
        .ok_or_else(|| "Replacement range is outside the base content.".to_string())?;

    let mut end_line = start_line;
    while end_line < spans.len() && spans[end_line].1 < end_offset {
        end_line += 1;
    }
    if end_line >= spans.len() {
        return Err("Replacement range is outside the base content.".to_string());
    }
    Ok((start_line, end_line + 1))
}

/// 在规范化内容上替换，但把未改动的行按原始内容回填（pi `applyReplacementsPreservingUnchangedLines`）。
fn apply_replacements_preserving_unchanged_lines(
    original_content: &str,
    base_content: &str,
    replacements: &[MatchedEdit],
) -> Result<String, String> {
    let original_lines = split_lines_with_endings(original_content);
    let (_, base_spans) = get_line_spans(base_content);
    if original_lines.len() != base_spans.len() {
        return Err(
            "Cannot preserve unchanged lines because the base content has a different line count."
                .to_string(),
        );
    }

    let mut sorted = replacements.to_vec();
    sorted.sort_by_key(|edit| edit.match_index);

    let mut groups: Vec<(usize, usize, Vec<MatchedEdit>)> = Vec::new();
    for replacement in sorted {
        let (start_line, end_line) = replacement_line_range(&base_spans, &replacement)?;
        if let Some(current) = groups.last_mut()
            && start_line < current.1
        {
            current.1 = current.1.max(end_line);
            current.2.push(replacement);
            continue;
        }
        groups.push((start_line, end_line, vec![replacement]));
    }

    let mut original_line_index = 0usize;
    let mut result = String::new();
    for (start_line, end_line, group_replacements) in groups {
        for line in &original_lines[original_line_index..start_line] {
            result.push_str(line);
        }
        let group_start = base_spans[start_line].0;
        let group_end = base_spans[end_line - 1].1;
        let group_content = &base_content[group_start..group_end];
        let group_replacements: Vec<MatchedEdit> = group_replacements
            .into_iter()
            .map(|mut edit| {
                edit.match_index -= group_start;
                edit
            })
            .collect();
        result.push_str(&apply_replacements(group_content, &group_replacements));
        original_line_index = end_line;
    }
    for line in &original_lines[original_line_index..] {
        result.push_str(line);
    }
    Ok(result)
}

/// 生成面向展示的行号 diff（pi `generateDiffString` 的精简版）。
///
/// 与 pi 的差别：不做「长段未改动内容中间省略」的压缩，直接输出全量上下文。
pub fn generate_diff_string(old_content: &str, new_content: &str) -> (String, Option<usize>) {
    use similar::{ChangeTag, TextDiff};

    let diff = TextDiff::from_lines(old_content, new_content);
    let old_lines = old_content.split('\n').collect::<Vec<_>>();
    let new_lines = new_content.split('\n').collect::<Vec<_>>();
    let width = old_lines.len().max(new_lines.len()).to_string().len();

    let mut output = Vec::new();
    let mut old_line = 1usize;
    let mut new_line = 1usize;
    let mut first_changed = None;
    for change in diff.iter_all_changes() {
        let value = change.value().strip_suffix('\n').unwrap_or(change.value());
        match change.tag() {
            ChangeTag::Insert => {
                first_changed.get_or_insert(new_line);
                output.push(format!("+{:>width$} {}", new_line, value, width = width));
                new_line += 1;
            }
            ChangeTag::Delete => {
                first_changed.get_or_insert(new_line);
                output.push(format!("-{:>width$} {}", old_line, value, width = width));
                old_line += 1;
            }
            ChangeTag::Equal => {
                output.push(format!(" {:>width$} {}", old_line, value, width = width));
                old_line += 1;
                new_line += 1;
            }
        }
    }
    (output.join("\n"), first_changed)
}

/// 生成标准 unified patch（pi `generateUnifiedPatch`）。
pub fn generate_unified_patch(path: &str, old_content: &str, new_content: &str) -> String {
    use similar::TextDiff;
    TextDiff::from_lines(old_content, new_content)
        .unified_diff()
        .context_radius(4)
        .header(path, path)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old_text: &str, new_text: &str) -> Edit {
        Edit {
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
        }
    }

    #[test]
    fn detect_and_restore_line_endings() {
        assert_eq!(detect_line_ending("a\r\nb\r\n"), LineEnding::Crlf);
        assert_eq!(detect_line_ending("a\nb\r\n"), LineEnding::Lf);
        assert_eq!(restore_line_endings("a\nb", LineEnding::Crlf), "a\r\nb");
        assert_eq!(normalize_to_lf("a\r\nb\rc"), "a\nb\nc");
    }

    #[test]
    fn single_exact_edit_replaces_the_text() {
        let (base, new) =
            apply_edits_to_normalized_content("hello world\n", &[edit("world", "there")], "f.txt")
                .unwrap();
        assert_eq!(base, "hello world\n");
        assert_eq!(new, "hello there\n");
    }

    #[test]
    fn missing_text_reports_pi_error_message() {
        let error = apply_edits_to_normalized_content("hello\n", &[edit("missing", "x")], "f.txt")
            .unwrap_err();
        assert_eq!(
            error,
            "Could not find the exact text in f.txt. The old text must match exactly including all \
             whitespace and newlines."
        );
    }

    #[test]
    fn duplicate_text_is_rejected() {
        let error =
            apply_edits_to_normalized_content("x\nx\n", &[edit("x", "y")], "f.txt").unwrap_err();
        assert!(error.starts_with("Found 2 occurrences of the text in f.txt."));
    }

    #[test]
    fn overlapping_edits_are_rejected() {
        let error = apply_edits_to_normalized_content(
            "abcdef\n",
            &[edit("abc", "X"), edit("bcd", "Y")],
            "f.txt",
        )
            .unwrap_err();
        assert!(error.contains("overlap"), "got: {error}");
    }

    #[test]
    fn no_op_edit_is_rejected() {
        let error = apply_edits_to_normalized_content("same\n", &[edit("same", "same")], "f.txt")
            .unwrap_err();
        assert!(error.contains("No changes made to f.txt"));
    }

    #[test]
    fn fuzzy_match_ignores_trailing_whitespace() {
        let (_, new) = apply_edits_to_normalized_content(
            "line one   \nline two\n",
            &[edit("line one\nline two", "replaced")],
            "f.txt",
        )
            .unwrap();
        assert_eq!(new, "replaced\n");
    }

    #[test]
    fn fuzzy_match_normalizes_smart_quotes() {
        let (_, new) = apply_edits_to_normalized_content(
            "say \u{201C}hi\u{201D}\n",
            &[edit("\"hi\"", "\"yo\"")],
            "f.txt",
        )
            .unwrap();
        assert_eq!(new, "say \"yo\"\n");
    }

    #[test]
    fn multiple_disjoint_edits_apply_against_the_original() {
        let (_, new) = apply_edits_to_normalized_content(
            "alpha\nbeta\ngamma\n",
            &[edit("alpha", "ALPHA"), edit("gamma", "GAMMA")],
            "f.txt",
        )
            .unwrap();
        assert_eq!(new, "ALPHA\nbeta\nGAMMA\n");
    }
}
