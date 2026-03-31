use super::visual::{OverlayType, PanelType};

/// Which UI region has keyboard focus.
/// Scroll state is owned by TuiState (adapter concern), not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum FocusState {
    Input,
    Chat,
    Sidebar { panel: PanelType, selected: usize },
    Overlay(OverlayType),
}
