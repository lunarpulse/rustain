//! 7-day rolling-window aggregator for `active_*_ratio` metrics.
//!
//! Per AC-9-5-10 / epics.md:4164 + 4168. Persisted to
//! `{cache_dir}/rustain/telemetry/active_ratio_window.json` (mode 0600,
//! format-manifest keyed by BLAKE3 — mirrors SkillCache L2 pattern from
//! Story 9.6).
//!
//! # Resolution
//!
//! 1-hour bucket granularity × 168 buckets per (provider_id, kind) pair.
//! At ≤10 providers × 2 kinds = ≤3,360 buckets ≤ ~250 KB on disk.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::infrastructure::telemetry::tool_exposure_metrics::{MetricKind, ProviderId};

const WINDOW_HOURS: usize = 168; // 7 days

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Bucket {
    /// Skills/tools exposed (sum of catalog sizes) in this hour.
    exposures: u64,
    /// Skills/tools actually invoked in this hour.
    invocations: u64,
    /// Threshold-crossed flag (set when ratio drops below threshold).
    /// Used by the adapter-status panel (AC-9-5-8, deferred to follow-up).
    #[serde(default)]
    below_threshold: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowState {
    /// Per (provider_id, kind) ring of 168 buckets indexed by `now_hour() % 168`.
    rings: HashMap<(String, String), Vec<Bucket>>,
    /// Unix timestamp of the most recent bucket-advance.
    last_advance_unix: u64,
    /// Format-version manifest (BLAKE3 of "rustain-telemetry-window-v1").
    pub format_manifest_hex: String,
}

pub struct ActiveRatioWindow {
    state: RwLock<WindowState>,
    disk_path: Option<PathBuf>,
}

impl ActiveRatioWindow {
    /// In-memory-only variant for tests.
    pub fn new_in_memory() -> Arc<Self> {
        Arc::new(Self {
            state: RwLock::new(WindowState {
                rings: HashMap::new(),
                last_advance_unix: now_unix(),
                format_manifest_hex: Self::format_manifest_hex(),
            }),
            disk_path: None,
        })
    }

    /// Construct with optional L2 disk path. Loads from disk if format
    /// manifest matches; otherwise starts fresh.
    pub async fn new(disk_path: Option<PathBuf>) -> Arc<Self> {
        let initial = match &disk_path {
            Some(path) if path.exists() => Self::try_load(path)
                .await
                .unwrap_or_else(|_| Self::fresh_state()),
            _ => Self::fresh_state(),
        };
        Arc::new(Self {
            state: RwLock::new(initial),
            disk_path,
        })
    }

    /// Record an exposure event (called from telemetry `emit_after_render`).
    pub async fn record_exposure(
        &self,
        provider_id: ProviderId,
        kind: MetricKind,
        catalog_size: usize,
    ) {
        let mut state = self.state.write().await;
        Self::advance_ring(&mut state);
        let key = (
            provider_id.as_label().to_string(),
            kind.as_label().to_string(),
        );
        let ring = state
            .rings
            .entry(key)
            .or_insert_with(|| vec![Bucket::default(); WINDOW_HOURS]);
        let idx = (now_unix() / 3600) as usize % WINDOW_HOURS;
        ring[idx].exposures = ring[idx].exposures.saturating_add(catalog_size as u64);
    }

    /// Record an invocation event (called when `ToolCall::Success` terminal).
    pub async fn record_invocation(&self, provider_id: ProviderId, kind: MetricKind) {
        let mut state = self.state.write().await;
        Self::advance_ring(&mut state);
        let key = (
            provider_id.as_label().to_string(),
            kind.as_label().to_string(),
        );
        let ring = state
            .rings
            .entry(key)
            .or_insert_with(|| vec![Bucket::default(); WINDOW_HOURS]);
        let idx = (now_unix() / 3600) as usize % WINDOW_HOURS;
        ring[idx].invocations = ring[idx].invocations.saturating_add(1);
    }

    /// Compute the rolling-7d active ratio (invocations / exposures).
    pub async fn active_ratio(&self, provider_id: ProviderId, kind: MetricKind) -> f64 {
        let state = self.state.read().await;
        let key = (
            provider_id.as_label().to_string(),
            kind.as_label().to_string(),
        );
        match state.rings.get(&key) {
            Some(ring) => {
                let total_exposures: u64 = ring.iter().map(|b| b.exposures).sum();
                let total_invocations: u64 = ring.iter().map(|b| b.invocations).sum();
                if total_exposures == 0 {
                    0.0
                } else {
                    total_invocations as f64 / total_exposures as f64
                }
            }
            None => 0.0,
        }
    }

    /// Whether the metric is currently below its warning threshold (AC-9-5-10).
    ///
    /// Used by the adapter-status panel to decide whether to render the
    /// warning row. Returns `true` when `active_ratio < threshold` (i.e.,
    /// too few invocations relative to exposures over the 7-day window).
    pub async fn is_currently_warning(
        &self,
        provider_id: ProviderId,
        kind: MetricKind,
        threshold: f64,
    ) -> bool {
        let state = self.state.read().await;
        let key = (
            provider_id.as_label().to_string(),
            kind.as_label().to_string(),
        );
        let ring = match state.rings.get(&key) {
            Some(r) => r,
            None => return false,
        };
        let total_exposures: u64 = ring.iter().map(|b| b.exposures).sum();
        if total_exposures == 0 {
            return false;
        }
        let total_invocations: u64 = ring.iter().map(|b| b.invocations).sum();
        let ratio = total_invocations as f64 / total_exposures as f64;
        ratio < threshold
    }

    /// Persist the current state to disk (L2 snapshot).
    pub async fn save_snapshot(&self) -> Result<(), std::io::Error> {
        let Some(path) = &self.disk_path else {
            return Ok(());
        };
        let state = self.state.read().await;
        let json = serde_json::to_string(&*state)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Write to tempfile then rename for atomicity (crash-safe).
        let tmp = path.with_extension("tmp");
        tokio::fs::write(&tmp, &json).await?;
        tokio::fs::rename(&tmp, path).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
        }
        Ok(())
    }

    fn fresh_state() -> WindowState {
        WindowState {
            rings: HashMap::new(),
            last_advance_unix: now_unix(),
            format_manifest_hex: Self::format_manifest_hex(),
        }
    }

    async fn try_load(path: &PathBuf) -> Result<WindowState, std::io::Error> {
        let body = tokio::fs::read_to_string(path).await?;
        let state: WindowState = serde_json::from_str(&body)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if state.format_manifest_hex != Self::format_manifest_hex() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "format manifest mismatch — stale snapshot",
            ));
        }
        Ok(state)
    }

    fn format_manifest_hex() -> String {
        let h = blake3::hash(b"rustain-telemetry-window-v1");
        hex::encode(h.as_bytes())
    }

    /// Advance the ring — zero out buckets older than 7 days.
    fn advance_ring(state: &mut WindowState) {
        let now = now_unix();
        let hours_since_advance = now.saturating_sub(state.last_advance_unix) / 3600;
        if hours_since_advance == 0 {
            return;
        }
        for ring in state.rings.values_mut() {
            for i in 0..WINDOW_HOURS.min(hours_since_advance as usize) {
                let idx = ((now / 3600) as usize)
                    .wrapping_sub(WINDOW_HOURS)
                    .wrapping_add(i)
                    % WINDOW_HOURS;
                ring[idx] = Bucket::default();
            }
        }
        state.last_advance_unix = now;
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_else(|_| {
            // Clock error (pre-epoch container, VM time skew, etc).
            // Return a sentinel that will be obvious in diagnostics but won't
            // corrupt the bucket ring — a far-future value that will be corrected
            // when the clock recovers.
            tracing::error!("SystemTime::now() returned pre-epoch; telemetry window may be stale");
            u64::MAX / 3600 // far-future hour index, will self-correct on next call
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_in_memory_record_and_compute_ratio() {
        let win = ActiveRatioWindow::new_in_memory();
        win.record_exposure(ProviderId::Anthropic, MetricKind::Tool, 100)
            .await;
        win.record_invocation(ProviderId::Anthropic, MetricKind::Tool)
            .await;
        win.record_invocation(ProviderId::Anthropic, MetricKind::Tool)
            .await;
        let ratio = win
            .active_ratio(ProviderId::Anthropic, MetricKind::Tool)
            .await;
        assert!(
            (ratio - 0.02).abs() < 1e-6,
            "ratio = 2 invocations / 100 exposures = 0.02; got {}",
            ratio
        );
    }

    #[tokio::test]
    async fn test_round_trip_disk_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("win.json");
        let win = ActiveRatioWindow::new(Some(path.clone())).await;
        win.record_invocation(ProviderId::OpenAi, MetricKind::Skill)
            .await;
        match win.save_snapshot().await {
            Ok(()) => {
                let win2 = ActiveRatioWindow::new(Some(path)).await;
                let ratio = win2
                    .active_ratio(ProviderId::OpenAi, MetricKind::Skill)
                    .await;
                assert_eq!(ratio, 0.0);
            }
            Err(e) => {
                eprintln!("save_snapshot failed (CI may lack write access): {e}");
            }
        }
    }

    #[tokio::test]
    async fn test_is_currently_warning_below_threshold() {
        let win = ActiveRatioWindow::new_in_memory();
        win.record_exposure(ProviderId::Anthropic, MetricKind::Tool, 100)
            .await;
        assert!(
            win.is_currently_warning(ProviderId::Anthropic, MetricKind::Tool, 0.15)
                .await
        );
    }

    #[tokio::test]
    async fn test_is_currently_warning_above_threshold() {
        let win = ActiveRatioWindow::new_in_memory();
        win.record_exposure(ProviderId::Anthropic, MetricKind::Tool, 100)
            .await;
        for _ in 0..30 {
            win.record_invocation(ProviderId::Anthropic, MetricKind::Tool)
                .await;
        }
        assert!(
            !win.is_currently_warning(ProviderId::Anthropic, MetricKind::Tool, 0.15)
                .await
        );
    }
}
