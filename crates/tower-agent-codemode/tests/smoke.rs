//! codemode 冒烟：真起两个内嵌组件（JS 沙箱 + coreutils），从宿主侧跑脚本。
//!
//! 两组测试：
//! - 直接驱动 [`CodemodeHost`]（走 `on_big_stack`：wasmtime 同步执行用调用方线程的栈）；
//! - 走 [`CodemodeTool`]（工具自带大栈 worker 线程，所以普通测试线程上跑就行——这正是工具层的价值）。

use std::{path::Path, pin::Pin, sync::Arc};

use futures_util::StreamExt;
use tower::ServiceExt;
use tower_agent_codemode::{
    COMMANDS, CodemodeError, CodemodeHost, CodemodeOptions, CodemodeTool, Mount, TOOL_NAME,
    tool_parameters,
};
use tower_agent_core::{AgentToolResult, ToolCallRequest};
use tower_agent_provider::{Arguments, ToolCall};

/// 在「栈够大」的线程上跑直接驱动 host 的测试体。
///
/// wasmtime 的同步执行用的是**调用方线程**的栈，`max_wasm_stack`（1 MiB）必须明显小于它；
/// 测试线程默认只有 2 MiB，深递归那类用例会先把原生栈压爆（实测 SIGABRT）。
fn on_big_stack<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(body)
        .expect("spawn")
        .join()
        .expect("join")
}

fn host_with(files: &[(&str, &str)]) -> (tempfile::TempDir, Arc<CodemodeHost>) {
    host_with_mounts(files, &[])
}

fn host_with_mounts(
    files: &[(&str, &str)],
    mounts: &[Mount],
) -> (tempfile::TempDir, Arc<CodemodeHost>) {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, contents) in files {
        let path = dir.path().join(name.trim_start_matches('/'));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(&path, contents).expect("write fixture");
    }
    let mut options = CodemodeOptions::embedded(dir.path());
    options.mounts = mounts.to_vec();
    let host = CodemodeHost::new(options).expect("host");
    (dir, host)
}

#[test]
fn runs_commands_and_reads_files() {
    on_big_stack(|| {
        let (_dir, host) = host_with(&[("nums.txt", "3\n1\n2\n")]);

        let out = host
            .run(
                r#"
                const sorted = await exec("sort", ["/nums.txt"]);
                console.log("sorted:", sorted.stdout.trim());
                fs.writeFile("/copy.txt", sorted.stdout);
                return { code: sorted.code, entries: fs.readdir("/").sort() };
                "#,
                None,
            )
            .expect("run");

        assert_eq!(out.stdout, "sorted: 1\n2\n3");
        assert_eq!(
            out.value.as_deref(),
            Some(r#"{"code":0,"entries":["copy.txt","nums.txt"]}"#)
        );
        // 命令流水（审计）：一条 sort，退出码 0。
        assert_eq!(out.commands.len(), 1);
        assert_eq!(out.commands[0].name, "sort");
        assert_eq!(out.commands[0].code, 0);
    });
}

#[test]
fn keeps_final_line_without_trailing_newline() {
    on_big_stack(|| {
        // conch 的回归用例：`cat` 一个结尾没有换行的文件，最后一个不完整行不能丢。
        let (_dir, host) = host_with(&[("nonl.txt", "apple\nbanana\nlast-no-newline")]);
        let out = host
            .run(
                r#"const r = await exec("cat", ["/nonl.txt"]); return r.stdout;"#,
                None,
            )
            .expect("run");
        assert_eq!(
            out.value.as_deref(),
            Some(r#""apple\nbanana\nlast-no-newline""#)
        );
    });
}

#[test]
fn mv_needs_a_coherent_root() {
    on_big_stack(|| {
        // uucore 的 canonicalize 会逐级 stat 路径前缀：会话根映射成 guest 的 `/` 时才不 ENOENT。
        let (_dir, host) = host_with(&[("a.txt", "x\n")]);
        let out = host
            .run(
                r#"
                const r = await exec("mv", ["/a.txt", "/b.txt"]);
                return { code: r.code, stderr: r.stderr, entries: fs.readdir("/").sort() };
                "#,
                None,
            )
            .expect("run");
        assert_eq!(
            out.value.as_deref(),
            Some(r#"{"code":0,"stderr":"","entries":["b.txt"]}"#)
        );
    });
}

#[test]
fn unknown_command_reports_127() {
    on_big_stack(|| {
        let (_dir, host) = host_with(&[]);
        let out = host
            .run(
                r#"const r = await exec("nosuchcmd", []); return { code: r.code, stderr: r.stderr };"#,
                None,
            )
            .expect("run");
        let value = out.value.unwrap_or_default();
        assert!(value.contains(r#""code":127"#), "got {value}");
        assert!(value.contains("unknown utility"), "got {value}");
    });
}

#[test]
fn script_errors_and_traps_surface_as_script_failures() {
    on_big_stack(|| {
        let (_dir, host) = host_with(&[]);

        // JS 异常：组件把它转成 Err(string)，宿主侧是 Script，并且带上真实的 message。
        let error = host
            .run(r#"throw new Error("boom");"#, None)
            .expect_err("should fail");
        assert!(matches!(error, CodemodeError::Script(_)), "got {error}");
        assert!(error.to_string().contains("boom"), "got {error}");

        // 深递归：在**组件里**跑时，QuickJS 的栈检查来不及在 wasm 影子栈耗尽前触发，
        // 所以这是硬 trap（独立跑同一段代码才是可捕获的 RangeError，见 docs §3.1）。
        // 关键是宿主把它降级成「脚本失败」，而不是崩掉或污染后续调用。
        let error = host
            .run(
                r#"(function f(n) { return n < 100_000 ? f(n + 1) : 0 })(0); return "no-error""#,
                None,
            )
            .expect_err("deep recursion should trap");
        assert!(matches!(error, CodemodeError::Script(_)), "got {error}");
        assert!(error.to_string().contains("stack"), "got {error}");

        // trap 之后同一个宿主照常工作（每次 run 一个新 Store）。
        let out = host.run(r#"return 1 + 1"#, None).expect("run after trap");
        assert_eq!(out.value.as_deref(), Some("2"));
    });
}

#[test]
fn session_root_is_the_only_visible_tree() {
    on_big_stack(|| {
        let (dir, host) = host_with(&[("inside.txt", "x\n")]);
        // guest 看到的 `/` 就是会话根；宿主真实路径对它不可见。
        let out = host
            .run(r#"return fs.readdir("/").sort()"#, None)
            .expect("run");
        assert_eq!(out.value.as_deref(), Some(r#"["inside.txt"]"#));
        assert_eq!(host.session_root(), dir.path());
        assert!(Path::new("/etc").exists()); // 宿主当然看得到，guest 看不到（下面断言）

        let out = host
            .run(
                r#"try { fs.readFile("/etc/hostname"); return "read-host-file" } catch (e) { return "denied" }"#,
                None,
            )
            .expect("run");
        assert_eq!(out.value.as_deref(), Some(r#""denied""#));
    });
}

/// 收集一次工具调用的结果流（中间项 + 终态）。
async fn collect_tool_call(
    tool: CodemodeTool,
    arguments: serde_json::Map<String, serde_json::Value>,
) -> Vec<AgentToolResult> {
    let call = ToolCall {
        id: "call-1".to_owned(),
        name: TOOL_NAME.to_owned(),
        arguments: Arguments::from(arguments),
        thought_signature: None,
        namespace: None,
    };
    let request = ToolCallRequest::new(call);
    let stream = tool.oneshot(request).await.expect("call");
    let stream: Pin<Box<dyn futures_util::Stream<Item = AgentToolResult> + Send>> =
        Box::pin(stream);
    stream.collect().await
}

#[test]
fn tool_streams_command_progress_then_final_result() {
    // 注意：这里**不**用 on_big_stack —— 工具自带大栈 worker 线程，普通线程上跑就行。
    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("nums.txt"), "3\n1\n2\n").expect("fixture");
    let tool = CodemodeTool::new(CodemodeOptions::embedded(dir.path())).expect("tool");

    let items = runtime.block_on(collect_tool_call(
        tool,
        serde_json::json!({
            "code": "const r = await exec(\"sort\", [\"/nums.txt\"]); return r.stdout.trim();",
        })
        .as_object()
        .expect("object")
        .clone(),
    ));

    // 至少两项：命令开始（进度）+ 终态结果。
    assert!(items.len() >= 2, "got {items:?}");
    let progress = &items[0];
    assert_eq!(
        progress
            .details
            .as_ref()
            .and_then(|d| d.get("kind"))
            .and_then(|v| v.as_str()),
        Some("command-start")
    );
    assert_eq!(
        progress
            .details
            .as_ref()
            .and_then(|d| d.get("name"))
            .and_then(|v| v.as_str()),
        Some("sort")
    );

    let final_result = items.last().expect("final");
    assert!(!final_result.is_error, "got {final_result:?}");
    let structured = final_result
        .structured_content
        .as_ref()
        .expect("structured");
    assert_eq!(structured.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        structured
            .get("commands")
            .and_then(|v| v.as_array())
            .map(Vec::len),
        Some(1)
    );
    // `value` 是脚本 `return` 值的 **JSON**，解析回来应等于那个字符串（真实换行）。
    let returned: serde_json::Value = serde_json::from_str(
        structured
            .get("value")
            .and_then(|v| v.as_str())
            .expect("value"),
    )
    .expect("value is json");
    assert_eq!(returned, serde_json::json!("1\n2\n3"));
}

#[test]
fn tool_rejects_bad_arguments_and_script_errors() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    let dir = tempfile::tempdir().expect("tempdir");
    let tool = CodemodeTool::new(CodemodeOptions::embedded(dir.path())).expect("tool");

    // 缺 `code`：直接给出错误结果，不进沙箱。
    let items = runtime.block_on(collect_tool_call(tool.clone(), serde_json::Map::new()));
    assert_eq!(items.len(), 1);
    assert!(items[0].is_error);

    // 脚本抛异常：终态是错误结果，但工具本身不失败（agent 能继续）。
    let items = runtime.block_on(collect_tool_call(
        tool,
        serde_json::json!({ "code": "throw new Error(\"boom\")" })
            .as_object()
            .expect("object")
            .clone(),
    ));
    let final_result = items.last().expect("final");
    assert!(final_result.is_error, "got {final_result:?}");
    assert!(
        format!("{:?}", final_result.details).contains("boom"),
        "got {:?}",
        final_result.details
    );
}

#[test]
fn tool_schema_is_complete() {
    let schema = tool_parameters();
    assert_eq!(schema.get("type").and_then(|v| v.as_str()), Some("object"));
    let required = schema
        .get("required")
        .and_then(|v| v.as_array())
        .expect("required");
    assert!(required.iter().any(|v| v.as_str() == Some("code")));
}

/// 从 `run` 的返回值（JSON 文本）解析出脚本 `return` 的内容。
fn returned(out: &tower_agent_codemode::RunResult) -> serde_json::Value {
    serde_json::from_str(out.value.as_deref().expect("value")).expect("json")
}

#[test]
fn command_functions_chain_into_a_pipeline() {
    on_big_stack(|| {
        let (_dir, host) = host_with(&[("nums.txt", "3\n1\n2\n")]);

        // `await cmd` 给 stdout；链式调用就是宿主侧顺序执行的 pipeline。
        let out = host
            .run(r#"return await cat("/nums.txt").sort()"#, None)
            .expect("run");
        assert_eq!(returned(&out), serde_json::json!("1\n2\n3\n"));

        let out = host
            .run(
                r#"return await cat("/nums.txt").sort().head("-n", "2")"#,
                None,
            )
            .expect("run");
        assert_eq!(returned(&out), serde_json::json!("1\n2\n"));

        // 三段 pipeline 的审计流水：cat → sort → head。
        assert_eq!(
            out.commands
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["cat", "sort", "head"]
        );

        // 逃生口 `exec` 照旧可用，能和链式混着写。
        let out = host
            .run(
                r#"const r = await exec("sort", [], { stdin: "2\n1\n" });
                   return { viaExec: r.code, viaChain: (await cat("/nums.txt").sort()).trim() };"#,
                None,
            )
            .expect("run");
        assert_eq!(
            returned(&out),
            serde_json::json!({ "viaExec": 0, "viaChain": "1\n2\n3" })
        );
    });
}

#[test]
fn failed_command_throws_unless_you_ask_for_the_result() {
    on_big_stack(|| {
        let (_dir, host) = host_with(&[]);

        // `await cmd` 在非零退出时抛 JS 异常（message 取自 stderr）。
        let out = host
            .run(
                r#"try { await cat("/missing.txt"); return "no-throw" }
                   catch (e) { return /missing\.txt/.test(String(e.message)) ? "threw-with-path" : ("other: " + e.message) }"#,
                None,
            )
            .expect("run");
        assert_eq!(returned(&out), serde_json::json!("threw-with-path"));

        // 想要退出码/原始 stderr 就用 `.result()`（不抛）。
        let out = host
            .run(
                r#"const r = await cat("/missing.txt").result();
                   return { code: r.code, hasStderr: r.stderr.includes("missing.txt") };"#,
                None,
            )
            .expect("run");
        let value = returned(&out);
        assert_ne!(value.get("code").and_then(|v| v.as_i64()), Some(0));
        assert_eq!(value.get("hasStderr").and_then(|v| v.as_bool()), Some(true));
    });
}

#[test]
fn fs_facade_covers_mkdir_exists_and_rm() {
    on_big_stack(|| {
        let (_dir, host) = host_with(&[]);
        let out = host
            .run(
                r#"fs.mkdir("/sub/deep");
                   fs.writeFile("/sub/deep/a.txt", "x");
                   const before = fs.exists("/sub/deep/a.txt");
                   fs.rm("/sub/deep");
                   return { before, after: fs.exists("/sub/deep"), root: fs.readdir("/") };"#,
                None,
            )
            .expect("run");
        assert_eq!(
            returned(&out),
            serde_json::json!({ "before": true, "after": false, "root": ["sub"] })
        );
    });
}

#[test]
fn command_list_matches_the_wrapper() {
    // JS 侧的函数是**宿主**按 `COMMANDS` 生成的，而真正的分发在 `wasm/coreutils-wasm`；
    // 两边漂移会让模型调到一个不存在的命令，所以这里直接对着源码比一次。
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../wasm/coreutils-wasm/src/main.rs"
    ))
    .expect("wrapper source");
    let mut from_wrapper = Vec::new();
    for line in source.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix('"') else {
            continue;
        };
        let Some((name, tail)) = rest.split_once("\" => ") else {
            continue;
        };
        if tail.starts_with("uu_") {
            from_wrapper.push(name.to_owned());
        }
    }
    assert_eq!(
        from_wrapper.len(),
        COMMANDS.len(),
        "宿主 COMMANDS 与 wrapper 登记表条数不一致"
    );
    for name in &from_wrapper {
        assert!(COMMANDS.contains(&name.as_str()), "宿主 COMMANDS 缺 {name}");
    }
}

#[test]
fn workspace_default_layout_reads_workspace_and_writes_only_in_session() {
    // 默认布局（`CodemodeTool::for_workspace`）：工作区**只读**挂到 `/work`，会话根可写，
    // 且会话根由工具持有——drop 即删（Q10「阶段一会话即焚」）。
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("seed.txt"), "seed\n").expect("fixture");

    let tool = CodemodeTool::for_workspace(workspace.path()).expect("tool");
    let session_root = tool.session_root().expect("session root").to_path_buf();
    assert!(session_root.exists());

    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    let items = runtime.block_on(collect_tool_call(
        tool.clone(),
        serde_json::json!({
            "code": r#"
                const seed = fs.readFile("/work/seed.txt");
                let writeWorkspace = "ok";
                try { fs.writeFile("/work/x.txt", "x") } catch (e) { writeWorkspace = "denied" }
                fs.writeFile("/out.txt", seed);
                return { seed, writeWorkspace, entries: fs.readdir("/") };
            "#,
        })
        .as_object()
        .expect("object")
        .clone(),
    ));

    let final_result = items.last().expect("final");
    assert!(!final_result.is_error, "got {final_result:?}");
    let value = final_result
        .structured_content
        .as_ref()
        .and_then(|v| v.get("value"))
        .and_then(|v| v.as_str())
        .expect("value");
    let returned: serde_json::Value = serde_json::from_str(value).expect("json");
    assert_eq!(
        returned.get("seed").and_then(|v| v.as_str()),
        Some("seed\n")
    );
    assert_eq!(
        returned.get("writeWorkspace").and_then(|v| v.as_str()),
        Some("denied")
    );

    // 工作区确实没被动过；会话根里有产物。
    assert!(!workspace.path().join("x.txt").exists());
    assert!(session_root.join("out.txt").exists());

    drop(tool);
    assert!(!session_root.exists(), "会话根应随工具销毁（会话即焚）");
}

#[test]
fn read_only_mount_serves_reads_and_blocks_writes() {
    on_big_stack(|| {
        let host_dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(host_dir.path().join("seed.txt"), "seed\n").expect("fixture");

        let (_session, host) = host_with_mounts(&[], &[Mount::read_only(host_dir.path(), "/work")]);

        // 只读挂载读得到……
        let out = host
            .run(r#"return fs.readFile("/work/seed.txt")"#, None)
            .expect("run");
        assert_eq!(returned(&out), serde_json::json!("seed\n"));

        // ……写不进去（fs 与命令两条路都挡住），宿主文件也没被创建。
        let out = host
            .run(
                r#"let viaFs = "ok";
                   try { fs.writeFile("/work/new.txt", "x") } catch (e) { viaFs = "denied" }
                   const r = await touch("/work/new2.txt").result();
                   return { viaFs, viaCommand: r.code !== 0 };"#,
                None,
            )
            .expect("run");
        assert_eq!(
            returned(&out),
            serde_json::json!({ "viaFs": "denied", "viaCommand": true })
        );
        assert!(!host_dir.path().join("new.txt").exists());
        assert!(!host_dir.path().join("new2.txt").exists());

        // 会话根照样可写。
        let out = host
            .run(
                r#"fs.writeFile("/ok.txt", "x"); return fs.exists("/ok.txt");"#,
                None,
            )
            .expect("run");
        assert_eq!(returned(&out), serde_json::json!(true));
    });
}

#[test]
fn paths_cannot_escape_the_visible_tree() {
    on_big_stack(|| {
        let (_dir, host) = host_with(&[]);
        let out = host
            .run(
                r#"let viaFs = "read";
                   try { fs.readFile("/../etc/hostname") } catch (e) { viaFs = "denied" }
                   const r = await cat("/../etc/hostname").result();
                   return { viaFs, viaCommand: r.code !== 0 };"#,
                None,
            )
            .expect("run");
        assert_eq!(
            returned(&out),
            serde_json::json!({ "viaFs": "denied", "viaCommand": true })
        );
    });
}

#[test]
fn command_timeout_cancels_host_blocked_waits() {
    on_big_stack(|| {
        let (_dir, host) = host_with(&[]);
        let started = std::time::Instant::now();
        let out = host
            .run(
                r#"const r = await exec("sleep", ["5"], { timeoutMs: 300 });
                   return { code: r.code, hasStderr: r.stderr.length > 0 };"#,
                None,
            )
            .expect("run");
        let value = returned(&out);
        assert_ne!(value.get("code").and_then(|v| v.as_i64()), Some(0));
        assert_eq!(value.get("hasStderr").and_then(|v| v.as_bool()), Some(true));
        // `sleep` 是把 guest 停在 `wasi:clocks` 的宿主调用里——epoch 打不断它（只打断正在
        // 执行的 wasm 指令），靠的是**异步链路 + `tokio::time::timeout`** 取消（docs §6）。
        // 所以这里断言的是「按时取消」：期限 300 ms，`sleep 5` 不该等满。
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "host 阻塞的等待应被取消，实际等了 {:?}",
            started.elapsed()
        );

        // 中断只影响那一次调用；宿主照常工作。
        let out = host
            .run(r#"return 1 + 1"#, None)
            .expect("run after timeout");
        assert_eq!(returned(&out), serde_json::json!(2));
    });
}
