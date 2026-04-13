use async_trait::async_trait;

use crate::domain::errors::ClipboardError;

/// Port for reading the system clipboard.
/// Implementations live in the adapters layer (arboard, noop).
/// The port is async so the adapter can hide spawn_blocking internally.
#[async_trait]
pub trait ClipboardPort: Send + Sync {
    /// Return the current clipboard image encoded as PNG bytes, or `None` if
    /// the clipboard contains no image.
    async fn read_image_png(&self) -> Result<Option<Vec<u8>>, ClipboardError>;

    /// Return the current clipboard text, or `None` if the clipboard is empty
    /// or contains only non-text content.
    async fn read_text(&self) -> Result<Option<String>, ClipboardError>;
}
