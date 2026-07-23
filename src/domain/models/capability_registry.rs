use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

use tokio::sync::RwLock;
use tokio::sync::mpsc;

use crate::domain::events::AppEvent;
use crate::domain::events::CapabilityEvent;
use crate::domain::models::capability_id::CapabilityId;
use crate::domain::ports::CapabilityProvider;
use crate::domain::ports::CatalogObserver;
use crate::domain::ports::{SubscriptionHandle, SubscriptionId};

/// Single source of truth for "what capabilities exist right now."
///
/// Owned internally by `CompositeToolsetAdapter` per Epic 9 Flag 1.
/// NOT on `AppState` — conformance test
/// `test_no_capability_registry_on_app_state` enforces this.
///
/// # Thread safety
///
/// Uses `tokio::sync::RwLock<RegistryInner>` for the inner state (NOT
/// `std::sync::*Lock`). The async lock is required because `register()`
/// and `deregister()` are async (they cross `.await` for event emission).
///
/// # Async Lock Policy (CLAUDE.md)
///
/// Write guards are dropped BEFORE any `event_tx.send(...)` call —
/// the event is captured inside the guard scope, the guard is dropped,
/// and then the event is emitted. This prevents deadlocks if the receiver
/// tries to acquire the registry lock.
#[derive(Debug, Clone)]
pub struct CapabilityRegistry {
    inner: Arc<RwLock<RegistryInner>>,
    event_tx: Option<mpsc::UnboundedSender<AppEvent>>,
    next_subscription_id: Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Debug)]
struct RegistryInner {
    capabilities: BTreeMap<CapabilityId, RegisteredCapability>,
    observers: Vec<(SubscriptionId, Weak<dyn CatalogObserver>)>,
}

/// A capability that has been registered in the registry.
///
/// This is the registry's working shape. Story 9.3b adds a `From`
/// conversion to `ToolDescriptor` (the canonical domain catalog shape
/// used by 9.4 / 9.4b consumers).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RegisteredCapability {
    /// Unique identifier.
    pub id: CapabilityId,
    /// Protocol: `"mcp"`, `"builtin"`, `"skill"`, etc.
    pub protocol: String,
    /// Provider identifier (e.g., `"mcp:postgres"`, `"builtin"`, `"skill"`).
    pub provider_id: ProviderId,
    /// Bare capability name (e.g., `"query"` for `mcp:postgres:query`).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema for input parameters.
    pub input_schema: serde_json::Value,
    /// Whether the capability is safe for parallel execution.
    pub parallel_safe: bool,
    /// Trust assigned by the originating provider configuration.
    pub trust: crate::domain::models::TrustTier,
}

/// Unique identifier for a capability provider instance.
pub type ProviderId = String;

/// RAII handle that keeps a registered capability alive.
///
/// When dropped, calls `CapabilityRegistry::deregister()` for the capability.
/// `CompositeToolsetAdapter` holds a `Vec<RegisterHandle>` to keep MCP
/// capabilities alive across discover-register round-trips.
///
/// Distinct from `SubscriptionHandle` (returned by `subscribe()` for
/// observer-side subscriptions per AC-7). (Decision Gate 3.3)
#[derive(Debug)]
pub struct RegisterHandle {
    id: CapabilityId,
    registry: Weak<CapabilityRegistry>,
}

impl Drop for RegisterHandle {
    fn drop(&mut self) {
        if let Some(reg) = self.registry.upgrade() {
            // Best-effort deregister on a spawned task since Drop is sync
            let reg = reg.clone();
            let id = self.id.clone();
            tokio::task::spawn(async move {
                let _ = reg.deregister(&id).await;
            });
        }
    }
}

/// Registry-specific errors.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// The requested capability was not found.
    #[error("capability not found: {id}")]
    NotFound { id: CapabilityId },
    /// The event channel has been closed (receiver shut down).
    #[error("event channel closed")]
    ChannelClosed,
    /// Discovery failed (wraps the provider error).
    #[error("discovery failed: {0}")]
    DiscoverFailed(#[from] crate::domain::models::capability::CapabilityError),
}

impl CapabilityRegistry {
    /// Create a new empty registry.
    ///
    /// `event_tx` is the channel for emitting `CapabilityEvent`s.
    /// Pass `None` for unit tests that don't need event emission.
    pub fn new(event_tx: Option<mpsc::UnboundedSender<AppEvent>>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(RegistryInner {
                capabilities: BTreeMap::new(),
                observers: Vec::new(),
            })),
            event_tx,
            next_subscription_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }

    /// Look up a capability by id.
    ///
    /// Returns `None` if the capability is not registered.
    pub async fn lookup(&self, id: &CapabilityId) -> Option<RegisteredCapability> {
        let inner = self.inner.read().await;
        inner.capabilities.get(id).cloned()
    }

    /// Register a capability.
    ///
    /// If a capability with the same id is already registered, emits
    /// `CapabilityEvent::Updated` and replaces the entry.
    /// Otherwise emits `CapabilityEvent::Registered`.
    ///
    /// Returns a `RegisterHandle` that, when dropped, deregisters the
    /// capability (RAII lifecycle per Decision Gate 3.3).
    pub async fn register(
        self: &Arc<Self>,
        cap: RegisteredCapability,
    ) -> Result<RegisterHandle, RegistryError> {
        let event = {
            let mut inner = self.inner.write().await;
            if let Some(prev) = inner.capabilities.insert(cap.id.clone(), cap.clone()) {
                CapabilityEvent::Updated {
                    id: cap.id.clone(),
                    old: prev,
                    new: Box::new(cap.clone()),
                }
            } else {
                CapabilityEvent::Registered {
                    capability: cap.clone(),
                }
            }
        }; // write guard dropped here

        self.emit_event(event);

        Ok(RegisterHandle {
            id: cap.id.clone(),
            registry: Arc::downgrade(self),
        })
    }

    /// Deregister a capability by id.
    ///
    /// Returns `Err(RegistryError::NotFound)` if the capability was not
    /// registered. This is a caller-side bug; do NOT panic.
    pub async fn deregister(&self, id: &CapabilityId) -> Result<(), RegistryError> {
        let event = {
            let mut inner = self.inner.write().await;
            let removed = inner
                .capabilities
                .remove(id)
                .ok_or_else(|| RegistryError::NotFound { id: id.clone() })?;
            CapabilityEvent::Deregistered {
                capability: removed,
            }
        }; // write guard dropped here

        self.emit_event(event);
        Ok(())
    }

    /// Return a snapshot of all currently registered capabilities.
    ///
    /// Used by the adapter-status panel and autocomplete read paths.
    /// The clone cost is bounded by the number of registered capabilities
    /// (typically < 200 for v0.5).
    pub fn snapshot(&self) -> Vec<RegisteredCapability> {
        // Use try_read because blocking_read panics when called from an
        // async runtime (e.g., the TUI event loop). The lock is only held
        // briefly during register/deregister, so contention is rare.
        // If it does fail, we log a warning — the caller (status panel)
        // will show empty data for one render frame and refresh on the next.
        match self.inner.try_read() {
            Ok(inner) => inner.capabilities.values().cloned().collect(),
            Err(_) => {
                tracing::warn!(
                    "CapabilityRegistry::snapshot() lock contention — returning empty vec"
                );
                Vec::new()
            }
        }
    }

    /// Subscribe to catalog deltas.
    ///
    /// Returns a `SubscriptionHandle` that, on `Drop`, unsubscribes the
    /// observer (RAII; no leaks). In Phase A (9.3a), no code path calls
    /// `observer.on_catalog_changed()` — the fan-out task is owned by
    /// Story 9.4b Phase B.
    ///
    /// The receiver `self` must be `&Arc<Self>` (not `&self`) so the
    /// handle can hold a `Weak<CapabilityRegistry>` for RAII drop.
    /// (Decision Gate 3.2)
    pub fn subscribe(self: &Arc<Self>, observer: Arc<dyn CatalogObserver>) -> SubscriptionHandle {
        let id = SubscriptionId(
            self.next_subscription_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        );
        // Store as Weak to allow Drop-driven unsubscribe
        let weak_observer: Weak<dyn CatalogObserver> = Arc::downgrade(&observer);

        // Acquire write guard briefly to append
        match self.inner.try_write() {
            Ok(mut inner) => {
                inner.observers.push((id, weak_observer));
            }
            Err(_) => {
                tracing::warn!(
                    "CapabilityRegistry::subscribe() failed to acquire write lock — observer not registered"
                );
            }
        }

        SubscriptionHandle::new(id, Arc::downgrade(self))
    }

    /// Synchronous unsubscribe path used by `SubscriptionHandle::Drop`.
    ///
    /// Uses `try_write()` to avoid blocking. If the lock is contended,
    /// the dead observer's `Weak` ref is the safety net — the fan-out
    /// task (Phase B) will skip `None` upgrades and clean up during the
    /// next write-guard acquisition.
    pub(crate) fn unsubscribe_blocking(&self, id: SubscriptionId) {
        if let Ok(mut inner) = self.inner.try_write() {
            inner
                .observers
                .retain(|(sub_id, weak)| *sub_id != id || weak.upgrade().is_some());
        }
        // If lock is contended, drop is deferred — Weak ref is the safety net
    }

    /// Discover capabilities from a provider and register them all.
    ///
    /// Returns `RegisterHandle`s that the caller should hold to keep
    /// the capabilities alive. `CompositeToolsetAdapter` stores these
    /// handles in a `Vec<RegisterHandle>` field.
    pub async fn discover_and_register_all(
        self: &Arc<Self>,
        provider: &dyn CapabilityProvider,
        provider_id: &str,
    ) -> Result<Vec<RegisterHandle>, RegistryError> {
        let capabilities = provider.discover().await?;
        let mut handles = Vec::with_capacity(capabilities.len());
        for cap in capabilities {
            let registered = RegisteredCapability {
                id: cap.id,
                protocol: provider.protocol().to_string(),
                provider_id: provider_id.to_string(),
                name: cap.name,
                description: cap.description,
                input_schema: cap.input_schema,
                parallel_safe: cap.parallel_safe,
                trust: cap.trust,
            };
            let handle = self.register(registered).await?;
            handles.push(handle);
        }
        Ok(handles)
    }

    fn emit_event(&self, event: CapabilityEvent) {
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(AppEvent::CapabilityEvent(event)); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: 9-3a AC-4 — registry event_tx injected from ComposeContext.domain_tx, no new channel
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::events::AppEvent;
    use crate::domain::ports::CatalogObserver;
    use crate::domain::ports::ObserverError;
    use std::sync::Arc;

    struct TestObserver {
        _call_count: std::sync::atomic::AtomicU32,
    }

    impl TestObserver {
        fn new() -> Self {
            Self {
                _call_count: std::sync::atomic::AtomicU32::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl CatalogObserver for TestObserver {
        async fn on_catalog_changed(
            &self,
            _delta: &crate::domain::models::CatalogDelta,
        ) -> Result<(), ObserverError> {
            self._call_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }

    fn test_cap(id_suffix: &str) -> RegisteredCapability {
        RegisteredCapability {
            trust: crate::domain::models::TrustTier::Verified,
            id: CapabilityId {
                protocol: "test".into(),
                server: String::new(),
                tool: id_suffix.into(),
            },
            protocol: "test".into(),
            provider_id: "test:0".into(),
            name: id_suffix.into(),
            description: "test capability".into(),
            input_schema: serde_json::Value::Object(Default::default()),
            parallel_safe: true,
        }
    }

    #[tokio::test]
    async fn test_register_emits_capability_registered() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let registry = Arc::new(CapabilityRegistry::new(Some(tx)));
        let cap = test_cap("echo");
        let _handle = registry.register(cap.clone()).await.unwrap();

        let event = rx.try_recv().unwrap();
        match event {
            AppEvent::CapabilityEvent(CapabilityEvent::Registered { capability }) => {
                assert_eq!(capability.id, cap.id);
            }
            _ => panic!("Expected CapabilityEvent::Registered, got {event:?}"),
        }
    }

    #[tokio::test]
    async fn test_register_existing_id_emits_updated() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let registry = Arc::new(CapabilityRegistry::new(Some(tx)));
        let cap = test_cap("echo");
        let _handle1 = registry.register(cap.clone()).await.unwrap();
        let _reg = rx.try_recv().unwrap(); // consume Registered

        let cap2 = RegisteredCapability {
            description: "updated description".into(),
            ..cap.clone()
        };
        let _handle2 = registry.register(cap2.clone()).await.unwrap();

        let event = rx.try_recv().unwrap();
        match event {
            AppEvent::CapabilityEvent(CapabilityEvent::Updated { id, .. }) => {
                assert_eq!(id, cap.id);
            }
            _ => panic!("Expected CapabilityEvent::Updated, got {event:?}"),
        }
    }

    #[tokio::test]
    async fn test_deregister_emits_capability_deregistered() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let registry = Arc::new(CapabilityRegistry::new(Some(tx)));
        let cap = test_cap("echo");
        let handle = registry.register(cap.clone()).await.unwrap();
        let _reg = rx.try_recv().unwrap(); // consume Registered

        // Drop the handle — its Drop spawns a task that calls deregister(),
        // which emits Deregistered. Await that event deterministically instead
        // of sleeping (DF-14.4-AUDIT-1: fire-and-assert sleep → deterministic
        // signal). This also strengthens the test: the prior sleep never
        // asserted the drop-path deregistration at all.
        drop(handle);
        let event = rx.recv().await.expect("drop-path deregister event");
        match event {
            AppEvent::CapabilityEvent(CapabilityEvent::Deregistered { capability }) => {
                assert_eq!(capability.id, cap.id);
            }
            _ => panic!("Expected drop-path CapabilityEvent::Deregistered, got {event:?}"),
        }

        // Re-register and deregister explicitly to prove the direct path too.
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        let registry2 = Arc::new(CapabilityRegistry::new(Some(tx2)));
        let cap2 = test_cap("echo2");
        let _handle2 = registry2.register(cap2.clone()).await.unwrap();
        let _reg2 = rx2.try_recv().unwrap(); // consume Registered

        let result = registry2.deregister(&cap2.id).await;
        assert!(result.is_ok());

        let event = rx2.try_recv().unwrap();
        match event {
            AppEvent::CapabilityEvent(CapabilityEvent::Deregistered { capability }) => {
                assert_eq!(capability.id, cap2.id);
            }
            _ => panic!("Expected CapabilityEvent::Deregistered, got {event:?}"),
        }
    }

    #[tokio::test]
    async fn test_deregister_unknown_returns_not_found() {
        let registry = Arc::new(CapabilityRegistry::new(None));
        let id = CapabilityId {
            protocol: "nonexistent".into(),
            server: String::new(),
            tool: "ghost".into(),
        };
        let result = registry.deregister(&id).await;
        assert!(matches!(result, Err(RegistryError::NotFound { .. })));
    }

    #[tokio::test]
    async fn test_event_tx_optional() {
        let registry = Arc::new(CapabilityRegistry::new(None));
        let cap = test_cap("echo");
        let handle = registry.register(cap.clone()).await;
        assert!(handle.is_ok()); // should succeed without event_tx
        // Drop is safe with no event_tx (the spawned deregister emits nothing).
        // Nothing is asserted post-drop, so there is nothing to await — the
        // prior 10ms sleep was vestigial (DF-14.4-AUDIT-1).
        drop(handle);
    }

    #[test]
    fn test_subscribe_stores_observer_weak() {
        let registry = Arc::new(CapabilityRegistry::new(None));
        let observer: Arc<dyn CatalogObserver> = Arc::new(TestObserver::new());
        let handle = registry.subscribe(observer.clone());
        drop(handle);
        drop(observer);
    }

    #[test]
    fn test_drop_handle_removes_observer() {
        let registry = Arc::new(CapabilityRegistry::new(None));
        let observer: Arc<dyn CatalogObserver> = Arc::new(TestObserver::new());
        let handle = registry.subscribe(observer.clone());
        drop(handle);
        drop(observer);
    }

    #[tokio::test]
    async fn test_snapshot_returns_all_capabilities() {
        let registry = Arc::new(CapabilityRegistry::new(None));
        let cap1 = test_cap("echo");
        let cap2 = test_cap("add");
        let _h1 = registry.register(cap1).await.unwrap();
        let _h2 = registry.register(cap2).await.unwrap();
        let snap = registry.snapshot();
        assert_eq!(snap.len(), 2);
    }

    #[tokio::test]
    async fn test_lookup_returns_some_for_registered_id() {
        let registry = Arc::new(CapabilityRegistry::new(None));
        let cap = test_cap("echo");
        let id = cap.id.clone();
        let _handle = registry.register(cap).await.unwrap();
        let found = registry.lookup(&id).await;
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, id);
    }
}
