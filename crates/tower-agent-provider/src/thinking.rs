//! Thinking levels shared by requests, assistant messages, and runtime state.

use serde::{Deserialize, Serialize};

/// pi `ThinkingLevel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ThinkingLevel {
    /// Lowest thinking budget.
    Minimal,
    /// Low thinking budget.
    Low,
    /// Medium thinking budget.
    Medium,
    /// High thinking budget.
    High,
    /// Very high thinking budget.
    XHigh,
    /// Highest thinking budget.
    Max,
}

/// pi `ModelThinkingLevel` (`"off" | ThinkingLevel`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModelThinkingLevel {
    /// Thinking disabled.
    Off,
    /// Use the selected thinking level.
    Level(ThinkingLevel),
}
