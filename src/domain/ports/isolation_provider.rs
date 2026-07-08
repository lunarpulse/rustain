use std::path::Path;

use async_trait::async_trait;

use crate::domain::models::{IsolationError, IsolationHandle, UnifiedDiff};

/// Filesystem scratch/Cow isolation seam.
///
/// This port models private workspace copies only. The R2 WASM
/// `ExecutionSandbox` remains a sibling seam, never a widening or super-trait
/// of this port (ADR-11-3 rule 3).
#[async_trait]
pub trait IsolationProvider: Send + Sync {
    /// Start a scratch clone over `lower` (overlayfs "lower-dir" terminology).
    async fn start(&self, lower: &Path) -> Result<IsolationHandle, IsolationError>;

    /// Capture the child's delta against the clone as a serializable diff.
    async fn diff(&self, h: &IsolationHandle) -> Result<UnifiedDiff, IsolationError>;

    /// Destructive teardown — consumes the handle and removes only the clone dir.
    async fn stop(&self, h: IsolationHandle) -> Result<(), IsolationError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn assert_object_safe(_: Arc<dyn IsolationProvider>) {}
}
