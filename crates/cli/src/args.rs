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

    /// 用纯文本输出事件，而不是启动 TUI。
    #[arg(long = "print")]
    pub print: bool,

    /// 只列出内置工具后退出。
    #[arg(long = "list-tools")]
    pub list_tools: bool,

    /// 使用离线 faux provider（默认也是它；与 `--provider` 同时给出时以 `--provider` 为准）。
    #[arg(long)]
    pub faux: bool,

    /// 选择 provider：faux / anthropic / openai / openai-responses / google。
    #[arg(
        long,
        value_name = "provider",
        value_parser = NonEmptyStringValueParser::new()
    )]
    pub provider: Option<String>,

    /// 模型 id（默认取 provider 的默认模型）。
    #[arg(
        long,
        value_name = "id",
        value_parser = NonEmptyStringValueParser::new()
    )]
    pub model: Option<String>,

    /// 打印帮助后退出。
    #[arg(short = 'h', long = "help", action = clap::ArgAction::Help)]
    pub help: Option<bool>,

    /// 打印版本后退出。
    #[arg(short = 'V', long = "version", action = clap::ArgAction::Version)]
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
    argv.extend(args.into_iter().map(Into::into));
    CliArgs::try_parse_from(argv)
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
        assert!(!args.faux);
        assert_eq!(args.provider, None);
        assert_eq!(args.model, None);
    }

    #[test]
    fn positional_words_join_into_one_prompt() {
        let args = parse_ok(&["看看", "这个", "仓库"]);
        assert_eq!(args.prompt_text().as_deref(), Some("看看 这个 仓库"));
    }

    #[test]
    fn flags_and_cwd_are_parsed() {
        let args = parse_ok(&["--print", "--faux", "-C", "/tmp/work", "hi"]);
        assert!(args.print);
        assert!(args.faux);
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

        let version = parse(["-V"]).expect_err("-V 由 clap 处理");
        assert_eq!(version.kind(), ErrorKind::DisplayVersion);
        assert!(version.to_string().starts_with("cli "), "got: {version}");
    }
}
