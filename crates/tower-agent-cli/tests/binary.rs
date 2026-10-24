//! 真正运行 `cli` 二进制的端到端测试（`--print` 模式不需要 TTY）。

use std::{
    path::Path,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

/// 每次调用分配一个独立的存储库路径，避免二进制测试读到 / 写到用户真实的
/// `~/.local/share/tower-agent/storage.db`。
static STORAGE_SEQ: AtomicUsize = AtomicUsize::new(0);

fn cli() -> Command {
    let seq = STORAGE_SEQ.fetch_add(1, Ordering::Relaxed);
    let storage = std::env::temp_dir().join(format!(
        "tower-agent-cli-test-{}-{seq}.db",
        std::process::id()
    ));
    let mut command = Command::new(env!("CARGO_BIN_EXE_tower-agent"));
    command.env("TOWER_AGENT_STORAGE", storage);
    command
}

/// 指定存储库与工作目录的二进制（多次调用共享同一份配置）。
fn cli_at(storage: &str, cwd: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tower-agent"));
    command.env("TOWER_AGENT_STORAGE", storage);
    command.arg("-C").arg(cwd);
    command
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
    let help = stdout(&output);
    assert!(help.contains("Usage"));
    for flag in [
        "--codemode",
        "--no-codemode",
        "--codemode-engine",
        "--codemode-session",
        "--codemode-session-clear",
        "--codemode-export",
        "--codemode-import",
        "--codemode-script-scope",
    ] {
        assert!(help.contains(flag), "help is missing `{flag}`:\n{help}");
    }
    assert!(
        !help.contains("mods"),
        "legacy mods wording remains:\n{help}"
    );

    let output = cli().arg("--version").output().expect("run cli");
    assert!(output.status.success());
    assert!(stdout(&output).starts_with("tower-agent "));

    // pi 的版本短选项是 `-v`。
    let output = cli().arg("-v").output().expect("run cli");
    assert!(output.status.success());
    assert!(stdout(&output).starts_with("tower-agent "));
}

#[tokio::test]
async fn codemode_export_import_commands_round_trip_through_the_database() {
    use tower_agent_storage::{
        ConfigScope, ScriptLanguage, ScriptPermissions, ScriptRecord, ScriptStore as _,
    };
    use tower_agent_turso::TursoStorage;

    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");
    let export = tempfile::tempdir().expect("export");
    let storage_path = home.path().join("scripts.db");
    let storage_arg = storage_path.to_string_lossy().into_owned();
    let project = ConfigScope::from_working_dir(work.path());
    let source = "tool_register(\"round_trip\", \"round_trip\", \"\", {\"type\": \"object\"})\n\
                  def round_trip(args):\n    return \"ok\"\n";

    let storage = TursoStorage::open(&storage_path)
        .await
        .expect("open storage");
    storage
        .upsert_script(ScriptRecord {
            id: "round-trip".to_owned(),
            name: "round_trip".to_owned(),
            language: ScriptLanguage::Monty,
            scope: project.clone(),
            source: source.to_owned(),
            enabled: true,
            permissions: ScriptPermissions::default(),
            metadata: serde_json::json!({ "requires": [] }),
            created_at: 1,
            updated_at: 2,
            source_hash: "export-recomputes-this".to_owned(),
        })
        .await
        .expect("write script");
    storage.flush().await.expect("flush");
    drop(storage);
    // Turso's background writer owns a short-lived connection; let it observe the
    // closed queue before the CLI opens the same database.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let output = cli_at(&storage_arg, work.path())
        .arg("--codemode-export")
        .arg(export.path())
        .output()
        .expect("run export");
    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    assert!(
        std::fs::read_dir(export.path())
            .expect("export dir")
            .filter_map(Result::ok)
            .any(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("codemode"))
    );

    std::fs::remove_file(&storage_path).expect("delete database");

    let output = cli_at(&storage_arg, work.path())
        .arg("--codemode-import")
        .arg(export.path())
        .args(["--codemode-script-scope", "project"])
        .output()
        .expect("run import");
    assert!(output.status.success(), "stderr:\n{}", stderr(&output));

    let restored = TursoStorage::open(&storage_path)
        .await
        .expect("reopen storage");
    let record = restored
        .get_script(&project, "round_trip")
        .await
        .expect("read script")
        .expect("restored script");
    assert_eq!(record.source, source);
    assert_eq!(record.language, ScriptLanguage::Monty);
    assert!(record.enabled);
}

#[test]
fn obsolete_mods_flag_is_rejected() {
    let output = cli().arg("--no-mods").output().expect("run cli");
    assert!(!output.status.success());
    let text = stderr(&output);
    assert!(text.contains("--no-mods"), "got:\n{text}");

    let output = cli()
        .args(["--codemode", "--no-codemode"])
        .output()
        .expect("run cli");
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("cannot be used with"),
        "got:\n{}",
        stderr(&output)
    );
}

#[test]
fn list_tools_prints_every_builtin_tool() {
    let output = cli().arg("--list-tools").output().expect("run cli");
    assert!(output.status.success());
    let text = stdout(&output);
    for name in ["read", "edit", "write", "grep", "find", "ls"] {
        assert!(text.contains(name), "missing `{name}` in:\n{text}");
    }
    assert!(
        !text
            .lines()
            .any(|line| line.trim_start().starts_with("shell ")),
        "shell is dynamically loaded:\n{text}"
    );
    assert!(text.contains("内置工具（6 个）"), "got:\n{text}");
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
fn configured_sidecar_provider_runs_without_recompiling() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");
    let providers = home.path().join("providers.json");
    std::fs::write(
        &providers,
        serde_json::json!({
            "providers": {
                "sidecar": {
                    "kind": "sidecar",
                    "name": "Sidecar",
                    "default_model": "sidecar-model",
                    "command": [
                        "sh",
                        "-c",
                        "cat >/dev/null; printf '%s\\n' '{\"type\":\"done\",\"text\":\"sidecar reply\"}'"
                    ]
                }
            }
        })
        .to_string(),
    )
    .expect("write provider config");
    let storage = home.path().join("storage.db");
    let output = cli_at(&storage.to_string_lossy(), work.path())
        .env("TOWER_AGENT_PROVIDERS", &providers)
        .args([
            "--print",
            "--no-session",
            "--no-codemode",
            "--provider",
            "sidecar",
            "hello",
        ])
        .output()
        .expect("run sidecar provider");
    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    assert!(
        stdout(&output).contains("sidecar reply"),
        "stdout:\n{}",
        stdout(&output)
    );
}

#[test]
fn configured_dynamic_provider_runs_without_recompiling() {
    let Some(library) = std::env::var_os("TPA_PROVIDER_DYNAMIC_EXAMPLE") else {
        eprintln!("skipping: set TPA_PROVIDER_DYNAMIC_EXAMPLE to the provider cdylib path");
        return;
    };
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");
    let providers = home.path().join("providers.json");
    std::fs::write(
        &providers,
        serde_json::json!({
            "providers": {
                "dynamic": {
                    "kind": "dynamic",
                    "library": library.to_string_lossy(),
                    "default_model": "dynamic-model"
                }
            }
        })
        .to_string(),
    )
    .expect("write provider config");
    let storage = home.path().join("storage.db");
    let output = cli_at(&storage.to_string_lossy(), work.path())
        .env("TOWER_AGENT_PROVIDERS", &providers)
        .args([
            "--print",
            "--no-session",
            "--no-codemode",
            "--provider",
            "dynamic",
            "hello",
        ])
        .output()
        .expect("run dynamic provider");
    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    assert!(
        stdout(&output).contains("dynamic reply"),
        "stdout:\n{}",
        stdout(&output)
    );
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

#[test]
fn config_commands_round_trip_through_storage() {
    let home = tempfile::tempdir().expect("temp dir");
    let work = tempfile::tempdir().expect("temp dir");
    let other = tempfile::tempdir().expect("temp dir");
    let storage = home.path().join("config.db").to_string_lossy().into_owned();

    // 空库：读一个没设置的 key 是非零退出。
    let output = cli_at(&storage, work.path())
        .args(["--config-get", "defaultModel"])
        .output()
        .expect("run cli");
    assert!(!output.status.success());

    // user 作用域写入 + 读回（字符串值不加引号）。
    let output = cli_at(&storage, work.path())
        .args(["--config-set", "defaultModel=gpt-5.5"])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "stderr:\n{}", stderr(&output));

    let output = cli_at(&storage, work.path())
        .args(["--config-get", "defaultModel"])
        .output()
        .expect("run cli");
    assert!(output.status.success());
    assert_eq!(stdout(&output).trim(), "gpt-5.5");

    // JSON 值原样解析、紧凑回显。
    let output = cli_at(&storage, work.path())
        .args(["--config-set", "retry={\"max\":2}"])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let output = cli_at(&storage, work.path())
        .args(["--config-get", "retry"])
        .output()
        .expect("run cli");
    assert_eq!(stdout(&output).trim(), "{\"max\":2}");

    // 列表是「用户级打底」的合并视图。
    let output = cli_at(&storage, work.path())
        .arg("--config-list")
        .output()
        .expect("run cli");
    let text = stdout(&output);
    assert!(text.contains("defaultModel = gpt-5.5"), "got:\n{text}");
    assert!(text.contains("retry = {\"max\":2}"), "got:\n{text}");

    // 项目作用域覆盖用户作用域，且只影响当前项目。
    let output = cli_at(&storage, work.path())
        .args([
            "--config-scope",
            "project",
            "--config-set",
            "defaultModel=local-model",
        ])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "stderr:\n{}", stderr(&output));

    let output = cli_at(&storage, work.path())
        .args(["--config-get", "defaultModel"])
        .output()
        .expect("run cli");
    assert_eq!(stdout(&output).trim(), "local-model");

    let output = cli_at(&storage, other.path())
        .args(["--config-get", "defaultModel"])
        .output()
        .expect("run cli");
    assert_eq!(stdout(&output).trim(), "gpt-5.5");

    // 指定作用域的原始列表能看到项目级那一条。
    let output = cli_at(&storage, work.path())
        .args(["--config-scope", "project", "--config-list"])
        .output()
        .expect("run cli");
    assert_eq!(stdout(&output).trim(), "defaultModel = local-model");

    // 删掉项目级覆盖后，有效值退回用户级。
    let output = cli_at(&storage, work.path())
        .args([
            "--config-scope",
            "project",
            "--config-unset",
            "defaultModel",
        ])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let output = cli_at(&storage, work.path())
        .args(["--config-get", "defaultModel"])
        .output()
        .expect("run cli");
    assert_eq!(stdout(&output).trim(), "gpt-5.5");
}

#[cfg(feature = "codemode-js")]
#[test]
fn codemode_sessions_use_the_unified_directory() {
    let config = tempfile::tempdir().expect("config");
    let sessions = config.path().join("tower-agent/codemode/sessions");
    std::fs::create_dir_all(sessions.join("demo")).expect("create session");
    std::fs::write(sessions.join("demo/result.txt"), "ok").expect("write artifact");
    std::fs::create_dir_all(config.path().join("tower-agent/codemode/legacy")).expect("legacy dir");

    let output = cli()
        .env("XDG_CONFIG_HOME", config.path())
        .arg("--codemode-sessions")
        .output()
        .expect("run cli");
    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("demo"), "got:\n{text}");
    assert!(!text.contains("legacy"), "legacy directory leaked:\n{text}");

    let output = cli()
        .env("XDG_CONFIG_HOME", config.path())
        .args(["--codemode-session-clear", "demo"])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    assert!(!sessions.join("demo").exists());
}
