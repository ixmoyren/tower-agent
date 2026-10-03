//! 工具入参的路径解析（对应 pi `coding-agent` 的 `core/tools/path-utils.ts` 与 `utils/paths.ts`）。

use std::path::{Component, Path, PathBuf};

const UNICODE_SPACES: [char; 12] = [
    '\u{00A0}', '\u{2000}', '\u{2001}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}',
    '\u{2007}', '\u{2008}', '\u{2009}', '\u{200A}',
];

/// 把 Unicode 空格变体规范化成普通空格。
fn normalize_unicode_spaces(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if UNICODE_SPACES.contains(&c) || c == '\u{202F}' || c == '\u{205F}' || c == '\u{3000}'
            {
                ' '
            } else {
                c
            }
        })
        .collect()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// 词法规范化：折叠 `.` 与 `..`，不触碰文件系统（对应 Node `path.resolve` 的规范化部分）。
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    result.push("..");
                }
            }
            other => result.push(other.as_os_str()),
        }
    }
    if result.as_os_str().is_empty() {
        result.push(".");
    }
    result
}

/// 展开路径：规范化 Unicode 空格、去掉 CLI 的 `@` 前缀、展开开头的 `~`（pi `expandPath`）。
pub fn expand_path(input: &str) -> String {
    let mut normalized = normalize_unicode_spaces(input);
    if let Some(stripped) = normalized.strip_prefix('@') {
        normalized = stripped.to_string();
    }
    if let Some(home) = home_dir() {
        if normalized == "~" {
            return home.to_string_lossy().into_owned();
        }
        if let Some(rest) = normalized.strip_prefix("~/") {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    normalized
}

/// 相对 `cwd` 解析路径（pi `resolveToCwd`）。
pub fn resolve_to_cwd(input: &str, cwd: &Path) -> PathBuf {
    let expanded = expand_path(input);
    let candidate = Path::new(&expanded);
    if candidate.is_absolute() {
        normalize_lexically(candidate)
    } else {
        normalize_lexically(&cwd.join(candidate))
    }
}

fn path_exists(path: &Path) -> bool {
    std::fs::metadata(path).is_ok()
}

/// macOS 截图文件名用窄不换行空格（U+202F）而非普通空格。
fn try_macos_screenshot_path(path: &Path) -> Option<PathBuf> {
    let value = path.to_string_lossy();
    let mut replaced = String::with_capacity(value.len());
    let mut chars = value.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c == ' ' {
            // 只有后面紧跟 AM/PM 时才替换，保持与 pi 的正则一致。
            let rest: String = chars.clone().map(|(_, c)| c).take(3).collect();
            let upper = rest.to_ascii_uppercase();
            if upper.starts_with("AM.") || upper.starts_with("PM.") {
                replaced.push('\u{202F}');
                continue;
            }
        }
        replaced.push(c);
    }
    (replaced != value).then(|| PathBuf::from(replaced))
}

/// 读取时解析路径，尝试 macOS 上的常见变体（pi `resolveReadPath`）。
pub fn resolve_read_path(input: &str, cwd: &Path) -> PathBuf {
    let resolved = resolve_to_cwd(input, cwd);
    if path_exists(&resolved) {
        return resolved;
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(variant) = try_macos_screenshot_path(&resolved) {
        candidates.push(variant);
    }
    // macOS 截图名里用 U+2019 弯引号，而用户通常输入直引号。
    let curly = PathBuf::from(resolved.to_string_lossy().replace('\'', "\u{2019}"));
    candidates.push(curly);

    for candidate in candidates {
        if candidate != resolved && path_exists(&candidate) {
            return candidate;
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_path_normalizes_spaces_and_strips_at_prefix() {
        assert_eq!(expand_path("@foo/bar"), "foo/bar");
        assert_eq!(expand_path("a\u{00A0}b"), "a b");
        assert_eq!(expand_path("a\u{202F}b"), "a b");
        assert_eq!(expand_path("a\u{3000}b"), "a b");
    }

    #[test]
    fn resolve_to_cwd_joins_relative_paths_and_normalizes() {
        let cwd = Path::new("/tmp/work");
        assert_eq!(
            resolve_to_cwd("src/./lib.rs", cwd),
            PathBuf::from("/tmp/work/src/lib.rs")
        );
        assert_eq!(resolve_to_cwd("../other", cwd), PathBuf::from("/tmp/other"));
        assert_eq!(
            resolve_to_cwd("/abs/path/../x", cwd),
            PathBuf::from("/abs/x")
        );
    }

    #[test]
    fn resolve_to_cwd_expands_tilde() {
        // HOME is always set in the test environment; compare against the same source.
        let home = std::env::var("HOME").expect("HOME is set");
        assert_eq!(
            resolve_to_cwd("~/x", Path::new("/")),
            PathBuf::from(format!("{home}/x"))
        );
    }
}
