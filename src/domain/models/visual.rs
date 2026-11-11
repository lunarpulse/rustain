use serde::{Deserialize, Serialize};

/// Border style variants for content blocks.
/// Mapped to ratatui `BorderType` + optional custom line sets in the theme layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockBorder {
    None,
    DottedThin,
    SolidThin,
    BoldThick,
    Double,
    AgentAuto,
}

/// Target of a delete confirmation dialog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeleteConfirmTarget {
    /// Delete a single conversation by ID with its title for display.
    Single { id: String, title: String },
    /// Delete all conversations with the count for display.
    Bulk { count: usize },
}

/// Non-color semantic symbols so information conveys without color (UX-DR32, NFR34).
pub mod symbols {
    pub const SUCCESS: char = '✓';
    pub const WORKING: char = '●';
    pub const ERROR: char = '✗';
    pub const WARNING: char = '⚠';
    pub const INFO: char = 'ℹ';
    pub const UNKNOWN: char = '·';
    pub const OWNED: char = '♦';
    pub const PEER: char = '◇';
}
