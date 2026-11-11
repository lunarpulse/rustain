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
