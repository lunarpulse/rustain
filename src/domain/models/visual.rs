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
#[serde(rename_all = "lowercase")]
pub enum DensityMode {
    Focus,
    Monitor,
    Dashboard,
}

impl Default for DensityMode {
    fn default() -> Self {
        Self::Focus
    }
}

impl DensityMode {
    /// Single-char status-bar chip ('F' / 'M' / 'D').
    pub fn indicator_char(&self) -> char {
        match self {
            Self::Focus => 'F',
            Self::Monitor => 'M',
            Self::Dashboard => 'D',
        }
    }

    /// Full label for SystemNotice flash on transition + help overlay.
    pub fn display_label(&self) -> &'static str {
        match self {
            Self::Focus => "Focus",
            Self::Monitor => "Monitor",
            Self::Dashboard => "Dashboard",
        }
    }

    /// Default sidebar visibility for this mode.
    /// Focus = hidden; Monitor = visible; Dashboard = hidden (panels render in main area).
    pub fn default_sidebar_visible(&self) -> bool {
        matches!(self, Self::Monitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn density_mode_default_is_focus() {
        assert_eq!(DensityMode::default(), DensityMode::Focus);
    }

    #[test]
    fn indicator_chars_are_unique() {
        let chars = [
            DensityMode::Focus.indicator_char(),
            DensityMode::Monitor.indicator_char(),
            DensityMode::Dashboard.indicator_char(),
        ];
        assert_eq!(chars[0], 'F');
        assert_eq!(chars[1], 'M');
        assert_eq!(chars[2], 'D');
    }

    #[test]
    fn display_labels_are_correct() {
        assert_eq!(DensityMode::Focus.display_label(), "Focus");
        assert_eq!(DensityMode::Monitor.display_label(), "Monitor");
        assert_eq!(DensityMode::Dashboard.display_label(), "Dashboard");
    }

    #[test]
    fn only_monitor_defaults_sidebar_visible() {
        assert!(!DensityMode::Focus.default_sidebar_visible());
        assert!(DensityMode::Monitor.default_sidebar_visible());
        assert!(!DensityMode::Dashboard.default_sidebar_visible());
    }

    #[test]
    fn serde_roundtrip_lowercase() {
        let cases: &[(&str, DensityMode)] = &[
            ("focus", DensityMode::Focus),
            ("monitor", DensityMode::Monitor),
            ("dashboard", DensityMode::Dashboard),
        ];
        for (input, expected) in cases {
            let parsed: DensityMode = serde_json::from_str(&format!("\"{}\"", input)).unwrap();
            assert_eq!(parsed, *expected);
            let serialized = serde_json::to_string(expected).unwrap();
            assert_eq!(serialized, format!("\"{}\"", input));
        }
    }
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
    /// Skill trust prompt (Story 5-2 AC4) — y/n/i for workspace-tier skill activation.
    SkillTrust,
    /// Skill trust inspection mode (Story 5-2 AC4) — view file contents, Esc returns to prompt.
    SkillTrustInspect,
    /// Plan approval card (Story 6-0d AC4) — y/a/n/e for plan mode exit approval.
    PlanApproval,
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
    /// Usage / cost panel (Ctrl+X, U). Story 7.5 AC3 (UX-DR111).
    UsagePanel,
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
    pub const UNKNOWN: char = '·';
    pub const OWNED: char = '♦';
    pub const PEER: char = '◇';
}
