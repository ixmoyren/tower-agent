//! UI 插件扩展点。
//!
//! [`UiPlugin`] 让插件在不改动内置渲染的前提下贡献 UI：状态栏片段、浮层（overlay）
//! 以及某个工具结果的定制渲染。它和 `agent` 的工具层（[`tower_agent_runtime::layer`]）配套：工具层
//! 决定工具**怎么执行**，`UiPlugin` 决定工具**怎么显示**。
//!
//! 后端固定为 termina（不使用 crossterm）；插件只依赖 ratatui 的渲染类型
//! （[`Rect`] / [`Buffer`] / [`Line`]），因此换后端不影响插件。

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph, Widget},
};
use tower_agent_core::{Panel, PanelAnchor, UiState, render_template};

pub use crate::model::ToolResultView;

/// 一个 UI 插件。
pub trait UiPlugin: Send + Sync + 'static {
    /// 插件名（用于日志/诊断）。
    fn name(&self) -> &str;

    /// 状态栏右侧片段；`None` 表示不贡献。
    fn status_segment(&self) -> Option<Line<'static>> {
        None
    }

    /// 绘制一个浮层；返回 `true` 表示已绘制（供调用方判断是否需要独占该区域）。
    fn render_overlay(&self, _area: Rect, _buf: &mut Buffer) -> bool {
        false
    }

    /// 覆盖某个工具结果的渲染；`None` 表示走内置渲染。
    fn render_tool_result(
        &self,
        _tool: &str,
        _result: &ToolResultView,
        _width: usize,
    ) -> Option<Vec<Line<'static>>> {
        None
    }
}

/// 内置 TUI 前端插件：把 TUI 本身注册进宿主，贡献一个状态栏片段。
///
/// 宿主通过 [`UiPlugin`] 装配前端——TUI 不是 `Service`（它订阅事件流），但**作为插件**
/// 参与装配；未来的 web 前端同样以 `UiPlugin` 形式接入。
pub struct TuiPlugin {
    label: String,
}

impl TuiPlugin {
    /// 用默认标签 `tui` 构造。
    #[must_use]
    pub fn new() -> Self {
        Self {
            label: "tui".into(),
        }
    }

    /// 用自定义标签构造（用于区分多个 TUI 前端）。
    #[must_use]
    pub fn with_label(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
        }
    }
}

impl Default for TuiPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl UiPlugin for TuiPlugin {
    fn name(&self) -> &str {
        "tui"
    }

    fn status_segment(&self) -> Option<Line<'static>> {
        Some(Line::from(Span::styled(
            format!("[{}]", self.label),
            Style::default().fg(Color::Cyan),
        )))
    }
}

/// 宿主便捷：内置前端插件集合（当前只有 TUI）。
#[must_use]
pub fn frontend_plugins() -> Vec<Box<dyn UiPlugin>> {
    vec![Box::new(TuiPlugin::new())]
}

/// 把共享 [`UiState`] 里的 mod 贡献渲染出来（状态栏片段 / 面板 / 工具结果模板）。
///
/// 这是「mods 改 TUI」的终端适配器：它只消费前端无关的数据模型，因此 web / GUI
/// 前端可以用同一份 `UiState` 走各自的渲染路径。
pub struct ContributionPlugin {
    ui: UiState,
}

impl ContributionPlugin {
    /// 用共享 UI 状态构造。
    #[must_use]
    pub fn new(ui: UiState) -> Self {
        Self { ui }
    }
}

impl UiPlugin for ContributionPlugin {
    fn name(&self) -> &str {
        "mods"
    }

    fn status_segment(&self) -> Option<Line<'static>> {
        let segments = self.ui.sorted_status();
        if segments.is_empty() {
            return None;
        }
        let mut spans = Vec::with_capacity(segments.len() * 2);
        for (index, segment) in segments.into_iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw(" "));
            }
            let style = match segment.color.as_deref().and_then(color_for) {
                Some(color) => Style::default().fg(color),
                None => Style::default(),
            };
            spans.push(Span::styled(segment.text, style));
        }
        Some(Line::from(spans))
    }

    fn render_overlay(&self, area: Rect, buf: &mut Buffer) -> bool {
        let panels = self.ui.snapshot().panels;
        if panels.is_empty() {
            return false;
        }
        let mut drew = false;
        for panel in panels.values() {
            let rect = panel_rect(area, panel);
            if rect.width < 3 || rect.height < 3 {
                continue;
            }
            Clear.render(rect, buf);
            Paragraph::new(panel.lines.join("\n"))
                .block(Block::bordered().title(panel.title.clone()))
                .render(rect, buf);
            drew = true;
        }
        drew
    }

    fn render_tool_result(
        &self,
        tool: &str,
        result: &ToolResultView,
        _width: usize,
    ) -> Option<Vec<Line<'static>>> {
        let renderer = self.ui.snapshot().tool_renderers.get(tool).cloned()?;
        let lines = render_template(
            &renderer.template,
            tool,
            &result.summary,
            result.details.as_deref(),
            result.is_error,
        );
        Some(lines.into_iter().map(Line::from).collect())
    }
}

/// 面板 -> 终端矩形（按 anchor 停靠并夹在可用区域内）。
fn panel_rect(area: Rect, panel: &Panel) -> Rect {
    let default_height = panel.lines.len() as u16 + 2;
    let width = panel.width.unwrap_or(40).min(area.width);
    let height = panel.height.unwrap_or(default_height).min(area.height);
    let (x, y) = match panel.anchor {
        PanelAnchor::TopLeft => (area.x, area.y),
        PanelAnchor::TopRight => (area.x + area.width.saturating_sub(width), area.y),
        PanelAnchor::BottomLeft => (area.x, area.y + area.height.saturating_sub(height)),
        PanelAnchor::BottomRight => (
            area.x + area.width.saturating_sub(width),
            area.y + area.height.saturating_sub(height),
        ),
        PanelAnchor::Center => (
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
        ),
    };
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// 颜色名 -> ratatui 颜色。
fn color_for(name: &str) -> Option<Color> {
    Some(match name.to_ascii_lowercase().as_str() {
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "white" => Color::White,
        _ => return None,
    })
}
