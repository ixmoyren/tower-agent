//! 命令行参数解析（对应 pi `coding-agent` 的 CLI 入口参数）。
//!
//! 参数表用 `clap` 的 derive 定义：帮助、版本与参数错误都由 clap 统一生成，
//! 因此不再手写解析循环与用法字符串。`--provider` / `--model` 的取值语义留给
//! [`crate::provider`] 校验（这样"未知 provider / 缺凭据"的错误文案保持一致）。

use std::{ffi::OsString, path::PathBuf};

use clap::{Parser, builder::NonEmptyStringValueParser};
use tower_agent_codemode_core::ScriptLanguage;

/// 配置作用域（对应 storage 的 `ConfigScope`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ConfigScopeArg {
    /// 用户级：跨项目共享。
    User,
    /// 项目级：当前工作目录（`-C/--cwd`）。
    Project,
}

/// 进程级默认 codemode 引擎。
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CodemodeEngineArg {
    /// monty Python-subset engine.
    Monty,
    /// rquickjs JavaScript engine.
    Js,
}

impl From<CodemodeEngineArg> for ScriptLanguage {
    fn from(value: CodemodeEngineArg) -> Self {
        match value {
            CodemodeEngineArg::Monty => Self::Monty,
            CodemodeEngineArg::Js => Self::JavaScript,
        }
    }
}

/// 解析后的命令行参数。
#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(
    name = "tower-agent",
    version,
    about = "tower-agent: a Rust port of the pi (TypeScript agent framework) tower-agent",
    disable_help_flag = true,
    disable_version_flag = true,
    help_template = "{about-with-newline}\nUsage: {usage}\n\nArguments:\n{positionals}\n\nOptions:\n{options}"
)]
pub struct CliArgs {
    /// 一次性提示词；缺省时从 stdin 读取全部内容。
    #[arg(
        value_name = "prompt",
        help = "One-shot prompt; reads all of stdin when omitted."
    )]
    pub prompt: Vec<String>,

    /// 工具的工作目录（`-C` / `--cwd`）。
    #[arg(
        short = 'C',
        long = "cwd",
        value_name = "dir",
        help = "Working directory for the tools."
    )]
    pub cwd: Option<PathBuf>,

    /// 非交互模式：处理完提示词后退出（对应 pi `-p` / `--print`）。
    #[arg(
        short = 'p',
        long = "print",
        help = "Non-interactive: process the prompt, then exit."
    )]
    pub print: bool,

    /// 只列出内置工具后退出。
    #[arg(long = "list-tools", help = "List the built-in tools, then exit.")]
    pub list_tools: bool,

    /// 显式启用 JavaScript codemode 沙箱。
    #[arg(
        long = "codemode",
        conflicts_with = "no_codemode",
        help = "Enable the JavaScript codemode sandbox (coreutils + filesystem)."
    )]
    pub codemode: bool,

    /// 关闭全部 codemode 宿主、工具与沙箱。
    #[arg(
        long = "no-codemode",
        help = "Disable all codemode hosts, tools, and sandboxes."
    )]
    pub no_codemode: bool,

    /// codemode 的进程级默认引擎；脚本元数据中的 language 仍可覆盖它。
    #[arg(
        long = "codemode-engine",
        value_name = "monty|js",
        value_enum,
        help = "Default codemode engine: monty or js (script metadata can override it)."
    )]
    pub codemode_engine: Option<CodemodeEngineArg>,

    /// codemode 的命名会话：目录固定在 `<config>/tower-agent/codemode/sessions/<name>`。
    #[arg(
        long = "codemode-session",
        value_name = "name",
        value_parser = NonEmptyStringValueParser::new(),
        help = "Persistent codemode session name (reuses <config>/tower-agent/codemode/sessions/<name>)."
    )]
    pub codemode_session: Option<String>,

    /// 额外挂载，可重复：`<dir>[:<guest>][:ro|rw]`（缺省 guest=/work、只读）。
    #[arg(
        long = "codemode-mount",
        value_name = "dir[:guest][:ro|rw]",
        help = "Extra codemode mount (repeatable): <dir>[:<guest>][:ro|rw]."
    )]
    pub codemode_mounts: Vec<String>,

    /// 列出已持久化的 codemode 会话，然后退出。
    #[arg(
        long = "codemode-sessions",
        help = "List persistent codemode sessions, then exit."
    )]
    pub codemode_sessions: bool,

    /// 删掉一个持久化的 codemode 会话（连同里面的产物），然后退出。
    #[arg(
        long = "codemode-session-clear",
        value_name = "name",
        value_parser = NonEmptyStringValueParser::new(),
        help = "Remove a persistent codemode session (and its artifacts), then exit."
    )]
    pub codemode_session_clear: Option<String>,

    /// 选择 provider：anthropic / openai / openai-responses / google（缺省时按环境里的凭据挑）。
    #[arg(
        long,
        value_name = "provider",
        value_parser = NonEmptyStringValueParser::new(),
        help = "Provider to use: anthropic / openai / openai-responses / google / deepseek \
                (defaults to the first one with credentials in the environment)."
    )]
    pub provider: Option<String>,

    /// 一次性 API key（覆盖环境变量；对应 pi `--api-key`）。
    #[arg(
        long = "api-key",
        value_name = "key",
        help = "One-off API key (overrides the environment variable)."
    )]
    pub api_key: Option<String>,

    /// 模型 id（默认取 provider 的默认模型）。
    #[arg(
        long,
        value_name = "id",
        value_parser = NonEmptyStringValueParser::new(),
        help = "Model id (defaults to the provider's default model)."
    )]
    pub model: Option<String>,

    /// 思考级别：off / minimal / low / medium / high / xhigh / max（对应 pi `--thinking`）。
    #[arg(
        long,
        value_name = "level",
        help = "Thinking level: off / minimal / low / medium / high / xhigh / max."
    )]
    pub thinking: Option<String>,

    /// 只启用列出的内置工具，逗号分隔（对应 pi `-t` / `--tools`）。
    #[arg(
        short = 't',
        long = "tools",
        value_name = "tools",
        value_delimiter = ',',
        value_parser = trim_value,
        help = "Only enable the listed built-in tools (comma-separated)."
    )]
    pub tools: Vec<String>,

    /// 排除列出的内置工具，逗号分隔（对应 pi `-xt` / `--exclude-tools`）。
    #[arg(
        long = "exclude-tools",
        value_name = "tools",
        value_delimiter = ',',
        value_parser = trim_value,
        help = "Exclude the listed built-in tools (comma-separated)."
    )]
    pub exclude_tools: Vec<String>,

    /// 禁用全部内置工具（对应 pi `-nt` / `--no-tools`）。
    #[arg(long = "no-tools", help = "Disable all built-in tools.")]
    pub no_tools: bool,

    /// 禁用内置工具（本移植版没有扩展工具，等同 `--no-tools`；对应 pi `-nbt`）。
    #[arg(
        long = "no-builtin-tools",
        help = "Disable built-in tools (this port has no extension tools, so it equals --no-tools)."
    )]
    pub no_builtin_tools: bool,

    /// 读取一项配置后退出：默认读「项目覆盖用户」的有效值，给了 `--config-scope` 则读该作用域。
    #[arg(
        long = "config-get",
        value_name = "key",
        value_parser = NonEmptyStringValueParser::new(),
        help = "Print a config value, then exit (effective value unless --config-scope is given)."
    )]
    pub config_get: Option<String>,

    /// 写入一项配置后退出：`<key>=<value>`；value 先按 JSON 解析，失败则当字符串。
    #[arg(
        long = "config-set",
        value_name = "key=value",
        help = "Set a config value (`<key>=<value>`; value is JSON if parseable, else a string), then exit."
    )]
    pub config_set: Option<String>,

    /// 删除一项配置后退出（作用域见 `--config-scope`）。
    #[arg(
        long = "config-unset",
        value_name = "key",
        value_parser = NonEmptyStringValueParser::new(),
        help = "Delete a config key, then exit."
    )]
    pub config_unset: Option<String>,

    /// 列出配置后退出：默认列合并后的有效配置，给了 `--config-scope` 则列该作用域。
    #[arg(
        long = "config-list",
        help = "List config (effective by default, or the scope given by --config-scope), then exit."
    )]
    pub config_list: bool,

    /// `--config-set` / `--config-unset` 的目标作用域（默认 user）。
    #[arg(
        long = "config-scope",
        value_name = "scope",
        value_enum,
        help = "Scope for --config-set / --config-unset (user or project; default user)."
    )]
    pub config_scope: Option<ConfigScopeArg>,

    /// 继续当前工作目录下最近的一次会话（对应 pi `--continue`）。
    #[arg(
        long = "continue",
        help = "Continue the most recent session in the working directory."
    )]
    pub continue_session: bool,

    /// 继续指定 id 的会话（对应 pi `--session`）。
    #[arg(
        long = "session",
        value_name = "id",
        value_parser = NonEmptyStringValueParser::new(),
        help = "Continue the session with the given id."
    )]
    pub session: Option<String>,

    /// 不保存会话 transcript（对应 pi `--no-session`）。
    #[arg(long = "no-session", help = "Do not persist the session transcript.")]
    pub no_session: bool,

    /// 打印帮助后退出。
    #[arg(
        short = 'h',
        long = "help",
        action = clap::ArgAction::Help,
        help = "Print help and exit."
    )]
    pub help: Option<bool>,

    /// 打印版本后退出。
    #[arg(
        short = 'v',
        long = "version",
        action = clap::ArgAction::Version,
        help = "Print version and exit."
    )]
    pub version: Option<bool>,
}

impl CliArgs {
    /// 把位置参数拼成一条提示词（没有给出位置参数时为 `None`）。
    #[must_use]
    pub fn prompt_text(&self) -> Option<String> {
        (!self.prompt.is_empty()).then(|| self.prompt.join(" "))
    }
}

/// 解析命令行参数。
///
/// `args` **不含程序名**（这样单元测试可以只写选项）；`--help` / `--version` 与参数错误
/// 都以 [`clap::Error`] 返回，由调用方决定怎么打印与退出。
pub fn parse<I, S>(args: I) -> Result<CliArgs, clap::Error>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut argv: Vec<OsString> = vec![OsString::from("tower-agent")];
    argv.extend(args.into_iter().map(Into::into).map(expand_pi_shorthand));
    CliArgs::try_parse_from(argv)
}

/// pi 的几组短选项是多字母的（`-nt` / `-nbt` / `-xt`），clap 的短选项只能是单字符，
/// 所以解析前先把它们展开成等价的长选项，保证 `pi` 的命令行能照抄。
fn expand_pi_shorthand(arg: OsString) -> OsString {
    match arg.to_str() {
        Some("-nt") => OsString::from("--no-tools"),
        Some("-nbt") => OsString::from("--no-builtin-tools"),
        Some("-xt") => OsString::from("--exclude-tools"),
        _ => arg,
    }
}

/// 去掉逗号分隔项前后的空白（pi 的 `--tools` / `--exclude-tools` 会 trim）。
fn trim_value(value: &str) -> Result<String, std::convert::Infallible> {
    Ok(value.trim().to_string())
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;

    use super::*;

    fn parse_ok(args: &[&str]) -> CliArgs {
        parse(args.iter().copied()).expect("args parse")
    }

    #[test]
    fn no_arguments_yields_defaults() {
        let args = parse_ok(&[]);
        assert!(args.prompt.is_empty());
        assert_eq!(args.prompt_text(), None);
        assert_eq!(args.cwd, None);
        assert!(!args.print);
        assert!(!args.list_tools);
        assert_eq!(args.provider, None);
        assert_eq!(args.api_key, None);
        assert_eq!(args.model, None);
        assert_eq!(args.thinking, None);
        assert!(args.tools.is_empty());
        assert!(args.exclude_tools.is_empty());
        assert!(!args.no_tools);
        assert!(!args.no_builtin_tools);
    }

    #[test]
    fn positional_words_join_into_one_prompt() {
        let args = parse_ok(&["看看", "这个", "仓库"]);
        assert_eq!(args.prompt_text().as_deref(), Some("看看 这个 仓库"));
    }

    #[test]
    fn flags_and_cwd_are_parsed() {
        let args = parse_ok(&["--print", "-C", "/tmp/work", "hi"]);
        assert!(args.print);
        assert_eq!(args.cwd, Some(PathBuf::from("/tmp/work")));
        assert_eq!(args.prompt_text().as_deref(), Some("hi"));

        let args = parse_ok(&["--cwd=/tmp/other"]);
        assert_eq!(args.cwd, Some(PathBuf::from("/tmp/other")));
    }

    #[test]
    fn provider_and_model_are_parsed() {
        let args = parse_ok(&["--provider", "anthropic", "--model", "claude-test", "hi"]);
        assert_eq!(args.provider.as_deref(), Some("anthropic"));
        assert_eq!(args.model.as_deref(), Some("claude-test"));
        assert_eq!(args.prompt_text().as_deref(), Some("hi"));

        let args = parse_ok(&["--provider=google", "--model=gemini-test"]);
        assert_eq!(args.provider.as_deref(), Some("google"));
        assert_eq!(args.model.as_deref(), Some("gemini-test"));
    }

    #[test]
    fn double_dash_stops_option_parsing() {
        let args = parse_ok(&["--", "--print", "-x"]);
        assert!(!args.print);
        assert_eq!(args.prompt_text().as_deref(), Some("--print -x"));
    }

    #[test]
    fn unknown_options_are_rejected() {
        let error = parse(["--nope"]).expect_err("unknown option");
        assert_eq!(error.kind(), ErrorKind::UnknownArgument);
        assert!(error.to_string().contains("--nope"), "got: {error}");
    }

    #[test]
    fn options_that_need_values_are_rejected_when_missing() {
        for args in [
            vec!["-C"],
            vec!["--provider"],
            vec!["--model"],
            vec!["--provider="],
            vec!["--model="],
        ] {
            assert!(
                parse(args.clone()).is_err(),
                "`{args:?}` 应当报错（缺取值）"
            );
        }
    }

    #[test]
    fn help_and_version_are_handled_by_clap() {
        let help = parse(["--help"]).expect_err("--help 由 clap 处理");
        assert_eq!(help.kind(), ErrorKind::DisplayHelp);
        assert!(!help.use_stderr());
        assert!(help.to_string().contains("Usage"), "got: {help}");

        // pi 的版本短选项是 `-v`（clap 默认的 `-V` 已关闭）。
        let version = parse(["-v"]).expect_err("-v 由 clap 处理");
        assert_eq!(version.kind(), ErrorKind::DisplayVersion);
        assert!(
            version.to_string().starts_with("tower-agent "),
            "got: {version}"
        );
    }

    #[test]
    fn print_and_thinking_accept_pi_spellings() {
        let args = parse_ok(&["-p", "--thinking", "high", "hi"]);
        assert!(args.print);
        assert_eq!(args.thinking.as_deref(), Some("high"));
        assert_eq!(args.prompt_text().as_deref(), Some("hi"));

        let args = parse_ok(&[
            "--provider",
            "anthropic",
            "--model",
            "m",
            "--api-key",
            "sk-x",
        ]);
        assert_eq!(args.api_key.as_deref(), Some("sk-x"));
    }

    #[test]
    fn tool_selection_flags_match_pi() {
        let args = parse_ok(&["-t", "read,grep", "hi"]);
        assert_eq!(args.tools, vec!["read", "grep"]);

        let args = parse_ok(&["--tools", "read, grep"]);
        assert_eq!(args.tools, vec!["read", "grep"]);

        let args = parse_ok(&["-xt", "shell,write"]);
        assert_eq!(args.exclude_tools, vec!["shell", "write"]);

        let args = parse_ok(&["-nt"]);
        assert!(args.no_tools);

        let args = parse_ok(&["-nbt"]);
        assert!(args.no_builtin_tools);
    }

    #[test]
    fn config_flags_parse() {
        let args = parse_ok(&["--config-set", "model=gpt", "--config-scope", "project"]);
        assert_eq!(args.config_set.as_deref(), Some("model=gpt"));
        assert_eq!(args.config_scope, Some(ConfigScopeArg::Project));
        assert!(!args.config_list);

        let args = parse_ok(&["--config-list"]);
        assert!(args.config_list);
        assert_eq!(args.config_scope, None);

        let args = parse_ok(&["--config-get", "model", "--config-unset", "theme"]);
        assert_eq!(args.config_get.as_deref(), Some("model"));
        assert_eq!(args.config_unset.as_deref(), Some("theme"));

        // 作用域只认 user / project。
        assert!(parse(["--config-scope", "nope"]).is_err());
    }

    #[test]
    fn codemode_engine_cli_flag_overrides_config() {
        use tower_agent_codemode_core::EngineSelection;

        let args = parse_ok(&["--codemode-engine", "monty"]);
        let selection = EngineSelection {
            cli: args.codemode_engine.map(Into::into),
            config: Some(ScriptLanguage::JavaScript),
        };
        assert_eq!(selection.process_default(), ScriptLanguage::Monty);
        assert_eq!(
            ScriptLanguage::from(
                parse_ok(&["--codemode-engine", "js"])
                    .codemode_engine
                    .expect("engine"),
            ),
            ScriptLanguage::JavaScript
        );
        assert!(parse(["--codemode-engine", "python"]).is_err());
    }

    #[test]
    fn codemode_enable_and_disable_flags_conflict() {
        assert!(parse_ok(&["--codemode"]).codemode);
        assert!(parse_ok(&["--no-codemode"]).no_codemode);
        assert!(parse(["--codemode", "--no-codemode"]).is_err());
        assert!(parse(["--no-mods"]).is_err());
    }

    #[test]
    fn codemode_config_engine_is_used_without_cli_override() {
        use tower_agent_codemode_core::EngineSelection;

        let selection = EngineSelection {
            cli: None,
            config: ScriptLanguage::parse("monty"),
        };
        assert_eq!(selection.process_default(), ScriptLanguage::Monty);
        assert_eq!(
            EngineSelection::default().process_default(),
            ScriptLanguage::JavaScript
        );
    }

    #[test]
    fn codemode_script_metadata_overrides_process_default() {
        use tower_agent_codemode_core::{EngineSelection, ScriptMetadata};

        let selection = EngineSelection {
            cli: Some(ScriptLanguage::Monty),
            config: Some(ScriptLanguage::JavaScript),
        };
        assert_eq!(
            selection.resolve(Some(ScriptMetadata::with_language(
                ScriptLanguage::JavaScript
            ))),
            ScriptLanguage::JavaScript
        );
    }

    #[test]
    fn session_flags_parse() {
        let args = parse_ok(&["--continue"]);
        assert!(args.continue_session);
        assert!(!args.no_session);

        let args = parse_ok(&["--session", "abc123"]);
        assert_eq!(args.session.as_deref(), Some("abc123"));

        let args = parse_ok(&["--no-session"]);
        assert!(args.no_session);
    }
}
