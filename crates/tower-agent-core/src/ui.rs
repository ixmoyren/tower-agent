//! 前端无关的 UI 贡献模型。
//!
//! 插件（尤其是 monty `mods`）用一份**声明式数据**描述它想在界面上贡献什么：
//! 状态栏片段、面板文本、某个工具结果的渲染模板。它不依赖 `ratatui` 或任何具体
//! 前端，因此终端 TUI、web、GUI 都能消费同一份 [`UiContribution`]。
//!
//! 视图状态由 [`UiState`] 承载：它可 clone、可跨线程共享，每次修改都会推进
//! [`UiState::version`]，前端据此判断是否需要重绘。

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use serde::{Deserialize, Serialize};

/// 一份 UI 贡献：状态栏片段 + 面板 + 工具结果渲染模板。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UiContribution {
    /// 状态栏片段（按 `priority` 降序排列后拼接）。
    pub status: Vec<StatusSegment>,
    /// 面板，key 为面板 id。
    pub panels: BTreeMap<String, Panel>,
    /// 工具名 → 渲染模板。
    pub tool_renderers: BTreeMap<String, ToolRenderer>,
}

/// 状态栏片段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusSegment {
    /// 片段 id（同一 id 的重复设置是覆盖）。
    pub id: String,
    /// 显示文本。
    pub text: String,
    /// 颜色名（`red` / `green` / `yellow` / `blue` / `magenta` / `cyan` / `gray`）。
    pub color: Option<String>,
    /// 排序权重，越大越靠右。
    pub priority: i64,
}

/// 面板文本。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Panel {
    /// 面板 id。
    pub id: String,
    /// 标题。
    pub title: String,
    /// 正文行。
    pub lines: Vec<String>,
    /// 停靠位置。
    pub anchor: PanelAnchor,
    /// 宽度（终端单元格，`None` 用前端默认值）。
    pub width: Option<u16>,
    /// 高度（终端格子数，`None` 用前端默认值）。
    pub height: Option<u16>,
}

impl Panel {
    /// 用 id、标题与正文行构造一个右上角面板。
    #[must_use]
    pub fn new(id: impl Into<String>, title: impl Into<String>, lines: Vec<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            lines,
            anchor: PanelAnchor::TopRight,
            width: None,
            height: None,
        }
    }
}

/// 面板停靠位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PanelAnchor {
    /// 左上。
    TopLeft,
    /// 右上（默认）。
    #[default]
    TopRight,
    /// 左下。
    BottomLeft,
    /// 右下。
    BottomRight,
    /// 居中。
    Center,
}

impl PanelAnchor {
    /// 从字符串解析（`top_left` / `top-right` / `TopRight` 等宽松写法）。
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "top_left" | "left" | "top" => Self::TopLeft,
            "bottom_left" => Self::BottomLeft,
            "bottom_right" | "bottom" => Self::BottomRight,
            "center" => Self::Center,
            _ => Self::TopRight,
        }
    }
}

/// 某个工具结果的渲染模板。
///
/// 模板行里的占位符会在渲染时替换：`{tool}`、`{summary}`、`{details}`、
/// `{status}`（`ok` / `error`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRenderer {
    /// 模板行。
    pub template: Vec<String>,
}

impl ToolRenderer {
    /// 用模板行构造。
    #[must_use]
    pub fn new(template: Vec<String>) -> Self {
        Self { template }
    }
}

/// 共享的 UI 状态：可 clone、可跨线程共享，带单调递增的版本号。
#[derive(Clone, Default)]
pub struct UiState {
    inner: Arc<Mutex<UiContribution>>,
    version: Arc<AtomicU64>,
}

impl std::fmt::Debug for UiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiState")
            .field("version", &self.version())
            .finish_non_exhaustive()
    }
}

impl UiState {
    /// 空状态。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前贡献的快照。
    #[must_use]
    pub fn snapshot(&self) -> UiContribution {
        self.inner.lock().expect("ui state poisoned").clone()
    }

    /// 当前版本号（每次修改递增）。
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    /// 用闭包修改贡献，并推进版本号。
    pub fn update(&self, edit: impl FnOnce(&mut UiContribution)) {
        {
            let mut contribution = self.inner.lock().expect("ui state poisoned");
            edit(&mut contribution);
        }
        self.version.fetch_add(1, Ordering::SeqCst);
    }

    /// 覆盖/插入一个状态栏片段。
    pub fn set_status(&self, segment: StatusSegment) {
        self.update(|contribution| {
            match contribution
                .status
                .iter_mut()
                .find(|existing| existing.id == segment.id)
            {
                Some(existing) => *existing = segment,
                None => contribution.status.push(segment),
            }
        });
    }

    /// 移除一个状态栏片段；返回是否移除了。
    pub fn remove_status(&self, id: &str) -> bool {
        let mut removed = false;
        self.update(|contribution| {
            let before = contribution.status.len();
            contribution.status.retain(|segment| segment.id != id);
            removed = contribution.status.len() != before;
        });
        removed
    }

    /// 覆盖/插入一个面板。
    pub fn set_panel(&self, panel: Panel) {
        self.update(|contribution| {
            contribution.panels.insert(panel.id.clone(), panel);
        });
    }

    /// 移除一个面板；返回是否移除了。
    pub fn remove_panel(&self, id: &str) -> bool {
        let mut removed = false;
        self.update(|contribution| {
            removed = contribution.panels.remove(id).is_some();
        });
        removed
    }

    /// 覆盖某个工具结果的渲染模板。
    pub fn set_tool_renderer(&self, tool: impl Into<String>, renderer: ToolRenderer) {
        self.update(|contribution| {
            contribution.tool_renderers.insert(tool.into(), renderer);
        });
    }

    /// 清空全部贡献。
    pub fn clear(&self) {
        self.update(|contribution| *contribution = UiContribution::default());
    }

    /// 状态栏片段按 `priority` 降序排序后的快照。
    #[must_use]
    pub fn sorted_status(&self) -> Vec<StatusSegment> {
        let mut status = self.snapshot().status;
        status.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.id.cmp(&right.id))
        });
        status
    }
}

/// `{summary}` 等占位符替换（供各前端复用的最小模板实现）。
#[must_use]
pub fn render_template(
    template: &[String],
    tool: &str,
    summary: &str,
    details: Option<&str>,
    is_error: bool,
) -> Vec<String> {
    let status = if is_error { "error" } else { "ok" };
    template
        .iter()
        .map(|line| {
            line.replace("{tool}", tool)
                .replace("{summary}", summary)
                .replace("{details}", details.unwrap_or(""))
                .replace("{status}", status)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_upserted_and_sorted_by_priority() {
        let ui = UiState::new();
        ui.set_status(StatusSegment {
            id: "a".into(),
            text: "a".into(),
            color: None,
            priority: 1,
        });
        ui.set_status(StatusSegment {
            id: "b".into(),
            text: "b".into(),
            color: None,
            priority: 5,
        });
        ui.set_status(StatusSegment {
            id: "a".into(),
            text: "a2".into(),
            color: None,
            priority: 9,
        });

        let status = ui.sorted_status();
        assert_eq!(status.len(), 2, "同 id 覆盖而不是追加");
        assert_eq!(status[0].text, "a2");
        assert_eq!(status[1].text, "b");
    }

    #[test]
    fn panels_and_renderers_round_trip() {
        let ui = UiState::new();
        ui.set_panel(Panel::new("p", "Title", vec!["x".into()]));
        ui.set_tool_renderer(
            "bash",
            ToolRenderer::new(vec!["{status}: {summary}".into()]),
        );
        let snapshot = ui.snapshot();
        assert_eq!(snapshot.panels["p"].title, "Title");
        assert_eq!(snapshot.tool_renderers["bash"].template.len(), 1);
        assert!(ui.remove_panel("p"));
        assert!(!ui.remove_panel("p"));
    }

    #[test]
    fn version_advances_on_every_edit() {
        let ui = UiState::new();
        let start = ui.version();
        ui.set_status(StatusSegment {
            id: "x".into(),
            text: "x".into(),
            color: None,
            priority: 0,
        });
        assert!(ui.version() > start);
    }

    #[test]
    fn template_substitutes_placeholders() {
        let rendered = render_template(
            &["{status} {tool}: {summary}".into(), "{details}".into()],
            "bash",
            "done",
            Some("detail"),
            false,
        );
        assert_eq!(rendered[0], "ok bash: done");
        assert_eq!(rendered[1], "detail");
    }
}
