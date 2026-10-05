//! UI 插件扩展点。
//!
//! [`UiPlugin`] 让插件在不改动内置渲染的前提下贡献 UI：状态栏片段、浮层（overlay）
//! 以及某个工具结果的定制渲染。它和 `agent` 的工具层（[`agent::layer`]）配套：工具层
//! 决定工具**怎么执行**，`UiPlugin` 决定工具**怎么显示**。
//!
//! 后端固定为 termina（不使用 crossterm）；插件只依赖 ratatui 的渲染类型
//! （[`Rect`] / [`Buffer`] / [`Line`]），因此换后端不影响插件。

use ratatui::{buffer::Buffer, layout::Rect, text::Line};

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
