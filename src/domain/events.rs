use crate::domain::models::NoticeLevel;

/// Domain-level application events flowing through the event loop.
/// crossterm types MUST NOT appear here — the adapter converts them to DomainInputEvent.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// Converted terminal input (crossterm → domain)
    InputEvent(DomainInputEvent),
    /// Internal domain events
    DomainEvent(DomainEventPayload),
    /// Render tick
    Tick,
    /// Graceful shutdown request
    Shutdown,
    /// System notice (status bar messages)
    SystemNotice(NoticeLevel, String),
}

/// Terminal input events, abstracted from crossterm.
/// The TUI adapter converts `crossterm::event::Event` into these.
#[derive(Debug, Clone)]
pub enum DomainInputEvent {
    KeyPress(char),
    SpecialKey(DomainKey),
    Resize(u16, u16),
    FocusGained,
    FocusLost,
}

/// Abstract key representation — no crossterm dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainKey {
    Enter,
    Esc,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    Tab,
}

/// Payload for domain-originated events (placeholder for future stories).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum DomainEventPayload {
    /// Placeholder — streaming events, tool results, etc. added in later stories
    Noop,
}

/// Chunk action stub — used in streaming pipeline (Story 1.2+).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum ChunkAction {
    Append(String),
    Replace(String),
    Complete,
}
