//! 命名会话的列出 / 清理（M7）。不碰真实 config 目录，都在临时目录里做。

use tower_agent_codemode::{
    clear_session_in, list_sessions_in, session_name_is_valid, sessions_root_in,
};

#[test]
fn sessions_live_under_the_unified_codemode_root() {
    let config = std::path::Path::new("/config");
    assert_eq!(
        sessions_root_in(config),
        std::path::Path::new("/config/tower-agent/codemode/sessions")
    );
}

#[test]
fn lists_and_clears_named_sessions() {
    let root = tempfile::tempdir().expect("root");
    // 两个会话目录 + 一个不该被列出来的普通文件。
    std::fs::create_dir_all(root.path().join("alpha/nested")).expect("alpha");
    std::fs::write(root.path().join("alpha/out.txt"), "x").expect("artifact");
    std::fs::create_dir_all(root.path().join("beta")).expect("beta");
    std::fs::write(root.path().join("not-a-session.txt"), "x").expect("file");

    let sessions = list_sessions_in(root.path());
    assert_eq!(
        sessions
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "beta"]
    );

    assert!(clear_session_in(root.path(), "alpha").expect("clear"));
    assert!(!root.path().join("alpha").exists());
    assert!(
        !clear_session_in(root.path(), "alpha").expect("clear again"),
        "第二次应为「本来就不在」"
    );
    assert!(root.path().join("beta").exists(), "别动别的会话");
}

#[test]
fn session_name_cannot_escape_the_root() {
    let root = tempfile::tempdir().expect("root");
    for bad in ["", ".", "..", "../evil", "a/b", "a\\b"] {
        assert!(!session_name_is_valid(bad), "`{bad}` 应被判非法");
        let error = clear_session_in(root.path(), bad).expect_err("非法名字应报错");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
    assert!(session_name_is_valid("demo"));
    assert!(session_name_is_valid("demo-2026"));
}
