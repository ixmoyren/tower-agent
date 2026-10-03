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
    assert!(stdout(&output).contains("用法"));

    let output = cli().arg("--version").output().expect("run cli");
    assert!(output.status.success());
    assert!(stdout(&output).starts_with("cli "));
}

#[test]
fn list_tools_prints_every_builtin_tool() {
    let output = cli().arg("--list-tools").output().expect("run cli");
    assert!(output.status.success());
    let text = stdout(&output);
    for name in [
        "read",
        "bash",
        "powershell",
        "edit",
        "write",
        "grep",
        "find",
        "ls",
    ] {
        assert!(text.contains(name), "missing `{name}` in:\n{text}");
    }
    assert!(text.contains("内置工具（8 个）"), "got:\n{text}");
}

#[test]
fn print_mode_runs_a_full_turn_with_a_real_tool() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("note.txt"), "来自文件的内容\n").unwrap();

    let output = cli()
        .args([
            "--print",
            "--faux",
            "-C",
            dir.path().to_str().expect("utf-8 path"),
            "!read {\"path\":\"note.txt\"}",
        ])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("[agent] start"), "got:\n{text}");
    assert!(text.contains("[user] !read"), "got:\n{text}");
    assert!(text.contains("[tool] read"), "got:\n{text}");
    assert!(text.contains("[tool:ok] read"), "got:\n{text}");
    assert!(text.contains("来自文件的内容"), "got:\n{text}");
    assert!(text.contains("[assistant]"), "got:\n{text}");
    assert!(text.contains("[agent] end"), "got:\n{text}");
}

#[test]
fn print_mode_reads_the_prompt_from_stdin_when_no_positional_argument_is_given() {
    use std::{io::Write as _, process::Stdio};

    let mut child = cli()
        .args(["--print"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cli");
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all("来自标准输入".as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait cli");

    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("[user] 来自标准输入"), "got:\n{text}");
    assert!(text.contains("[faux] 收到：来自标准输入"), "got:\n{text}");
}

#[test]
fn unknown_option_fails_with_usage() {
    let output = cli().arg("--nope").output().expect("run cli");
    assert!(!output.status.success());
    assert!(stderr(&output).contains("未知选项：--nope"));
    assert!(stderr(&output).contains("用法"));
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
