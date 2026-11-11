/// Which UI region has keyboard focus.
/// Scroll state is owned by TuiState (adapter concern), not here.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum FocusState {
    Input,
    Chat,
}
