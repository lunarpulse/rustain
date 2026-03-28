/// Which UI region has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusState {
    Input,
    Chat,
}
