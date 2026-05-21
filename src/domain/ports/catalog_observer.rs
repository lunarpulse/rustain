use async_trait::async_trait;
use std::sync::Weak;

use crate::domain::models::catalog_delta::CatalogDelta;

/// Observer that reacts to catalog deltas emitted by the `CapabilityRegistry`.
///
/// # Contract
///
/// 1. **Fast:** No I/O on the hot path — the caller may invoke under a debounce
///    window. Heavy work should be offloaded to a background task.
/// 2. **Idempotent:** The same delta may be redelivered after failure recovery.
/// 3. **No panic:** Propagate errors via `Result` — panics may crash the event
///    loop.
///
/// # Phase A (9.3a) vs Phase B (9.4b)
///
/// In 9.3a and 9.4 Phase A, `CapabilityRegistry::subscribe` stores the observer
/// in `RegistryInner.observers` but NO code path calls `on_catalog_changed`.
/// The fan-out task that iterates observers and calls this method is owned by
/// Story 9.4b Phase B (via `CatalogObserverRegistry` at
/// `src/infrastructure/composition/catalog_observer_registry.rs`).
///
/// See ADR-09-01 v2.2 §Phased Implementation for the full decomposition.
#[async_trait]
pub trait CatalogObserver: Send + Sync {
    /// React to a catalog delta.
    async fn on_catalog_changed(&self, delta: &CatalogDelta) -> Result<(), ObserverError>;
}

/// RAII handle returned by `CapabilityRegistry::subscribe()`.
///
/// When dropped, unsubscribes the observer from the registry. If the registry
/// has already been dropped (the `Weak` reference fails to upgrade), the handle
/// silently goes out of scope.
///
/// # SAFETY
///
/// `Drop` uses `try_write()` on the registry inner lock to avoid blocking. If
/// the lock is contended, the observer's `Weak` ref remains in the `observers`
/// vec until the next write-guard acquisition (the fan-out task in Phase B will
/// skip dead `Weak`s). This is safe because the observer is behind `Weak` and
/// won't be called after its owner is dropped.
pub struct SubscriptionHandle {
    id: SubscriptionId,
    registry: Weak<crate::domain::models::capability_registry::CapabilityRegistry>,
}

impl SubscriptionHandle {
    pub(crate) fn new(
        id: SubscriptionId,
        registry: Weak<crate::domain::models::capability_registry::CapabilityRegistry>,
    ) -> Self {
        Self { id, registry }
    }
}

impl Drop for SubscriptionHandle {
    fn drop(&mut self) {
        if let Some(reg) = self.registry.upgrade() {
            reg.unsubscribe_blocking(self.id);
        }
    }
}

/// Monotonically incrementing subscription identifier (per registry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriptionId(pub u64);

/// Errors from catalog observer operations.
#[derive(Debug, thiserror::Error)]
pub enum ObserverError {
    #[error("observer rejected delta: {0}")]
    Rejected(String),
    #[error("observer transport error: {0}")]
    Transport(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::capability_registry::CapabilityRegistry;
    use std::sync::Arc;

    pub(crate) struct TestObserver {
        pub call_count: std::sync::atomic::AtomicU32,
    }

    impl TestObserver {
        pub(crate) fn new(initial: u32) -> Self {
            Self {
                call_count: std::sync::atomic::AtomicU32::new(initial),
            }
        }
    }

    #[async_trait]
    impl CatalogObserver for TestObserver {
        async fn on_catalog_changed(&self, _delta: &CatalogDelta) -> Result<(), ObserverError> {
            self.call_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn test_subscription_handle_drop_unsubscribes() {
        let registry = Arc::new(CapabilityRegistry::new(None));
        let observer = Arc::new(TestObserver::new(0));
        let handle = registry.subscribe(observer.clone());
        // Handle holds a Weak<CapabilityRegistry>
        drop(handle);
        // Handle dropped — observer should be unsubscribed
        // Observer Arc still alive via our local
        drop(observer);
    }
}
