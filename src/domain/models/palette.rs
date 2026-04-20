use serde::{Deserialize, Serialize};

/// Scope categories for command palette entries.
/// Determines which prefix filter surfaces the entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PaletteScope {
    /// No scope — matches all unscoped searches.
    All,
    /// `/` prefix — slash commands.
    SlashCommand,
    /// `@` prefix — files, agents, MCP servers.
    FileMention,
    /// `:` prefix — models and providers.
    Model,
    /// `>` prefix — profiles.
    Profile,
    /// `!` prefix — adapter management.
    Adapter,
}

/// Actions that can be dispatched from the command palette.
/// Stub variants (InsertMention, SwitchModel, SwitchProfile, OpenPanel) are
/// reserved for future epics (7, 8) and intentionally not yet constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum PaletteAction {
    /// Execute a slash command by name with optional args (e.g., ("mode", Some("plan"))).
    ExecuteCommand(String, Option<String>),
    /// Insert a mention at cursor position (e.g., "@path/to/file").
    InsertMention(String),
    /// Switch to a model by ID (stub for Epic 7).
    SwitchModel(String),
    /// Switch to a profile by name (stub for Epic 8).
    SwitchProfile(String),
    /// Open a sidebar panel (stub for future).
    OpenPanel(super::visual::PanelType),
    /// Show version info as a FeedbackBlock in the chat pane.
    // Covers: FR109
    ShowVersion,
    /// Create a new tab.
    NewTab,
    /// Close the active tab.
    CloseTab,
    /// Delete all conversations (with confirmation).
    // Covers: AC5 (bulk delete), P9
    DeleteAllConversations,
    /// Paste image (or text) from the system clipboard.
    PasteImageFromClipboard,
    /// Toggle the conversation history sidebar (Ctrl+H).
    // Covers: FR107, UX-DR20
    ToggleSidebar,
    /// No-op — target feature not yet implemented.
    Noop,
}

/// A single entry in the command palette.
#[derive(Debug, Clone)]
pub struct PaletteEntry {
    /// Display name shown in the palette.
    pub name: String,
    /// Brief description shown alongside the name.
    pub description: String,
    /// Keyboard shortcut string, if any (e.g., "Ctrl+X, M").
    pub shortcut: Option<String>,
    /// Scope determines which prefix filter surfaces this entry.
    pub scope: PaletteScope,
    /// Action to execute when this entry is selected.
    pub action: PaletteAction,
}
