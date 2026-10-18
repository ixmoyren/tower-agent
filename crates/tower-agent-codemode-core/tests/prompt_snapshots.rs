//! Reviewable snapshots for capability-derived codemode prompts.

use tower_agent_codemode_core::{
    Capability, PromptBuilder, PromptLimits, PromptMount, ScriptLanguage,
};

fn mounts() -> Vec<PromptMount> {
    vec![
        PromptMount::read_only("/work").with_description("workspace"),
        PromptMount::read_write("/output").with_description("session output"),
    ]
}

#[test]
fn javascript_full_prompt_matches_snapshot() {
    let document = PromptBuilder::new(
        ScriptLanguage::JavaScript,
        Capability::ALL,
        mounts(),
        PromptLimits::default(),
    )
    .build();
    assert_eq!(
        document.render(),
        include_str!("snapshots/javascript-full.md")
    );
}

#[test]
fn python_full_prompt_matches_snapshot() {
    let document = PromptBuilder::new(
        ScriptLanguage::Monty,
        Capability::ALL,
        mounts(),
        PromptLimits::default(),
    )
    .build();
    assert_eq!(document.render(), include_str!("snapshots/python-full.md"));
}

#[test]
fn disabling_capabilities_removes_their_prompt_sections() {
    let document = PromptBuilder::new(
        ScriptLanguage::JavaScript,
        [Capability::Fs],
        mounts(),
        PromptLimits {
            timeout_ms: 5_000,
            memory_bytes: 64 * 1024 * 1024,
        },
    )
    .build();
    assert_eq!(
        document.render(),
        include_str!("snapshots/javascript-fs-only.md")
    );
    assert!(!document.instructions.contains("tool_call"));
    assert!(!document.instructions.contains("state_get"));
    assert!(!document.instructions.contains("ui_status"));
}

#[test]
fn engines_share_capability_model_and_differ_only_in_examples() {
    let python = PromptBuilder::new(
        ScriptLanguage::Monty,
        Capability::ALL,
        mounts(),
        PromptLimits::default(),
    )
    .build();
    let javascript = PromptBuilder::new(
        ScriptLanguage::JavaScript,
        Capability::ALL,
        mounts(),
        PromptLimits::default(),
    )
    .build();

    for capability in Capability::ALL {
        let heading = format!("### {capability}");
        assert!(python.instructions.contains(&heading));
        assert!(javascript.instructions.contains(&heading));
    }
    assert!(python.instructions.contains("```python"));
    assert!(javascript.instructions.contains("```js"));
}
