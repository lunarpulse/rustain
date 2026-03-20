use crossterm::event::KeyEvent;
use tokio::sync::oneshot;

use super::stream::TuiStreamEvent;

/// Approval decision from the user for tool execution
#[derive(Debug, Clone, Copy)]
pub enum ApprovalDecision {
    Allow,
    AlwaysAllow,
    Deny,
    Cancel,
}

/// Permission request sent from streaming task to UI.
/// The streaming task blocks on `response_tx` until the UI sends a decision.
pub struct PermissionRequest {
    pub tool_name: String,
    pub tool_input: String,
    pub tool_id: String,
    pub response_tx: oneshot::Sender<ApprovalDecision>,
}

// Manual Debug impl since oneshot::Sender doesn't implement Debug
impl std::fmt::Debug for PermissionRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermissionRequest")
            .field("tool_name", &self.tool_name)
            .field("tool_id", &self.tool_id)
            .finish()
    }
}

/// Unified application event — all event sources feed into a single channel.
/// The main loop consumes these via tokio::select! for clean concurrent handling.
#[derive(Debug)]
pub enum AppEvent {
    /// Terminal key press
    Key(KeyEvent),

    /// Terminal resize
    Resize(u16, u16),

    /// LLM stream event (from background streaming task)
    Stream(TuiStreamEvent),

    /// Tool permission request (streaming task blocks until UI responds)
    Permission(PermissionRequest),

    /// Periodic tick for animations (spinners, cursor blink)
    Tick,
}
