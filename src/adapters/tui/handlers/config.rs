//! Config reload handler — Story 8.1 AC-10.
//!
//! Handles `AppEvent::ConfigReload`: atomically swaps the new config into the
//! `ArcSwap<AppConfig>` via `ConfigStorePort` (domain-pure per AC-14).
//!
//! Returns `AppEvent::ConfigReloaded { success, error }` so telemetry
//! subscribers receive truthful outcome data (AC-15). The event_loop dispatch
//! arm additionally emits a `SystemNotice` for the status-bar flash (AC-12).
//!
//! The actual `config::try_load()` call lives in the `event_loop.rs` dispatch arm
//! so this handler does NOT import `crate::infrastructure::*`.

#![allow(dead_code)]

use crate::domain::errors::DomainError;
use crate::domain::events::AppEvent;
use crate::domain::models::AppConfig;
use crate::domain::ports::ConfigStorePort;

use super::HandlerOutcome;

/// Apply a config reload result: on success, swap into the ArcSwap and emit
/// ConfigReloaded { success: true }; on failure, preserve prior config and
/// emit ConfigReloaded { success: false, error: <reason> } per AC-11.
pub fn handle_config_reload(
    result: Result<AppConfig, DomainError>,
    config_store: &dyn ConfigStorePort,
) -> HandlerOutcome {
    match result {
        Ok(new_config) => {
            config_store.store(new_config);
            HandlerOutcome::Notify(AppEvent::ConfigReloaded {
                success: true,
                error: None,
            })
        }
        Err(e) => {
            tracing::error!("config reload failed: {:?}", e);
            HandlerOutcome::Notify(AppEvent::ConfigReloaded {
                success: false,
                error: Some(format!(
                    "Configuration reload failed — keeping previous config. Reason: {}",
                    short_reason(&e)
                )),
            })
        }
    }
}

/// Truncate a `DomainError` to ≤80 chars for the status-bar flash.
fn short_reason(e: &DomainError) -> String {
    let msg = e.to_string();
    if msg.len() <= 80 {
        msg
    } else {
        let truncate_at = msg.floor_char_boundary(77.min(msg.len()));
        format!("{}…", &msg[..truncate_at])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ports::ConfigStorePort;
    use std::sync::Arc;

    struct TestConfigStore {
        inner: std::sync::Mutex<Arc<AppConfig>>, // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: test-only, never across .await
    }

    impl ConfigStorePort for TestConfigStore {
        fn load(&self) -> Arc<AppConfig> {
            self.inner.lock().unwrap().clone() // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: test-only
        }
        fn store(&self, config: AppConfig) {
            *self.inner.lock().unwrap() = Arc::new(config); // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: test-only
        }
    }

    fn test_config() -> AppConfig {
        AppConfig::default()
    }

    #[test]
    fn reload_success_emits_config_reloaded_true() {
        let store = TestConfigStore {
            inner: std::sync::Mutex::new(Arc::new(test_config())),
        };
        let outcome = handle_config_reload(Ok(test_config()), &store);
        match outcome {
            HandlerOutcome::Notify(AppEvent::ConfigReloaded { success, error, .. }) => {
                assert!(success);
                assert!(error.is_none());
            }
            _other => panic!("expected Notify(ConfigReloaded {{ success: true }})"),
        }
    }

    #[test]
    fn reload_failure_emits_config_reloaded_false() {
        let store = TestConfigStore {
            inner: std::sync::Mutex::new(Arc::new(test_config())),
        };
        let err = DomainError::Config(crate::domain::errors::ConfigError::Missing(
            "test-field".to_string(),
        ));
        let outcome = handle_config_reload(Err(err), &store);
        match outcome {
            HandlerOutcome::Notify(AppEvent::ConfigReloaded { success, error, .. }) => {
                assert!(!success);
                assert!(error.unwrap().contains("keeping previous config"));
            }
            _other => panic!("expected Notify(ConfigReloaded {{ success: false }})"),
        }
    }

    #[test]
    fn short_reason_truncates_long_message() {
        let err = DomainError::Other("a".repeat(200));
        let reason = short_reason(&err);
        assert!(reason.len() <= 80);
        assert!(reason.ends_with('…'));
    }

    #[test]
    fn short_reason_preserves_short_message() {
        let err = DomainError::Other("short".to_string());
        let reason = short_reason(&err);
        assert_eq!(reason, "short");
    }
}
