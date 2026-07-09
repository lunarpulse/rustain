//! File-backed budget pause-state store (Story 7.5 AC7).
//!
//! Persists a single JSON line to `~/.rustain/budget_state.json` recording
//! when the daily-budget warning is dismissed-until. The store is stateless:
//! each call re-resolves the path and reads/writes the file.
//!
//! Uses `anyhow::Result` at the infrastructure boundary instead of adding a
//! new `StorageError` variant (enum-stability rule carried from S7.1a).

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Persisted budget pause-state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetState {
    /// Unix timestamp (seconds) until which the daily-budget warning is
    /// suppressed. `0` means no active dismiss (the default).
    pub dismissed_until_unix: i64,
}

/// Stateless file-backed budget-state store.
pub struct BudgetStateStore;

impl BudgetStateStore {
    pub fn new() -> Self {
        Self
    }

    fn path() -> Result<PathBuf> {
        crate::infrastructure::paths::budget_state_path()
    }

    /// Load the persisted state. Returns `BudgetState::default()` (i.e.
    /// `dismissed_until_unix: 0`) when the file is missing or unparseable —
    /// per AC7 the daily-budget warning is non-critical, so corrupt state
    /// is recovered to "no dismiss" rather than surfaced as an error.
    pub async fn load(&self) -> BudgetState {
        let Ok(path) = Self::path() else {
            tracing::debug!("budget_state path unresolvable; using default");
            return BudgetState::default();
        };

        match tokio::fs::read_to_string(&path).await {
            Ok(contents) => match serde_json::from_str::<BudgetState>(contents.trim()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("budget_state.json unparseable; using default: {e}");
                    BudgetState::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!("budget_state.json missing; using default");
                BudgetState::default()
            }
            Err(e) => {
                tracing::warn!("budget_state.json read failed; using default: {e}");
                BudgetState::default()
            }
        }
    }

    /// Persist the state. Returns an error on IO failure so the caller can
    /// surface a `SystemNotice` warning to the user (AC7).
    pub async fn save(&self, state: &BudgetState) -> Result<()> {
        let path = Self::path()?;
        let line = serde_json::to_string(state).context("serialize BudgetState")?;
        tokio::fs::write(&path, line)
            .await
            .with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }
}

impl Default for BudgetStateStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[serial_test::serial(rustain_data_dir)]
    async fn budget_state_save_load_roundtrip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let original = std::env::var("RUSTAIN_DATA_DIR").ok(); // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
        unsafe {
            std::env::set_var("RUSTAIN_DATA_DIR", tmp.path().as_os_str()); // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
        }

        let store = BudgetStateStore::new();

        // Missing file → default
        let loaded = store.load().await;
        assert_eq!(loaded, BudgetState::default());
        assert_eq!(loaded.dismissed_until_unix, 0);

        // Save + load round-trip
        let want = BudgetState {
            dismissed_until_unix: 1_747_238_400,
        };
        store.save(&want).await.expect("save");
        let got = store.load().await;
        assert_eq!(got, want);

        // Corrupt file → graceful default
        let path = crate::infrastructure::paths::budget_state_path().expect("path");
        std::fs::write(&path, "this is not json").expect("write garbage");
        let recovered = store.load().await;
        assert_eq!(recovered, BudgetState::default());

        match original {
            Some(v) => unsafe { std::env::set_var("RUSTAIN_DATA_DIR", v) }, // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
            None => unsafe { std::env::remove_var("RUSTAIN_DATA_DIR") }, // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
        }
    }
}
