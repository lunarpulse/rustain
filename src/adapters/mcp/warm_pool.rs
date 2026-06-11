//! Warm-storage pool for persistent MCP servers across profile switches.
//!
//! Uses `tokio::sync::Mutex` (NOT `std::sync`) because the migration path
//! holds the lock across `.await`. This does NOT count against
//! `MAX_KNOWN_STD_SYNC_LOCKS` per process-architecture.md §1.2.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Type alias for the warm pool: server-id -> running service handle.
///
/// TODO(Story 9.1): Replace `()` with actual rmcp `RunningService` type
/// when full rmcp integration lands. For now, we store a placeholder.
pub type WarmPool = HashMap<String, Arc<()>>;

static WARM_POOL: std::sync::OnceLock<Mutex<WarmPool>> = std::sync::OnceLock::new();

fn get_pool() -> &'static Mutex<WarmPool> {
    WARM_POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Store a running service in the warm pool.
pub async fn store_in_warm_pool(server_id: String, _service: Arc<()>) {
    let pool = get_pool();
    let mut guard = pool.lock().await;
    guard.insert(server_id, _service);
}

/// Take a running service from the warm pool.
pub async fn take_from_warm_pool(server_id: &str) -> Option<Arc<()>> {
    let pool = get_pool();
    let mut guard = pool.lock().await;
    guard.remove(server_id)
}

/// Drain all entries from the warm pool (used during full shutdown).
pub async fn drain_warm_pool() -> WarmPool {
    let pool = get_pool();
    let mut guard = pool.lock().await;
    std::mem::take(&mut *guard)
}
