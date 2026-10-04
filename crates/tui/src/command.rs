//! 斜杠命令表：逐条对齐 pi `coding-agent` 的 `BUILTIN_SLASH_COMMANDS`。
//!
//! 命令**名称与说明**与 pi 保持一致（这样从 pi 过来的人看到的是同一套菜单）；
//! [`SlashCommand::ported`] 标出本移植版是否真的实现了它，菜单里会给未实现的命令加
//! “尚未移植”的标注，而不是假装能用。

/// pi 的一条内置斜杠命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommand {
    /// 命令名（不带 `/`）。
    pub name: &'static str,
    /// 命令说明（与 pi 的 `description` 一致）。
    pub description: &'static str,
    /// 参数提示（pi 的 `argumentHint`）。
    pub argument_hint: Option<&'static str>,
    /// 本 Rust 移植版是否已经实现。
    pub ported: bool,
}

const fn cmd(
    name: &'static str,
    description: &'static str,
    argument_hint: Option<&'static str>,
    ported: bool,
) -> SlashCommand {
    SlashCommand {
        name,
        description,
        argument_hint,
        ported,
    }
}

/// pi `BUILTIN_SLASH_COMMANDS` 的完整列表（顺序也一致）。
pub const BUILTIN_SLASH_COMMANDS: &[SlashCommand] = &[
    cmd("settings", "Open settings menu", None, false),
    cmd(
        "model",
        "Select model (opens selector UI)",
        Some("<provider/model>"),
        true,
    ),
    cmd(
        "tree",
        "Navigate session tree (switch branches)",
        None,
        false,
    ),
    cmd("thinking", "Set thinking level", Some("<level>"), true),
    cmd(
        "scoped-models",
        "Enable/disable models for Ctrl+P cycling",
        None,
        false,
    ),
    cmd(
        "export",
        "Export session (HTML default, or specify path: .html/.jsonl)",
        None,
        false,
    ),
    cmd(
        "import",
        "Import and resume a session from a JSONL file",
        None,
        false,
    ),
    cmd(
        "share",
        "Share session as a secret GitHub gist",
        None,
        false,
    ),
    cmd(
        "bug",
        "Report a bug to the Pi developers",
        Some("<description>"),
        false,
    ),
    cmd("copy", "Copy last agent message to clipboard", None, false),
    cmd("name", "Set session display name", None, false),
    cmd("session", "Show session info and stats", None, true),
    cmd("changelog", "Show changelog entries", None, false),
    cmd("hotkeys", "Show all keyboard shortcuts", None, true),
    cmd(
        "fork",
        "Create a new fork from a previous user message",
        None,
        false,
    ),
    cmd(
        "clone",
        "Duplicate the current session at the current position",
        None,
        false,
    ),
    cmd(
        "trust",
        "Save project trust decision for future sessions",
        None,
        false,
    ),
    cmd(
        "login",
        "Configure provider authentication",
        Some("<provider>"),
        false,
    ),
    cmd("logout", "Remove provider authentication", None, false),
    cmd("new", "Start a new session", None, true),
    cmd(
        "compact",
        "Manually compact the session context",
        None,
        false,
    ),
    cmd("resume", "Resume a different session", None, false),
    cmd(
        "reload",
        "Reload keybindings, extensions, skills, prompts, themes, and context files",
        None,
        false,
    ),
    cmd("quit", "Quit tower-pi-agent", None, true),
];

/// 按名字查找一条内置命令。
#[must_use]
pub fn lookup(name: &str) -> Option<&'static SlashCommand> {
    BUILTIN_SLASH_COMMANDS
        .iter()
        .find(|command| command.name == name)
}

/// 按输入过滤命令列表（空输入返回全部）。
///
/// 前缀匹配排在子串匹配前面，同档保持 pi 表里的原始顺序——这样敲 `/m` 时
/// `model` 会排在 `compact` 前面。
#[must_use]
pub fn matching(query: &str) -> Vec<&'static SlashCommand> {
    let query = query.trim().to_ascii_lowercase();
    // 空查询时保持 pi 表里的原始顺序（菜单默认就是 pi 的顺序）。
    if query.is_empty() {
        return BUILTIN_SLASH_COMMANDS.iter().collect();
    }
    let mut matches: Vec<&'static SlashCommand> = BUILTIN_SLASH_COMMANDS
        .iter()
        .filter(|command| command.name.contains(&query))
        .collect();
    matches.sort_by_key(|command| (!command.name.starts_with(&query), command.name));
    matches
}

/// 编辑器里的斜杠菜单：输入以 `/` 开头且还没进入参数时返回过滤后的命令。
#[must_use]
pub fn menu(input: &str) -> Option<Vec<&'static SlashCommand>> {
    let query = input.strip_prefix('/')?;
    if query.contains(char::is_whitespace) {
        return None;
    }
    Some(matching(query))
}

/// `!` 之外的按键提示（`/hotkeys` 用）。
#[must_use]
pub fn hotkeys() -> &'static [&'static str] {
    &[
        "/        打开命令菜单（Tab 补全 · Enter 执行）",
        "i / Esc  插入 / 普通模式（vim 键位）",
        "Enter    普通模式下提交输入",
        "PgUp/PgDn 滚动会话记录",
        "Ctrl+C / q 退出",
    ]
}

/// 斜杠命令需要访问的会话能力；由宿主（`cli`）实现。
///
/// 命令在 TUI 的事件循环里**同步**执行，只做本地状态修改（不联网、不跑模型）：
/// 需要异步长任务的 pi 命令（`/compact`、`/export`…）在本移植版里尚未实现。
pub trait CommandHost: Send + Sync {
    /// `/new`：重置当前会话（清空 transcript，保留 system prompt 与工具）。
    fn new_session(&self) -> Result<(), String>;

    /// `/session`：会话信息，逐行返回。
    fn session_info(&self) -> Vec<String>;

    /// `/model [provider/model]`：无参数时返回当前模型说明，有参数时切换。
    fn model(&self, arg: Option<&str>) -> Result<String, String>;

    /// `/thinking [level]`：无参数时返回当前思考级别，有参数时设置。
    fn thinking(&self, arg: Option<&str>) -> Result<String, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_matches_pi_order_and_names() {
        let names: Vec<&str> = BUILTIN_SLASH_COMMANDS
            .iter()
            .map(|command| command.name)
            .collect();
        assert_eq!(
            names,
            vec![
                "settings",
                "model",
                "tree",
                "thinking",
                "scoped-models",
                "export",
                "import",
                "share",
                "bug",
                "copy",
                "name",
                "session",
                "changelog",
                "hotkeys",
                "fork",
                "clone",
                "trust",
                "login",
                "logout",
                "new",
                "compact",
                "resume",
                "reload",
                "quit",
            ]
        );
        assert_eq!(BUILTIN_SLASH_COMMANDS.len(), 24);
    }

    #[test]
    fn ported_commands_are_the_ones_we_implement() {
        let ported: Vec<&str> = BUILTIN_SLASH_COMMANDS
            .iter()
            .filter(|command| command.ported)
            .map(|command| command.name)
            .collect();
        assert_eq!(
            ported,
            vec!["model", "thinking", "session", "hotkeys", "new", "quit"]
        );
    }

    #[test]
    fn menu_needs_a_bare_slash_prefix() {
        assert!(menu("hello").is_none());
        assert!(menu("/model gpt").is_none(), "进入参数后关闭菜单");
        assert_eq!(menu("/").map(|items| items.len()), Some(24));
        assert!(menu("/qui").is_some());
    }

    #[test]
    fn matching_prefers_prefix_hits() {
        let items = matching("m");
        let names: Vec<&str> = items.iter().map(|command| command.name).collect();
        assert_eq!(names[0], "model", "前缀匹配优先：{names:?}");
        assert!(names.contains(&"compact"), "子串匹配仍保留：{names:?}");
        assert!(matching("nope").is_empty());
    }

    #[test]
    fn lookup_finds_builtins_only() {
        assert_eq!(lookup("quit").map(|command| command.ported), Some(true));
        assert!(lookup("nope").is_none());
    }
}
