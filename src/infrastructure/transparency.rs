//! Transparency read seam and `transparency.jsonl` export (Story 18.2, AC3/AC7).
//!
//! The effect shell around
//! [`crate::domain::services::transparency`]: it reads the durable room
//! journal through the [`RoomJournalReader`] port, structurally validates it,
//! folds it with the domain core, and — only when asked — renders the result to
//! disk.
//!
//! # Regenerable, never a second source of truth
//!
//! The export is written **whole** on every run: render to a private temporary
//! file, `fsync`, atomically replace the destination, then sync its parent
//! directory. Nothing appends to it and nothing in the product reads it back.
//! Delete it, replace it, or corrupt it — re-running the export reproduces the
//! previous good bytes exactly, because every fact in it came from the journal
//! (ADR-17-CC-05).
//!
//! # Not live
//!
//! [`RoomJournalReader::load_entries`] is a point-in-time read under a shared
//! `flock`. It is consistent, not a subscription: the daemon can append between
//! two calls. Any surface built on this must say "as of" and never "live".

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::domain::ports::{RoomJournalError, RoomJournalReader};
use crate::domain::services::transparency::{
    TransparencyExport, TransparencyReport, fold_transparency, render_export,
    validate_replay_structure,
};

#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum TransparencyExportError {
    #[error("writing the transparency export: {0}")]
    Io(#[from] std::io::Error),
    #[error("resolving the transparency export path: {0}")]
    Path(String),
}

/// Read seam + export shell. Constructed at the composition root and hung on
/// `AppState`; the TUI never holds a concrete `NodeJournal`.
pub struct TransparencyService {
    reader: Arc<dyn RoomJournalReader>,
    workspace: PathBuf,
}

impl TransparencyService {
    #[must_use]
    pub fn new(reader: Arc<dyn RoomJournalReader>, workspace: PathBuf) -> Self {
        Self { reader, workspace }
    }

    /// Where an export would be written.
    ///
    /// # Errors
    ///
    /// Fails when `{workspace}/.rustain/` cannot be created.
    pub fn export_path(&self) -> Result<PathBuf, TransparencyExportError> {
        crate::infrastructure::paths::transparency_export_path(&self.workspace)
            .map_err(|error| TransparencyExportError::Path(error.to_string()))
    }

    /// Read the journal and project it. **Not live** — see the module doc.
    ///
    /// # Errors
    ///
    /// Propagates a journal read failure. A viewer shows the error rather than
    /// rendering a partial fold as if it were the whole log.
    pub async fn report(&self) -> Result<TransparencyReport, RoomJournalError> {
        let entries = self.reader.load_entries().await?;
        Ok(TransparencyReport {
            findings: validate_replay_structure(&entries),
            rows: fold_transparency(&entries),
        })
    }

    /// Render one previously-read report to
    /// `{workspace}/.rustain/transparency.jsonl`.
    ///
    /// The caller owns the point-in-time report. This method deliberately does
    /// not read again, so the rows displayed beside an export and the exported
    /// bytes describe the same journal snapshot even while the daemon appends.
    ///
    /// The replacement is a private temporary regular file in `.rustain`:
    /// fully written and synced, atomically renamed over the destination, then
    /// followed by a parent-directory sync. `rename` replaces a destination
    /// symlink itself rather than following it.
    ///
    /// # Errors
    ///
    /// Propagates file-write failures.
    pub async fn export_report(
        &self,
        report: &TransparencyReport,
    ) -> Result<TransparencyExport, TransparencyExportError> {
        let path = self.export_path()?;
        let body = render_export(&report.rows);
        let write_path = path.clone();
        tokio::task::spawn_blocking(move || write_export(&write_path, body.as_bytes()))
            .await
            .expect("transparency export task panicked")?;
        Ok(TransparencyExport {
            path,
            rows: report.rows.len(),
            findings: report.findings.clone(),
        })
    }
}

fn write_export(path: &Path, body: &[u8]) -> Result<(), std::io::Error> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "transparency export path has no parent directory",
        )
    })?;
    let mut builder = tempfile::Builder::new();
    builder
        .prefix(".transparency-")
        .suffix(".tmp")
        .rand_bytes(16);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o600));
    }
    let mut temporary = builder.tempfile_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    temporary.write_all(body)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::domain::models::{
        Direction, JournalEntry, JournalRecord, PeerId, RejectReason, RoomEvent,
    };

    struct Fixed(Vec<JournalEntry>);

    #[async_trait::async_trait]
    impl RoomJournalReader for Fixed {
        async fn load_entries(&self) -> Result<Vec<JournalEntry>, RoomJournalError> {
            Ok(self.0.clone())
        }
    }

    struct Changing {
        reads: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl RoomJournalReader for Changing {
        async fn load_entries(&self) -> Result<Vec<JournalEntry>, RoomJournalError> {
            match self.reads.fetch_add(1, Ordering::Relaxed) {
                0 => Ok(vec![rejection(1, Direction::Inbound)]),
                _ => Ok(vec![
                    rejection(1, Direction::Inbound),
                    rejection(2, Direction::Outbound),
                ]),
            }
        }
    }

    fn rejection(seq: u64, direction: Direction) -> JournalEntry {
        JournalEntry::new(
            seq,
            JournalRecord::Room(RoomEvent::RemoteEnvelopeRejected {
                peer: PeerId::from_public_key(&[seq as u8; 32]).unwrap(),
                reason: RejectReason::Policy {
                    detail: "policy".to_owned(),
                },
                direction,
                task: None,
            }),
            1_700_000_000_000 + seq as i64,
        )
    }

    #[tokio::test]
    async fn export_is_regenerable_byte_for_byte_after_corruption() {
        let workspace = tempfile::tempdir().unwrap();
        let service = TransparencyService::new(
            Arc::new(Fixed(vec![
                rejection(1, Direction::Inbound),
                rejection(2, Direction::Outbound),
            ])),
            workspace.path().to_path_buf(),
        );

        let report = service.report().await.unwrap();
        let first = service.export_report(&report).await.unwrap();
        assert_eq!(first.rows, 2, "the fixture must actually produce rows");
        let good = std::fs::read(&first.path).unwrap();
        assert_eq!(good.iter().filter(|b| **b == b'\n').count(), 2);

        // Corrupt, then delete: both recover to the same bytes because the
        // file holds no fact that is not in the journal.
        std::fs::write(&first.path, b"garbage\n").unwrap();
        service.export_report(&report).await.unwrap();
        assert_eq!(std::fs::read(&first.path).unwrap(), good);

        std::fs::remove_file(&first.path).unwrap();
        service.export_report(&report).await.unwrap();
        assert_eq!(std::fs::read(&first.path).unwrap(), good);
    }

    #[tokio::test]
    async fn a_structurally_diverged_journal_is_reported_without_overclaiming() {
        let workspace = tempfile::tempdir().unwrap();
        // seq 1 then 3: a line was removed from the middle.
        let service = TransparencyService::new(
            Arc::new(Fixed(vec![
                rejection(1, Direction::Inbound),
                rejection(3, Direction::Inbound),
            ])),
            workspace.path().to_path_buf(),
        );
        let report = service.report().await.unwrap();
        assert!(!report.is_structurally_consistent());
        assert!(report.structural_divergence_report().is_some());
        let export = service.export_report(&report).await.unwrap();
        assert!(!export.findings.is_empty());
    }

    #[tokio::test]
    async fn export_uses_the_report_snapshot_without_a_second_journal_read() {
        let workspace = tempfile::tempdir().unwrap();
        let reader = Arc::new(Changing {
            reads: AtomicUsize::new(0),
        });
        let service = TransparencyService::new(reader.clone(), workspace.path().to_path_buf());

        let report = service.report().await.unwrap();
        let export = service.export_report(&report).await.unwrap();

        assert_eq!(reader.reads.load(Ordering::Relaxed), 1);
        assert_eq!(report.rows.len(), 1);
        assert_eq!(export.rows, 1);
        assert_eq!(
            std::fs::read(&export.path)
                .unwrap()
                .iter()
                .filter(|byte| **byte == b'\n')
                .count(),
            1,
            "the export must describe the supplied snapshot, not a later read"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn export_replaces_a_world_readable_destination_with_owner_only_mode() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = tempfile::tempdir().unwrap();
        let service = TransparencyService::new(
            Arc::new(Fixed(vec![rejection(1, Direction::Inbound)])),
            workspace.path().to_path_buf(),
        );
        let path = service.export_path().unwrap();
        std::fs::write(&path, b"old export").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let report = service.report().await.unwrap();
        let export = service.export_report(&report).await.unwrap();

        assert_eq!(export.rows, report.rows.len());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn export_replaces_a_destination_symlink_without_touching_its_target() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let workspace = tempfile::tempdir().unwrap();
        let service = TransparencyService::new(
            Arc::new(Fixed(vec![rejection(1, Direction::Inbound)])),
            workspace.path().to_path_buf(),
        );
        let path = service.export_path().unwrap();
        let target = workspace.path().join("must-stay-unchanged");
        std::fs::write(&target, b"outside destination").unwrap();
        symlink(&target, &path).unwrap();

        let report = service.report().await.unwrap();
        service.export_report(&report).await.unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"outside destination");
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        assert!(metadata.file_type().is_file());
        assert!(!metadata.file_type().is_symlink());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    }
}
