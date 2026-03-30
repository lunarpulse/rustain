use super::visual::{OverlayType, PanelType};

/// Which UI region has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum FocusState {
    Input,
    Chat { scroll_offset: usize },
    Sidebar { panel: PanelType, selected: usize },
    Overlay(OverlayType),
}
