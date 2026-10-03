//! 命令行参数解析（对应 pi `coding-agent` 的 CLI 入口参数）。
//!
//! 手写解析而不是引入 clap：选项很少，且解析逻辑本身需要被单元测试覆盖。

use std::path::PathBuf;

/// 解析后的命令行参数。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CliArgs {
    /// 一次性提示词（`cli "..."` 或 `-p "..."`）。
    pub prompt: Option<String>,
    /// 工具的工作目录（`-C` / `--cwd`）。
    pub cwd: Option<PathBuf>,
    /// 用纯文本输出事件，而不是启动 TUI（`-p` 的别名是 `--print`）。
    pub print: bool,
    /// 只列出内置工具后退出。
    pub list_tools: bool,
    /// 使用离线 faux provider（默认也是它，显式写出便于脚本自解释）。
    pub faux: bool,
    /// 打印帮助。
    pub help: bool,
    /// 打印版本。
    pub version: bool,
}

/// 参数错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgError {
    message: String,
}

impl ArgError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// 错误信息。
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for ArgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ArgError {}

/// 用法说明。
pub const USAGE: &str = "\
tower-pi-agent（cli）

用法:
  cli [选项] [提示词...]

选项:
  -h, --help          打印帮助
  -V, --version       打印版本
  -C, --cwd <目录>    工具的工作目录（默认：当前目录）
      --list-tools    列出内置工具后退出
      --print         用纯文本输出事件，不启动 TUI
      --faux          使用离线 faux provider（当前唯一可用的 provider）

说明:
  没有给出提示词时，会从 stdin 读取全部内容作为提示词。
  真实 provider（anthropic/openai/…）属于 `ai` crate 的 provider 层，尚未移植；
  `--faux` 是确定性的离线 provider，用于在没有模型的情况下跑通完整会话。";

/// 解析参数。
pub fn parse<I, S>(args: I) -> Result<CliArgs, ArgError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut parsed = CliArgs::default();
    let mut prompt_parts: Vec<String> = Vec::new();
    let mut iter = args.into_iter().map(Into::into).peekable();
    let mut only_positional = false;

    while let Some(arg) = iter.next() {
        if only_positional {
            prompt_parts.push(arg);
            continue;
        }
        match arg.as_str() {
            "--" => only_positional = true,
            "-h" | "--help" => parsed.help = true,
            "-V" | "--version" => parsed.version = true,
            "--list-tools" => parsed.list_tools = true,
            "--print" => parsed.print = true,
            "--faux" => parsed.faux = true,
            "-C" | "--cwd" => {
                let value = iter
                    .next()
                    .ok_or_else(|| ArgError::new(format!("{arg} 需要一个目录参数")))?;
                parsed.cwd = Some(PathBuf::from(value));
            }
            other if other.starts_with("--cwd=") => {
                let value = other.trim_start_matches("--cwd=");
                if value.is_empty() {
                    return Err(ArgError::new("--cwd 需要一个目录参数"));
                }
                parsed.cwd = Some(PathBuf::from(value));
            }
            other if other.starts_with('-') && other != "-" => {
                return Err(ArgError::new(format!("未知选项：{other}")));
            }
            other => prompt_parts.push(other.to_string()),
        }
    }

    if !prompt_parts.is_empty() {
        parsed.prompt = Some(prompt_parts.join(" "));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(args: &[&str]) -> CliArgs {
        parse(args.iter().copied()).expect("args parse")
    }

    #[test]
    fn no_arguments_yields_defaults() {
        let args = parse_ok(&[]);
        assert_eq!(args.prompt, None);
        assert_eq!(args.cwd, None);
        assert!(!args.print);
        assert!(!args.list_tools);
    }

    #[test]
    fn positional_words_join_into_one_prompt() {
        let args = parse_ok(&["看看", "这个", "仓库"]);
        assert_eq!(args.prompt.as_deref(), Some("看看 这个 仓库"));
    }

    #[test]
    fn flags_and_cwd_are_parsed() {
        let args = parse_ok(&["--print", "--faux", "-C", "/tmp/work", "hi"]);
        assert!(args.print);
        assert!(args.faux);
        assert_eq!(args.cwd, Some(PathBuf::from("/tmp/work")));
        assert_eq!(args.prompt.as_deref(), Some("hi"));

        let args = parse_ok(&["--cwd=/tmp/other"]);
        assert_eq!(args.cwd, Some(PathBuf::from("/tmp/other")));
    }

    #[test]
    fn double_dash_stops_option_parsing() {
        let args = parse_ok(&["--", "--print", "-x"]);
        assert!(!args.print);
        assert_eq!(args.prompt.as_deref(), Some("--print -x"));
    }

    #[test]
    fn unknown_options_are_rejected() {
        let error = parse(["--nope"]).expect_err("unknown option");
        assert_eq!(error.message(), "未知选项：--nope");
        let error = parse(["-C"]).expect_err("missing value");
        assert_eq!(error.message(), "-C 需要一个目录参数");
    }

    #[test]
    fn help_and_version_are_flags() {
        assert!(parse_ok(&["--help"]).help);
        assert!(parse_ok(&["-V"]).version);
    }
}
