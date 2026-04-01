use crate::domain::models::{NoticeLevel, StreamChunk, ToolResult};

/// Domain-level application events flowing through the event loop.
/// crossterm types MUST NOT appear here — the adapter converts them to DomainInputEvent.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// Converted terminal input (crossterm → domain)
    InputEvent(DomainInputEvent),
    /// Streaming chunk from provider
    ProviderChunk(StreamChunk),
    /// Tool execution result
    ToolResult(ToolResultEvent),
    /// Terminal resize
    Resize(u16, u16),
    /// Render tick
    Tick,
    /// Graceful shutdown request
    Shutdown,
    /// System notice (status bar messages)
    SystemNotice(NoticeLevel, String),
    /// Internal domain events (legacy — kept for backward compat with 1.1a event loop)
    DomainEvent(DomainEventPayload),
}

/// Event wrapping a tool execution result.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ToolResultEvent {
    pub tab_id: String,
    pub result: ToolResult,
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
    CtrlC,
}

/// Payload for domain-originated events (placeholder for future stories).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum DomainEventPayload {
    /// Placeholder — streaming events, tool results, etc. added in later stories
    Noop,
}

/// Action returned by apply_chunk() to tell the event loop what to do.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkAction {
    /// No action needed.
    None,
    /// Trigger a redraw on next tick.
    NeedsRedraw,
    /// Turn is complete — persist and optionally generate title.
    TurnComplete {
        persist: bool,
        trigger_title_generation: bool,
    },
    /// Turn continues (tool use loop).
    TurnContinuing,
}
