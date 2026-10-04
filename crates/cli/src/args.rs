//! 命令行参数解析（对应 pi `coding-agent` 的 CLI 入口参数）。
//!
//! 参数表用 `clap` 的 derive 定义：帮助、版本与参数错误都由 clap 统一生成，
//! 因此不再手写解析循环与用法字符串。`--provider` / `--model` 的取值语义留给
//! [`crate::provider`] 校验（这样"未知 provider / 缺凭据"的错误文案保持一致）。

use std::{ffi::OsString, path::PathBuf};

use clap::{Parser, builder::NonEmptyStringValueParser};

/// 解析后的命令行参数。
#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(
    name = "cli",
    version,
    about = "tower-pi-agent：pi（TypeScript agent 框架）的 Rust 移植版 CLI",
    disable_help_flag = true,
    disable_version_flag = true,
    help_template = "{about-with-newline}\n用法: {usage}\n\n参数:\n{positionals}\n\n选项:\n{options}"
)]
pub struct CliArgs {
    /// 一次性提示词；缺省时从 stdin 读取全部内容。
    #[arg(value_name = "提示词")]
    pub prompt: Vec<String>,

    /// 工具的工作目录（`-C` / `--cwd`）。
    #[arg(short = 'C', long = "cwd", value_name = "目录")]
    pub cwd: Option<PathBuf>,

    /// 非交互模式：处理完提示词后退出（对应 pi `-p` / `--print`）。
    #[arg(short = 'p', long = "print")]
    pub print: bool,

    /// 只列出内置工具后退出。
    #[arg(long = "list-tools")]
    pub list_tools: bool,

    /// 选择 provider：anthropic / openai / openai-responses / google（缺省时按环境里的凭据挑）。
    #[arg(
        long,
        value_name = "provider",
        value_parser = NonEmptyStringValueParser::new()
    )]
    pub provider: Option<String>,

    /// 一次性 API key（覆盖环境变量；对应 pi `--api-key`）。
    #[arg(long = "api-key", value_name = "key")]
    pub api_key: Option<String>,

    /// 模型 id（默认取 provider 的默认模型）。
    #[arg(
        long,
        value_name = "id",
        value_parser = NonEmptyStringValueParser::new()
    )]
    pub model: Option<String>,

    /// 思考级别：off / minimal / low / medium / high / xhigh / max（对应 pi `--thinking`）。
    #[arg(long, value_name = "level")]
    pub thinking: Option<String>,

    /// 只启用列出的内置工具，逗号分隔（对应 pi `-t` / `--tools`）。
    #[arg(
        short = 't',
        long = "tools",
        value_name = "tools",
        value_delimiter = ',',
        value_parser = trim_value
    )]
    pub tools: Vec<String>,

    /// 排除列出的内置工具，逗号分隔（对应 pi `-xt` / `--exclude-tools`）。
    #[arg(
        long = "exclude-tools",
        value_name = "tools",
        value_delimiter = ',',
        value_parser = trim_value
    )]
    pub exclude_tools: Vec<String>,

    /// 禁用全部内置工具（对应 pi `-nt` / `--no-tools`）。
    #[arg(long = "no-tools")]
    pub no_tools: bool,

    /// 禁用内置工具（本移植版没有扩展工具，等同 `--no-tools`；对应 pi `-nbt`）。
    #[arg(long = "no-builtin-tools")]
    pub no_builtin_tools: bool,

    /// 打印帮助后退出。
    #[arg(short = 'h', long = "help", action = clap::ArgAction::Help)]
    pub help: Option<bool>,

    /// 打印版本后退出。
    #[arg(short = 'v', long = "version", action = clap::ArgAction::Version)]
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
    let mut argv: Vec<OsString> = vec![OsString::from("cli")];
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
        assert!(help.to_string().contains("用法"), "got: {help}");

        // pi 的版本短选项是 `-v`（clap 默认的 `-V` 已关闭）。
        let version = parse(["-v"]).expect_err("-v 由 clap 处理");
        assert_eq!(version.kind(), ErrorKind::DisplayVersion);
        assert!(version.to_string().starts_with("cli "), "got: {version}");
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

        let args = parse_ok(&["-xt", "bash,write"]);
        assert_eq!(args.exclude_tools, vec!["bash", "write"]);

        let args = parse_ok(&["-nt"]);
        assert!(args.no_tools);

        let args = parse_ok(&["-nbt"]);
        assert!(args.no_builtin_tools);
    }
}
