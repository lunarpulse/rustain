use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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

/// Information density modes for the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DensityMode {
    Focus,
    Monitor,
    Dashboard,
}

/// Panel types for the sidebar focus target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PanelType {
    History,
    Tasks,
    Agents,
    Adapters,
}

/// Target of a delete confirmation dialog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeleteConfirmTarget {
    /// Delete a single conversation by ID with its title for display.
    Single { id: String, title: String },
    /// Delete all conversations with the count for display.
    Bulk { count: usize },
}

/// Types of confirmation dialogs.
/// Note: Not Copy because DeleteConfirmation contains String data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfirmationType {
    Permission,
    /// Permission feedback mini-input (AC5).
    PermissionFeedback,
    Question,
    /// Delete confirmation for sidebar conversations (AC5, AC6).
    DeleteConfirmation(DeleteConfirmTarget),
    /// Fork confirmation card (Story 4-3a, AC1).
    Fork,
    /// Rewind confirmation card (Story 4-3b, AC1).
    Rewind,
    /// Export overwrite confirmation (Story 4-4 AC12) —
    /// shown only in explicit-path mode when the target file already exists.
    ExportOverwrite(PathBuf),
}

/// Overlay types for modal focus targets.
/// Note: Not Copy because Confirmation contains non-Copy data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OverlayType {
    CommandPalette,
    ModelSelector,
    ProfileSwitcher,
    WhichKey,
    Help,
    Confirmation(ConfirmationType),
    /// Reverse search overlay for input history (Ctrl+R).
    // Covers: UX-DR74
    ReverseSearch,
    /// Within-conversation search overlay (Ctrl+F).
    // Covers: UX-DR86, Story 4-4 AC1-AC4
    Search,
    /// Cross-conversation search overlay (/ in sidebar).
    // Covers: UX-DR87, Story 4-4 AC5-AC7
    CrossSearch,
    /// Bookmark list bottom panel (' key).
    // Covers: UX-DR91, Story 4-4 AC10
    BookmarkList,
    /// Inline autocomplete popup (/ or @).
    // Covers: UX-DR75
    Autocomplete(super::autocomplete::AutocompleteKind),
}

/// Non-color semantic symbols so information conveys without color (UX-DR32, NFR34).
pub mod symbols {
    pub const SUCCESS: char = '✓';
    pub const WORKING: char = '●';
    pub const ERROR: char = '✗';
    pub const WARNING: char = '⚠';
    pub const INFO: char = 'ℹ';
    pub const OWNED: char = '♦';
    pub const PEER: char = '◇';
}
