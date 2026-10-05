//! 真正运行 `cli` 二进制的端到端测试（`--print` 模式不需要 TTY）。

use std::process::Command;

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cli"))
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn help_and_version_exit_successfully() {
    let output = cli().arg("--help").output().expect("run cli");
    assert!(output.status.success());
    assert!(stdout(&output).contains("Usage"));

    let output = cli().arg("--version").output().expect("run cli");
    assert!(output.status.success());
    assert!(stdout(&output).starts_with("cli "));

    // pi 的版本短选项是 `-v`。
    let output = cli().arg("-v").output().expect("run cli");
    assert!(output.status.success());
    assert!(stdout(&output).starts_with("cli "));
}

#[test]
fn list_tools_prints_every_builtin_tool() {
    let output = cli().arg("--list-tools").output().expect("run cli");
    assert!(output.status.success());
    let text = stdout(&output);
    for name in ["read", "shell", "edit", "write", "grep", "find", "ls"] {
        assert!(text.contains(name), "missing `{name}` in:\n{text}");
    }
    assert!(text.contains("内置工具（7 个）"), "got:\n{text}");
}

#[test]
fn unknown_option_is_rejected_by_clap() {
    let output = cli().arg("--nope").output().expect("run cli");
    assert!(!output.status.success());
    let text = stderr(&output);
    assert!(text.contains("--nope"), "got:\n{text}");
    assert!(text.contains("Usage"), "got:\n{text}");
}

#[test]
fn unknown_provider_fails_with_the_available_list() {
    let output = cli()
        .args(["--print", "--provider", "mistral", "hi"])
        .output()
        .expect("run cli");
    assert!(!output.status.success());
    let text = stderr(&output);
    assert!(text.contains("未知 provider：mistral"), "got:\n{text}");
    assert!(text.contains("anthropic"), "got:\n{text}");
}

#[test]
fn real_provider_without_api_key_fails_clearly() {
    let output = cli()
        .env_remove("ANTHROPIC_API_KEY")
        .args(["--print", "--provider", "anthropic", "hi"])
        .output()
        .expect("run cli");
    assert!(!output.status.success());
    let text = stderr(&output);
    assert!(text.contains("ANTHROPIC_API_KEY"), "got:\n{text}");
}

#[test]
fn missing_prompt_fails_with_usage() {
    // stdin 是空管道时没有提示词可用。
    use std::process::Stdio;
    let output = cli()
        .arg("--print")
        .stdin(Stdio::null())
        .output()
        .expect("run cli");
    assert!(!output.status.success());
    assert!(stderr(&output).contains("没有提示词"));
}

#[test]
fn faux_is_not_a_selectable_provider() {
    // faux 是测试替身，不编进二进制：既没有 `--faux`，`--provider faux` 也不认。
    let output = cli().args(["--faux", "hi"]).output().expect("run cli");
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("--faux"),
        "got:\n{}",
        stderr(&output)
    );

    let output = cli()
        .args(["--print", "--provider", "faux", "hi"])
        .output()
        .expect("run cli");
    assert!(!output.status.success());
    let text = stderr(&output);
    assert!(text.contains("未知 provider：faux"), "got:\n{text}");
    assert!(!text.contains("faux /"), "可用列表里不应有 faux：\n{text}");
}

#[test]
fn default_provider_comes_from_the_environment() {
    // 所有 provider 的环境变量都没有时给出明确提示，而不是默默退回 faux。
    let output = cli()
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("GEMINI_API_KEY")
        .env_remove("DEEPSEEK_API_KEY")
        .args(["--print", "hi"])
        .output()
        .expect("run cli");
    assert!(!output.status.success());
    let text = stderr(&output);
    assert!(text.contains("没有可用的 provider"), "got:\n{text}");
    assert!(text.contains("ANTHROPIC_API_KEY"), "got:\n{text}");
}

#[test]
fn api_key_requires_an_explicit_provider() {
    let output = cli()
        .args(["--print", "--api-key", "sk-test", "hi"])
        .output()
        .expect("run cli");
    assert!(!output.status.success());
    let text = stderr(&output);
    assert!(text.contains("--api-key"), "got:\n{text}");
    assert!(text.contains("--provider"), "got:\n{text}");
}
