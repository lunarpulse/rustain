//! Frame identities. Only the presentation reducer mutates the stack.

/// The four migrated surfaces. The pane and fifth-surface continuations land in A2b.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    CommandPalette,
    Help,
    NotificationLog,
    QuitConfirm,
}

impl Surface {
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::CommandPalette => "Command Palette",
            Self::Help => "Help — Rustain Keybindings",
            Self::NotificationLog => "System · Notifications",
            Self::QuitConfirm => "System · Quit",
        }
    }

    pub const fn context(self) -> crate::adapters::tui::keymap::Context {
        use crate::adapters::tui::keymap::Context;
        match self {
            Self::CommandPalette => Context::CommandPalette,
            Self::Help => Context::Help,
            Self::NotificationLog => Context::NotificationLog,
            Self::QuitConfirm => Context::QuitConfirm,
        }
    }
}

/// Stable identity of one opening, independent of its editable state slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub id: u64,
    pub surface: Surface,
    pub conversation_id: Option<String>,
}

/// Read-only to callers; frame insertion/removal belongs to the reducer.
#[derive(Debug, Default)]
pub struct ModalStack {
    pub(super) frames: Vec<Frame>,
    pub(super) last_id: u64,
}

impl ModalStack {
    #[must_use]
    pub fn top(&self) -> Option<&Frame> {
        self.frames.last()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Frame> {
        self.frames.iter()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

/// Reopening a covered surface closes the frames above it without resetting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opened {
    Pushed,
    Unchanged,
    Truncated,
}
