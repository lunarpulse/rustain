use super::visual::{OverlayType, PanelType};

/// Which UI region has keyboard focus.
/// Scroll state is owned by TuiState (adapter concern), not here.
/// Note: Not Copy because Overlay contains non-Copy data.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum FocusState {
    Input,
    Chat,
    Sidebar { panel: PanelType, selected: usize },
    Overlay(OverlayType),
}
