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

    #[test]
    fn budget_state_save_load_roundtrip() {
        // Keep the root alive until the isolated runtime-frontdoor scenario exits.
        // Command::env never changes the library test process's data directory.
        let tmp = tempfile::tempdir().expect("tempdir");
        let helper = format!(
            "{}::budget_state_subprocess_scenario",
            module_path!().split_once("::").unwrap().1
        );
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &helper, "--nocapture"])
            .env("RUSTAIN_BUDGET_STATE_TEST_DIR", tmp.path())
            .env("RUSTAIN_DATA_DIR", tmp.path())
            .output()
            .expect("run budget state subprocess");
        assert!(
            output.status.success(),
            "budget state subprocess failed: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            tmp.path().join("scenario.completed").is_file(),
            "budget state subprocess did not complete its assertions"
        );
    }

    #[tokio::test]
    async fn budget_state_subprocess_scenario() {
        let Some(root) =
            crate::infrastructure::utils::env_var_trimmed("RUSTAIN_BUDGET_STATE_TEST_DIR")
        else {
            return;
        };
        let root = PathBuf::from(root);
        // Validate isolation before invoking any path-resolving store operation.
        assert!(root.is_absolute() && root.is_dir());
        assert_eq!(
            crate::infrastructure::utils::env_var_trimmed("RUSTAIN_DATA_DIR")
                .expect("child data directory"),
            root.to_str().expect("UTF-8 test directory")
        );
        let path = root.join("budget_state.json");
        assert_eq!(
            crate::infrastructure::paths::budget_state_path().expect("path"),
            path
        );
        assert!(!path.exists());

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
        // Use the parent-owned path, never a newly resolved user-data path.
        std::fs::write(&path, "this is not json").expect("write garbage");
        let recovered = store.load().await;
        assert_eq!(recovered, BudgetState::default());
        // A successful exit alone could mean that --exact matched no tests.
        std::fs::write(root.join("scenario.completed"), b"complete")
            .expect("record completed budget scenario");
    }
}
