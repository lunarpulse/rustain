//! D-24 B: a writable `ask` reclaims abandoned session-write scratch before it loads or
//! pre-saves anything; `ask --dry-run` takes no session write path and never sweeps.
//!
//! Real `run_ask_core` over a real `FileSystemStorage` in a TempDir, scripted provider.

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::{Mutex, OnceLock};

use async_trait::async_trait;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::domain::errors::StorageError;
use rustain::domain::models::{Conversation, ConversationSummary};
use rustain::domain::ports::{StoragePort, StreamingProvider};
use tempfile::TempDir;

use super::d24_session_store::{Scripted, run, seeded, store_in, tree};
use super::{make_dry_run_opts, make_opts};

fn scripted(ask_for_tool: bool) -> Arc<dyn StreamingProvider> {
    Arc::new(Scripted {
        calls: AtomicUsize::new(0),
        ask_for_tool,
        competitor: None,
    })
}

fn sessions(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join(".claude").join("sessions")
}

/// An orphan a dead writer left: a pid nobody holds a scratch guard for.
fn plant_orphan(root: &std::path::Path, n: u32) -> std::path::PathBuf {
    std::fs::create_dir_all(root).unwrap();
    let orphan = root.join(format!(".rustain-424242-{n}.tmp"));
    std::fs::write(&orphan, b"abandoned").unwrap();
    orphan
}

/// Forwards to the real store and records whether the planted orphan still existed at the first
/// session lookup and at the first save (the producer's pre-save).
struct Probe {
    inner: Arc<FileSystemStorage>,
    orphan: std::path::PathBuf,
    at_first_load: OnceLock<bool>,
    at_first_save: OnceLock<bool>,
    saves: Mutex<usize>,
}

#[async_trait]
impl StoragePort for Probe {
    async fn load_conversation(&self, id: &str) -> Result<Option<Conversation>, StorageError> {
        let _ = self.at_first_load.set(self.orphan.exists());
        self.inner.load_conversation(id).await
    }
    async fn save_conversation(&self, conv: &Conversation) -> Result<(), StorageError> {
        let _ = self.at_first_save.set(self.orphan.exists());
        *self.saves.lock().unwrap() += 1;
        self.inner.save_conversation(conv).await
    }
    async fn list_conversations(&self) -> Result<Vec<ConversationSummary>, StorageError> {
        self.inner.list_conversations().await
    }
    async fn read_snapshot(
        &self,
        id: &str,
        checkpoint: rustain::domain::models::checkpoint::CheckpointId,
        path: &std::path::Path,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        self.inner.read_snapshot(id, checkpoint, path).await
    }
    async fn sweep_session_scratch(&self) {
        self.inner.sweep_session_scratch().await
    }
}

#[tokio::test]
async fn a_fresh_writable_ask_sweeps_before_its_pre_save() {
    let dir = TempDir::new().unwrap();
    let orphan = plant_orphan(&sessions(&dir), 1);
    let probe = Arc::new(Probe {
        inner: store_in(&dir),
        orphan: orphan.clone(),
        at_first_load: OnceLock::new(),
        at_first_save: OnceLock::new(),
        saves: Mutex::new(0),
    });

    let (exit, _, err) = run(
        scripted(true),
        probe.clone(),
        make_opts("hello"),
        dir.path(),
    )
    .await;

    assert_eq!(exit, ExitCode::SUCCESS, "stderr: {err}");
    assert_eq!(
        probe.at_first_save.get(),
        Some(&false),
        "the orphan was gone before the first (pre-)save"
    );
    assert!(*probe.saves.lock().unwrap() >= 1, "the run really saved");
    assert!(!orphan.exists());
}

#[tokio::test]
async fn a_resumed_writable_ask_sweeps_before_it_loads_or_saves() {
    let dir = TempDir::new().unwrap();
    let store = store_in(&dir);
    seeded(&store, "resume-me").await;
    let orphan = plant_orphan(&sessions(&dir), 2);
    let dir_orphan = plant_orphan(&sessions(&dir).join("resume-me"), 3);
    let probe = Arc::new(Probe {
        inner: store,
        orphan: orphan.clone(),
        at_first_load: OnceLock::new(),
        at_first_save: OnceLock::new(),
        saves: Mutex::new(0),
    });
    let mut opts = make_opts("continue");
    opts.session_id = Some("resume-me".into());

    let (exit, _, err) = run(scripted(false), probe.clone(), opts, dir.path()).await;

    assert_eq!(exit, ExitCode::SUCCESS, "stderr: {err}");
    assert_eq!(
        probe.at_first_load.get(),
        Some(&false),
        "swept before the load"
    );
    assert_eq!(
        probe.at_first_save.get(),
        Some(&false),
        "and before the save"
    );
    assert!(!orphan.exists() && !dir_orphan.exists());
}

#[tokio::test]
async fn a_dry_run_never_sweeps_and_changes_no_byte() {
    let dir = TempDir::new().unwrap();
    let store = store_in(&dir);
    seeded(&store, "resume-me").await;
    let orphan = plant_orphan(&sessions(&dir), 4);
    let dir_orphan = plant_orphan(&sessions(&dir).join("resume-me"), 5);
    let before = tree(&sessions(&dir));

    let mut opts = make_dry_run_opts("plan it", rustain::adapters::cli::ask::OutputFormat::Text);
    opts.session_id = Some("resume-me".into());
    let (exit, _, err) = run(scripted(true), store, opts, dir.path()).await;

    assert_eq!(exit, ExitCode::SUCCESS, "stderr: {err}");
    assert!(
        orphan.exists() && dir_orphan.exists(),
        "a preview does not sweep"
    );
    assert_eq!(tree(&sessions(&dir)), before, "the whole tree is unchanged");
}

#[tokio::test]
async fn an_ask_with_an_empty_query_fails_before_any_root_is_touched() {
    let dir = TempDir::new().unwrap();
    let orphan = plant_orphan(&sessions(&dir), 6);
    let before = tree(&sessions(&dir));
    let (exit, _, _) = run(
        scripted(false),
        store_in(&dir),
        make_opts("   "),
        dir.path(),
    )
    .await;
    assert_ne!(exit, ExitCode::SUCCESS);
    assert!(orphan.exists());
    assert_eq!(tree(&sessions(&dir)), before);
}
