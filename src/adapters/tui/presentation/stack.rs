//! Log/Quit frame identities. Only the presentation reducer mutates the stack.

/// Surfaces migrated in Task 1-A1; every other surface remains legacy-owned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    NotificationLog,
    QuitConfirm,
}

impl Surface {
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::NotificationLog => "System · Notifications",
            Self::QuitConfirm => "System · Quit",
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

/// Duplicate opens preserve both the frame identity and its payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opened {
    Pushed,
    Unchanged,
}
