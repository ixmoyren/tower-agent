//! Capability-derived prompts shared by codemode engines.

use std::{collections::BTreeSet, fmt::Write as _};

use super::{Capability, CapabilitySurface, ScriptLanguage};

/// How a path is exposed to a codemode script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMountMode {
    /// Reads are allowed, writes are rejected.
    ReadOnly,
    /// Reads and writes are allowed.
    ReadWrite,
    /// Writes stay in memory and do not reach the host path.
    OverlayMemory,
}

impl PromptMountMode {
    /// Human-readable description used in generated prompts.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "只读",
            Self::ReadWrite => "可读写",
            Self::OverlayMemory => "内存覆盖写入",
        }
    }
}

/// One guest path visible to a codemode script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptMount {
    /// Path as seen by the script.
    pub guest_path: String,
    /// Access mode for the mount.
    pub mode: PromptMountMode,
    /// Optional human-readable purpose, such as "workspace" or "session output".
    pub description: Option<String>,
}

impl PromptMount {
    /// Construct a read-only guest mount.
    #[must_use]
    pub fn read_only(guest_path: impl Into<String>) -> Self {
        Self {
            guest_path: guest_path.into(),
            mode: PromptMountMode::ReadOnly,
            description: None,
        }
    }

    /// Construct a read-write guest mount.
    #[must_use]
    pub fn read_write(guest_path: impl Into<String>) -> Self {
        Self {
            guest_path: guest_path.into(),
            mode: PromptMountMode::ReadWrite,
            description: None,
        }
    }

    /// Attach a human-readable purpose.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// Resource limits visible to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptLimits {
    /// Total wall-clock budget for one script run.
    pub timeout_ms: u64,
    /// Sandbox memory budget in bytes.
    pub memory_bytes: u64,
}

impl Default for PromptLimits {
    fn default() -> Self {
        Self {
            timeout_ms: 30_000,
            memory_bytes: 128 * 1024 * 1024,
        }
    }
}

/// The two model-visible prompt fields for a codemode tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptDocument {
    /// Short tool description.
    pub tool_description: String,
    /// Full instructions.
    pub instructions: String,
}

impl PromptDocument {
    /// Render both fields as one reviewable Markdown document.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "# Tool description\n\n{}\n\n# Instructions\n\n{}",
            self.tool_description, self.instructions
        )
    }
}

/// Builds codemode prompts from the enabled capability set.
#[derive(Debug, Clone)]
pub struct PromptBuilder {
    language: ScriptLanguage,
    capabilities: BTreeSet<Capability>,
    mounts: Vec<PromptMount>,
    limits: PromptLimits,
}

impl PromptBuilder {
    /// Construct a builder from an explicit capability collection.
    #[must_use]
    pub fn new(
        language: ScriptLanguage,
        capabilities: impl IntoIterator<Item = Capability>,
        mounts: Vec<PromptMount>,
        limits: PromptLimits,
    ) -> Self {
        Self {
            language,
            capabilities: capabilities.into_iter().collect(),
            mounts,
            limits,
        }
    }

    /// Construct a builder from a capability surface's enabled capabilities.
    #[must_use]
    pub fn from_surface(
        surface: &dyn CapabilitySurface,
        language: ScriptLanguage,
        mounts: Vec<PromptMount>,
        limits: PromptLimits,
    ) -> Self {
        Self::new(language, surface.capabilities(), mounts, limits)
    }

    /// Enabled capabilities in stable order.
    #[must_use]
    pub fn capabilities(&self) -> Vec<Capability> {
        self.capabilities.iter().copied().collect()
    }

    /// Build the model-visible tool description and instructions.
    #[must_use]
    pub fn build(&self) -> PromptDocument {
        PromptDocument {
            tool_description: self.tool_description(),
            instructions: self.instructions(),
        }
    }

    fn tool_description(&self) -> String {
        let language = self.language_label();
        if self.capabilities.is_empty() {
            return format!(
                "在 wasm 沙箱中执行{language}codemode 脚本。当前未授予任何能力；请直接说明无法完成任务。"
            );
        }
        let capabilities = self
            .capabilities
            .iter()
            .map(|capability| capability.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "在 wasm 沙箱中执行{language}codemode 脚本，用于组合已授权的命令、文件、宿主工具和扩展能力。\
             已授权能力：{capabilities}。只能使用提示中明确列出的接口。"
        )
    }

    fn instructions(&self) -> String {
        let mut output = String::new();
        writeln!(output, "# Codemode").expect("writing to String cannot fail");
        writeln!(output).expect("writing to String cannot fail");
        writeln!(
            output,
            "你在 wasm 沙箱中执行{}脚本。脚本通过宿主能力与外部世界交互，不能直接访问网络。",
            self.language_label()
        )
        .expect("writing to String cannot fail");
        writeln!(output).expect("writing to String cannot fail");
        writeln!(output, "## 限制").expect("writing to String cannot fail");
        writeln!(output, "- 单次执行时间上限：{} ms", self.limits.timeout_ms)
            .expect("writing to String cannot fail");
        writeln!(
            output,
            "- 沙箱内存上限：{}",
            format_bytes(self.limits.memory_bytes)
        )
        .expect("writing to String cannot fail");
        writeln!(output, "- 网络：不可用").expect("writing to String cannot fail");
        writeln!(output).expect("writing to String cannot fail");

        self.write_mounts(&mut output);
        self.write_capabilities(&mut output);
        self.write_examples(&mut output);
        output
    }

    fn write_mounts(&self, output: &mut String) {
        writeln!(output, "## 文件系统挂载").expect("writing to String cannot fail");
        if self.mounts.is_empty() {
            writeln!(output, "没有挂载目录。").expect("writing to String cannot fail");
        } else {
            if !self.capabilities.contains(&Capability::Fs) {
                writeln!(output, "当前 `fs` 权限关闭；下列挂载不可由脚本直接访问。")
                    .expect("writing to String cannot fail");
            }
            for mount in &self.mounts {
                write!(output, "- `{}`：{}", mount.guest_path, mount.mode.as_str())
                    .expect("writing to String cannot fail");
                if let Some(description) = &mount.description {
                    write!(output, "（{description}）").expect("writing to String cannot fail");
                }
                writeln!(output).expect("writing to String cannot fail");
            }
        }
        writeln!(output).expect("writing to String cannot fail");
    }

    fn write_capabilities(&self, output: &mut String) {
        writeln!(output, "## 可用能力").expect("writing to String cannot fail");
        writeln!(
            output,
            "未列出的 host function 不可调用；不要假设额外能力存在。"
        )
        .expect("writing to String cannot fail");
        if self.capabilities.is_empty() {
            writeln!(output, "\n无。").expect("writing to String cannot fail");
            return;
        }
        for capability in &self.capabilities {
            writeln!(output).expect("writing to String cannot fail");
            match capability {
                Capability::Exec => {
                    writeln!(output, "### exec").expect("writing to String cannot fail");
                    writeln!(
                        output,
                        "- `exec(name, args, opts)` 运行 coreutils-wasm 命令；返回 `code`、`stdout`、`stderr`。"
                    )
                    .expect("writing to String cannot fail");
                }
                Capability::Fs => {
                    writeln!(output, "### fs").expect("writing to String cannot fail");
                    writeln!(
                        output,
                        "- 文件操作只能访问上表挂载；写入范围由挂载模式决定。"
                    )
                    .expect("writing to String cannot fail");
                }
                Capability::Tools => {
                    writeln!(output, "### tools").expect("writing to String cannot fail");
                    writeln!(
                        output,
                        "- `tool_call(name, args)` 调用宿主工具；`tool_list()` 列出可用工具及其元数据。"
                    )
                    .expect("writing to String cannot fail");
                }
                Capability::RegisterTools => {
                    writeln!(output, "### register_tools").expect("writing to String cannot fail");
                    writeln!(
                        output,
                        "- `tool_register(...)` 注册脚本工具；`tool_unregister(name)` 只注销脚本自己注册的工具。"
                    )
                    .expect("writing to String cannot fail");
                }
                Capability::Hooks => {
                    writeln!(output, "### hooks").expect("writing to String cannot fail");
                    writeln!(
                        output,
                        "- `on(\"tool_call\", handler)` 和 `on(\"tool_result\", handler)` 可观察或调整工具调用。"
                    )
                    .expect("writing to String cannot fail");
                }
                Capability::Commands => {
                    writeln!(output, "### commands").expect("writing to String cannot fail");
                    writeln!(
                        output,
                        "- `register_command(name, handler)` 注册 agent 命令。"
                    )
                    .expect("writing to String cannot fail");
                }
                Capability::Ui => {
                    writeln!(output, "### ui").expect("writing to String cannot fail");
                    writeln!(
                        output,
                        "- `ui_status`、`ui_panel`、`ui_clear`、`ui_tool_render` 贡献前端无关的 UI。"
                    )
                    .expect("writing to String cannot fail");
                }
                Capability::State => {
                    writeln!(output, "### state").expect("writing to String cannot fail");
                    writeln!(
                        output,
                        "- `state_get(key)` / `state_set(key, value)` 读写脚本私有状态；`log(level, message)` 写日志。"
                    )
                    .expect("writing to String cannot fail");
                }
            }
        }
    }

    fn write_examples(&self, output: &mut String) {
        writeln!(output).expect("writing to String cannot fail");
        writeln!(output, "## 语法示例").expect("writing to String cannot fail");
        for capability in &self.capabilities {
            writeln!(output).expect("writing to String cannot fail");
            writeln!(output, "### {capability}").expect("writing to String cannot fail");
            match (self.language, capability) {
                (ScriptLanguage::JavaScript, Capability::Exec) => {
                    writeln!(
                        output,
                        "```js\nconst result = await exec(\"wc\", [\"-l\", \"/work/input.txt\"]);\nreturn result.stdout;\n```"
                    )
                    .expect("writing to String cannot fail");
                }
                (ScriptLanguage::Monty, Capability::Exec) => {
                    writeln!(
                        output,
                        "```python\nresult = exec(\"wc\", [\"-l\", \"/work/input.txt\"])\nreturn result[\"stdout\"]\n```"
                    )
                    .expect("writing to String cannot fail");
                }
                (ScriptLanguage::JavaScript, Capability::Fs) => {
                    writeln!(
                        output,
                        "```js\nconst text = fs.readFile(\"/work/input.txt\");\n```"
                    )
                    .expect("writing to String cannot fail");
                }
                (ScriptLanguage::Monty, Capability::Fs) => {
                    writeln!(
                        output,
                        "```python\nfrom pathlib import Path\ntext = Path(\"/work/input.txt\").read_text()\n```"
                    )
                    .expect("writing to String cannot fail");
                }
                (ScriptLanguage::JavaScript, Capability::Tools) => {
                    writeln!(
                        output,
                        "```js\nconst text = await tool_call(\"read\", {{ path: \"/work/input.txt\" }});\n```"
                    )
                    .expect("writing to String cannot fail");
                }
                (ScriptLanguage::Monty, Capability::Tools) => {
                    writeln!(
                        output,
                        "```python\ntext = tool_call(\"read\", {{\"path\": \"/work/input.txt\"}})\n```"
                    )
                    .expect("writing to String cannot fail");
                }
                (ScriptLanguage::JavaScript, Capability::RegisterTools) => {
                    writeln!(
                        output,
                        "```js\nfunction summarize(args) {{ return String(args.value); }}\ntool_register(\"summarize\", \"summarize\", \"Summarize a value\", {{ type: \"object\" }});\n```"
                    )
                    .expect("writing to String cannot fail");
                }
                (ScriptLanguage::Monty, Capability::RegisterTools) => {
                    writeln!(
                        output,
                        "```python\ndef summarize(args):\n    return str(args[\"value\"])\n\ntool_register(\"summarize\", \"summarize\", \"Summarize a value\", {{\"type\": \"object\"}})\n```"
                    )
                    .expect("writing to String cannot fail");
                }
                (_, Capability::Hooks) => {
                    writeln!(
                        output,
                        "```text\non(\"tool_call\", handler)\non(\"tool_result\", handler)\n```"
                    )
                    .expect("writing to String cannot fail");
                }
                (_, Capability::Commands) => {
                    writeln!(output, "```text\nregister_command(\"name\", handler)\n```")
                        .expect("writing to String cannot fail");
                }
                (ScriptLanguage::JavaScript, Capability::Ui) => {
                    writeln!(
                        output,
                        "```js\nui_status(\"job\", \"running\", \"blue\", 1);\nui_panel(\"job\", \"Job\", [\"running\"]);\n```"
                    )
                    .expect("writing to String cannot fail");
                }
                (ScriptLanguage::Monty, Capability::Ui) => {
                    writeln!(
                        output,
                        "```python\nui_status(\"job\", \"running\", \"blue\", 1)\nui_panel(\"job\", \"Job\", [\"running\"])\n```"
                    )
                    .expect("writing to String cannot fail");
                }
                (ScriptLanguage::JavaScript, Capability::State) => {
                    writeln!(
                        output,
                        "```js\nawait state_set(\"last\", {{ value: 1 }});\nconst state = await state_get(\"last\");\nlog(\"info\", \"updated state\");\nreturn state;\n```"
                    )
                    .expect("writing to String cannot fail");
                }
                (ScriptLanguage::Monty, Capability::State) => {
                    writeln!(
                        output,
                        "```python\nstate_set(\"last\", {{\"value\": 1}})\nstate = state_get(\"last\")\nlog(\"info\", \"updated state\")\nreturn state\n```"
                    )
                    .expect("writing to String cannot fail");
                }
            }
        }
    }

    fn language_label(&self) -> &'static str {
        match self.language {
            ScriptLanguage::Monty => "Python（monty 子集）",
            ScriptLanguage::JavaScript => "JavaScript（rquickjs）",
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{bytes} bytes")
    }
}
