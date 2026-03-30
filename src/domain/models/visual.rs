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

/// Overlay types for modal focus targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OverlayType {
    CommandPalette,
    ModelSelector,
    ProfileSwitcher,
    WhichKey,
    Help,
    // TODO(1.6): Add Confirmation(ConfirmationType) when permission system story implements it
}

/// Non-color semantic symbols so information conveys without color (UX-DR32, NFR34).
pub mod symbols {
    pub const SUCCESS: char = '✓';
    pub const WORKING: char = '●';
    pub const ERROR: char = '✗';
    pub const WARNING: char = '⚠';
    pub const OWNED: char = '♦';
    pub const PEER: char = '◇';
}
