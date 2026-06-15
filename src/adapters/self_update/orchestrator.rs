//! Self-update orchestration (Story 13.3a, AC1–AC11).
//!
//! `run_check`  — non-destructive "is an update available?" (always exits 0).
//! `run_update` — download → verify → atomic-replace flow.

use std::io::{BufRead, Write};
use std::process::ExitCode;

use semver::Version;

use super::trust::TRUSTED_KEYS;
use super::types::{CheckReport, UpdateError};
use crate::domain::ports::self_update::{BinaryReplacerPort, SelfUpdatePort};

// ──────────────────────────────────────────────────────────────────
// run_check (AC6)
// ──────────────────────────────────────────────────────────────────

/// `--check`: reports update availability WITHOUT downloading, verifying, or replacing.
/// Returns `ExitCode::SUCCESS` unconditionally — errors are surfaced as informational text.
pub async fn run_check(output_format: &str) -> ExitCode {
    let current_str = env!("CARGO_PKG_VERSION");

    let report = match super::client::GithubReleaseClient::new() {
        Ok(client) => run_check_with(&client, current_str).await,
        Err(_) => CheckReport {
            schema_version: "1.0".to_string(),
            current: current_str.to_string(),
            latest: None,
            update_available: false,
            status_line: "Could not check for updates (offline).".to_string(),
        },
    };

    match output_format {
        "json" => {
            if let Ok(json) = serde_json::to_string_pretty(&report) {
                println!("{json}");
            }
        }
        _ => println!("{report}"),
    }

    ExitCode::SUCCESS
}

/// Testable inner: builds a `CheckReport` using an injected client.
/// Always returns a report — offline/error arms produce informational reports.
async fn run_check_with(client: &dyn SelfUpdatePort, current_str: &str) -> CheckReport {
    let release = match client.latest_release().await {
        Ok(r) => r,
        Err(UpdateError::Offline(_)) => {
            return CheckReport {
                schema_version: "1.0".to_string(),
                current: current_str.to_string(),
                latest: None,
                update_available: false,
                status_line: "Could not check for updates (offline).".to_string(),
            };
        }
        Err(e) => {
            return CheckReport {
                schema_version: "1.0".to_string(),
                current: current_str.to_string(),
                latest: None,
                update_available: false,
                status_line: format!("Could not check for updates ({e})."),
            };
        }
    };

    let current = Version::parse(current_str).expect("CARGO_PKG_VERSION is valid semver");
    let latest = match Version::parse(&release.version) {
        Ok(v) => v,
        Err(e) => {
            return CheckReport {
                schema_version: "1.0".to_string(),
                current: current_str.to_string(),
                latest: Some(release.version),
                update_available: false,
                status_line: format!("Could not parse latest version ({e})."),
            };
        }
    };

    let update_available = latest > current;
    let status_line = if update_available {
        format!("Current: v{current} → Latest: v{latest}. Run 'rustain update' to upgrade.")
    } else {
        format!("Already at latest version (v{current}).")
    };

    CheckReport {
        schema_version: "1.0".to_string(),
        current: current_str.to_string(),
        latest: Some(latest.to_string()),
        update_available,
        status_line,
    }
}

// ──────────────────────────────────────────────────────────────────
// run_update (AC1–AC10)
// ──────────────────────────────────────────────────────────────────

/// Full update flow: platform check → download → verify → atomic replace.
pub async fn run_update() -> Result<(), UpdateError> {
    let client = super::client::GithubReleaseClient::new()
        .map_err(|e| UpdateError::Offline(format!("{e}")))?;
    run_update_with(&client).await
}

/// Testable inner: full update flow with an injected client.
async fn run_update_with(client: &dyn SelfUpdatePort) -> Result<(), UpdateError> {
    let current_str = env!("CARGO_PKG_VERSION");
    let current = Version::parse(current_str).expect("CARGO_PKG_VERSION is valid semver");
    let target = env!("TARGET");

    // Step 1 — writable check (AC10): ensure we can write next to the running binary.
    check_writable()?;

    // Step 2 — fetch latest release metadata.
    let release = client.latest_release().await?;

    // Step 3 — semver comparison (AC1, AC8).
    let latest = Version::parse(&release.version)
        .map_err(|e| UpdateError::Other(format!("bad remote version: {e}")))?;

    if latest == current {
        println!("Already at latest version (v{current}).");
        return Ok(());
    }
    if latest < current {
        return Err(UpdateError::DowngradeRefused {
            current: current.to_string(),
            latest: latest.to_string(),
        });
    }

    // Step 4 — construct expected asset name and check it exists (AC9).
    let binary_name = expected_asset_name(&latest, target);
    let binary_asset = release
        .assets
        .iter()
        .find(|a| a.name == binary_name)
        .ok_or_else(|| UpdateError::PlatformNotSupported(target.to_string()))?;
    let sums_asset = release
        .assets
        .iter()
        .find(|a| a.name == "SHA256SUMS")
        .ok_or_else(|| UpdateError::Other("Release missing SHA256SUMS asset".to_string()))?;
    let sig_asset = release
        .assets
        .iter()
        .find(|a| a.name == "SHA256SUMS.minisig")
        .ok_or_else(|| {
            UpdateError::Other("Release missing SHA256SUMS.minisig asset".to_string())
        })?;

    // Step 5 — confirm with user.
    let notes_preview: String = release.notes.lines().take(5).collect::<Vec<_>>().join("\n");
    println!("Current: v{current} → Latest: v{latest}");
    if !notes_preview.is_empty() {
        println!("\nRelease notes (first 5 lines):\n{notes_preview}\n");
    }
    print!("Update? [y/n] ");
    std::io::stdout()
        .flush()
        .map_err(|e| UpdateError::Other(e.to_string()))?;

    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|e| UpdateError::Other(e.to_string()))?;
    if !answer.trim().eq_ignore_ascii_case("y") {
        return Ok(());
    }

    // Step 6 — download all three assets.
    let (binary_bytes, manifest_text, sig_text) = tokio::try_join!(
        client.download_asset(binary_asset),
        client.download_text_asset(sums_asset),
        client.download_text_asset(sig_asset),
    )?;

    // Step 7 — verify (AC2). Abort before any filesystem mutation.
    super::verify::verify_release(
        manifest_text.as_bytes(),
        sig_text.as_bytes(),
        &binary_bytes,
        &binary_name,
        TRUSTED_KEYS,
    )?;

    // Step 8 — acquire lock + backup + atomic replace (AC4).
    let replacer = super::replacer::DefaultBinaryReplacer::new()?;
    let backup_path = replacer.backup_current().await?;

    if let Err(replace_err) = replacer.atomic_replace(&binary_bytes).await {
        // Attempt rollback before surfacing the error.
        if let Err(restore_err) = replacer.restore(&backup_path).await {
            eprintln!(
                "⚠ Rollback also failed: {restore_err}\n  A backup of the previous binary is at: {}",
                backup_path.display()
            );
        }
        return Err(replace_err);
    }

    // Step 9 — success (AC3).
    println!("✓ Updated: v{current} → v{latest}");
    Ok(())
}

// ──────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────

/// Build the expected release asset filename for `target`.
fn expected_asset_name(version: &Version, target: &str) -> String {
    let base = format!("rustain-{version}-{target}");
    if target.contains("windows") {
        format!("{base}.exe")
    } else {
        base
    }
}

/// AC10: verify the install directory is user-writable.
fn check_writable() -> Result<(), UpdateError> {
    let exe = std::env::current_exe()
        .map_err(|e| UpdateError::ManagedInstall(format!("cannot locate self: {e}")))?;
    let dir = exe
        .parent()
        .ok_or_else(|| UpdateError::ManagedInstall("binary has no parent directory".to_string()))?;
    // Probe by creating (and immediately removing) a temp file.
    let probe = dir.join(".rustain-update-probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            Err(UpdateError::ManagedInstall(format!(
                "Install path is not user-writable (managed externally): {}",
                dir.display()
            )))
        }
        Err(e) => Err(UpdateError::ManagedInstall(format!(
            "cannot probe write access to {}: {e}",
            dir.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::self_update::types::{ReleaseAsset, ReleaseInfo, UpdateError};
    use crate::domain::ports::self_update::SelfUpdatePort;
    use async_trait::async_trait;
    use semver::Version;

    /// Fake client whose `latest_release` never touches the network. The hermetic
    /// tests below all abort before download, so those methods are unreachable.
    struct FakeClient {
        version: Option<String>, // None => offline
        assets: Vec<ReleaseAsset>,
    }
    #[async_trait]
    impl SelfUpdatePort for FakeClient {
        async fn latest_release(&self) -> Result<ReleaseInfo, UpdateError> {
            match &self.version {
                None => Err(UpdateError::Offline("fake offline".into())),
                Some(v) => Ok(ReleaseInfo {
                    version: v.clone(),
                    notes: String::new(),
                    assets: self.assets.clone(),
                }),
            }
        }
        async fn download_asset(&self, _: &ReleaseAsset) -> Result<Vec<u8>, UpdateError> {
            unreachable!("hermetic tests abort before download")
        }
        async fn download_text_asset(&self, _: &ReleaseAsset) -> Result<String, UpdateError> {
            unreachable!("hermetic tests abort before download")
        }
    }

    fn current() -> &'static str {
        env!("CARGO_PKG_VERSION")
    }
    fn newer() -> String {
        let mut v = Version::parse(current()).unwrap();
        v.minor += 1;
        v.to_string()
    }
    fn older() -> String {
        let v = Version::parse(current()).unwrap();
        if v.minor > 0 {
            format!("{}.{}.{}", v.major, v.minor - 1, v.patch)
        } else {
            format!("{}.0.0", v.major.saturating_sub(1))
        }
    }

    // P0 #2: run_check exit-0 path — up-to-date
    #[tokio::test]
    async fn check_up_to_date_reports_not_available() {
        let client = FakeClient {
            version: Some(current().to_string()),
            assets: vec![],
        };
        let report = run_check_with(&client, current()).await;
        assert!(
            !report.update_available,
            "up-to-date must report update_available=false"
        );
        assert_eq!(report.latest, Some(current().to_string()));
    }

    // P0 #2: available
    #[tokio::test]
    async fn check_available_reports_available() {
        let client = FakeClient {
            version: Some(newer()),
            assets: vec![],
        };
        let report = run_check_with(&client, current()).await;
        assert!(
            report.update_available,
            "newer must report update_available=true"
        );
    }

    // P0 #2/#3: offline — latest:null, update_available:false (proves the AC6 shape after the types.rs patch)
    #[tokio::test]
    async fn check_offline_emits_null_latest() {
        let client = FakeClient {
            version: None,
            assets: vec![],
        };
        let report = run_check_with(&client, current()).await;
        assert!(!report.update_available);
        assert!(report.latest.is_none());
        let json = serde_json::to_string(&report).unwrap();
        assert!(
            json.contains("\"latest\":null"),
            "offline JSON must emit \"latest\":null — got: {json}"
        );
    }

    // P0 #2 PAIRED NEGATIVE: run_update on the SAME offline → non-zero (proves the AC5/AC6 divergence is real)
    #[tokio::test]
    async fn update_offline_is_error() {
        let client = FakeClient {
            version: None,
            assets: vec![],
        };
        let result = run_update_with(&client).await;
        assert!(
            result.is_err(),
            "run_update on offline MUST error (AC5), got {result:?}"
        );
    }

    // P0 #8: correctly-signed OLDER release is refused (no replace)
    #[tokio::test]
    async fn update_refuses_signed_downgrade() {
        let client = FakeClient {
            version: Some(older()),
            assets: vec![],
        };
        let result = run_update_with(&client).await;
        assert!(
            matches!(result, Err(UpdateError::DowngradeRefused { .. })),
            "older release must be refused (AC8), got {result:?}"
        );
    }

    // AC9/P0 #12: no matching asset for the running target → hard-error BEFORE download
    #[tokio::test]
    async fn update_refuses_unsupported_platform_before_download() {
        let client = FakeClient {
            version: Some(newer()),
            assets: vec![],
        };
        let result = run_update_with(&client).await;
        assert!(
            matches!(result, Err(UpdateError::PlatformNotSupported(_))),
            "missing target asset must hard-error (AC9), got {result:?}"
        );
    }
}
