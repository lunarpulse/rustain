//! D-24 keystones at the real filesystem port: capture-time fencing, truncated/reverted/
//! migrated transactions, crash and partial outcomes, journal authority and two-process
//! exclusion. Every case drives `FileSystemStorage` over a `TempDir`; faults and pauses come from
//! the commit's own points (`session_authority::fault`), never from a permissive double.

use super::session_authority::{self as authority, Opened, fault};
use super::*;
use crate::domain::models::checkpoint::CheckpointId;
use crate::domain::models::{ChatMessage, MessageRole};
use crate::domain::ports::StoragePort;
use std::io::{BufRead, Write};
use tempfile::TempDir;

fn row(id: &str, content: &str) -> ChatMessage {
    ChatMessage {
        id: id.into(),
        role: if id.starts_with('a') {
            MessageRole::Assistant
        } else {
            MessageRole::User
        },
        content: content.into(),
        ..Default::default()
    }
}

fn conv(id: &str, rows: &[(&str, &str)]) -> Conversation {
    let mut c = Conversation {
        id: id.into(),
        title: "t".into(),
        created_at: 1,
        updated_at: 1,
        ..Default::default()
    };
    c.messages = rows.iter().map(|(i, t)| row(i, t)).collect();
    c
}

fn store(dir: &TempDir) -> FileSystemStorage {
    FileSystemStorage::with_workspace_root(dir.path().join("sessions"), dir.path().to_path_buf())
}

fn sessions(dir: &TempDir) -> PathBuf {
    dir.path().join("sessions")
}

async fn record(dir: &TempDir, id: &str) -> Option<session_authority::Record> {
    authority::acquire_existing(&sessions(dir), id)
        .await
        .unwrap()
        .and_then(|h| h.live().cloned())
}

async fn version(dir: &TempDir, id: &str) -> u64 {
    record(dir, id).await.map_or(0, |r| r.version)
}

fn ids(c: &Conversation) -> Vec<String> {
    c.messages.iter().map(|m| m.id.clone()).collect()
}

fn stale<T: std::fmt::Debug>(r: Result<T, StorageError>) -> bool {
    matches!(r, Err(StorageError::Stale(_)))
}

/// The record as the file holds it, without taking the authority (usable while an operation
/// is parked inside it).
fn peek_version(dir: &TempDir, id: &str) -> u64 {
    let path = sessions(dir)
        .join(authority_dir())
        .join(format!("{id}.auth.json"));
    serde_json::from_slice::<session_authority::Record>(&std::fs::read(path).unwrap())
        .unwrap()
        .version
}

fn authority_dir() -> &'static str {
    session_authority::AUTHORITY_DIR
}

/// Every file under `root` with its digest: the "nothing was written" oracle. The capture
/// sequence counter (`*.seq`) is excluded: a refused capture still spends a sequence number,
/// which is a harmless gap, not a committed watermark.
fn tree(root: &Path) -> Vec<(String, String)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else if p.extension().is_none_or(|ext| ext != "seq") {
                let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
                out.push((rel, authority::digest(&std::fs::read(&p).unwrap())));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

// ── AC1: capture, base and sequence ─────────────────────────────────────────────

#[tokio::test]
async fn a_conversation_advances_its_own_chain_and_each_save_is_one_version() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let mut c = conv("s1", &[("u1", "hello")]);
    s.save_conversation(&c).await.unwrap();
    assert_eq!(version(&dir, "s1").await, 1);
    c.messages.push(row("a1", "hi"));
    s.save_conversation(&c).await.unwrap();
    assert_eq!(version(&dir, "s1").await, 2);
    assert_eq!(
        ids(&s.load_conversation("s1").await.unwrap().unwrap()),
        ["u1", "a1"]
    );
}

#[tokio::test]
async fn an_older_base_is_refused_even_when_captured_after_the_competing_commit() {
    let dir = TempDir::new().unwrap();
    let (a_store, b_store) = (store(&dir), store(&dir));
    a_store
        .save_conversation(&conv("s1", &[("u1", "base")]))
        .await
        .unwrap();
    let mut a = a_store.load_conversation("s1").await.unwrap().unwrap();
    let mut b = b_store.load_conversation("s1").await.unwrap().unwrap();
    b.messages.push(row("a-b", "from B"));
    b_store.save_conversation(&b).await.unwrap();
    // A retained its loaded base: a capture made now (newest sequence) still rests on it.
    a.messages.push(row("a-a", "from A"));
    let late = a.capture();
    assert!(stale(a_store.save_conversation(&late).await));
    assert!(stale(a_store.save_conversation(&a).await));
    let disk = a_store.load_conversation("s1").await.unwrap().unwrap();
    assert_eq!(ids(&disk), ["u1", "a-b"], "the refused saves wrote nothing");
    assert_eq!(
        version(&dir, "s1").await,
        2,
        "a refusal does not advance the version"
    );
    // A genuinely reloads and commits; B is then on an older base until it reloads.
    let mut a2 = a_store.load_conversation("s1").await.unwrap().unwrap();
    a2.messages.push(row("a-a2", "A again"));
    a_store.save_conversation(&a2).await.unwrap();
    b.messages.push(row("a-b2", "B again"));
    assert!(stale(b_store.save_conversation(&b).await));
    assert_eq!(
        ids(&b_store.load_conversation("s1").await.unwrap().unwrap()),
        ["u1", "a-b", "a-a2"]
    );
}

#[tokio::test]
async fn a_capture_delivered_after_a_later_accepted_save_is_refused() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let mut live = conv("s1", &[("u1", "base")]);
    s.save_conversation(&live).await.unwrap();
    let mut early = live.capture();
    early.messages.push(row("a-early", "parked producer"));
    live.messages.push(row("a-live", "newer accepted"));
    s.save_conversation(&live).await.unwrap();
    assert!(
        stale(s.save_conversation(&early).await),
        "the parked capture is delivered late"
    );
    assert_eq!(
        ids(&s.load_conversation("s1").await.unwrap().unwrap()),
        ["u1", "a-live"]
    );
}

#[tokio::test]
async fn an_accepted_capture_is_never_replayed_not_even_after_a_restart() {
    let dir = TempDir::new().unwrap();
    let mut live = conv("s1", &[("u1", "base")]);
    store(&dir).save_conversation(&live).await.unwrap();
    live.messages.push(row("a1", "once"));
    let first = live.capture();
    let replay = first.capture();
    store(&dir).save_conversation(&first).await.unwrap();
    // The same capture again, identical bytes, through a new store instance.
    assert!(stale(store(&dir).save_conversation(&replay).await));
    assert_eq!(version(&dir, "s1").await, 2);
}

#[tokio::test]
async fn pipelined_captures_of_one_lineage_chain_but_never_reorder() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let mut live = conv("s1", &[("u1", "base")]);
    s.save_conversation(&live).await.unwrap();
    live.messages.push(row("a1", "first"));
    let first = live.capture();
    live.messages.push(row("a2", "second"));
    let second = live.capture();
    // The older one first: the newer is its validated causal successor (same lineage, no
    // foreign commit between).
    s.save_conversation(&first).await.unwrap();
    s.save_conversation(&second).await.unwrap();
    assert_eq!(
        ids(&s.load_conversation("s1").await.unwrap().unwrap()),
        ["u1", "a1", "a2"]
    );
    // The reverse order: the newer lands, the older is refused rather than shortening history.
    live.messages.push(row("a3", "third"));
    let third = live.capture();
    live.messages.push(row("a4", "fourth"));
    let fourth = live.capture();
    s.save_conversation(&fourth).await.unwrap();
    assert!(stale(s.save_conversation(&third).await));
    assert_eq!(
        ids(&s.load_conversation("s1").await.unwrap().unwrap()).len(),
        5
    );
}

#[tokio::test]
async fn delete_invalidates_the_old_incarnation_before_any_recreation() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let mut live = conv("s1", &[("u1", "base")]);
    s.save_conversation(&live).await.unwrap();
    live.messages.push(row("a1", "in flight"));
    let in_flight = live.capture();
    let observed_missing = conv("s1", &[("u1", "created elsewhere")]);
    s.delete_conversation("s1").await.unwrap();
    assert!(
        stale(s.save_conversation(&in_flight).await),
        "no resurrection by an old save"
    );
    assert!(
        stale(s.save_conversation(&observed_missing).await),
        "an update whose file is missing is not a create"
    );
    assert!(s.load_conversation("s1").await.unwrap().is_none());
    assert!(!s.session_path("s1").exists() && !s.session_dir("s1").exists());
    // Explicit creation starts a new incarnation; the old capture stays refused.
    s.create_conversation(&observed_missing).await.unwrap();
    assert!(stale(s.save_conversation(&in_flight).await));
    let loaded = s.load_conversation("s1").await.unwrap().unwrap();
    assert_eq!(ids(&loaded), ["u1"]);
    assert_eq!(loaded.messages[0].content, "created elsewhere");
}

#[tokio::test]
async fn a_second_store_instance_and_a_symlink_alias_share_one_authority() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(sessions(&dir)).unwrap();
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(sessions(&dir), &alias).unwrap();
    let via_alias = FileSystemStorage::new(alias);
    let direct = store(&dir);
    direct
        .save_conversation(&conv("s1", &[("u1", "base")]))
        .await
        .unwrap();
    let mut a = via_alias.load_conversation("s1").await.unwrap().unwrap();
    let mut b = direct.load_conversation("s1").await.unwrap().unwrap();
    b.messages.push(row("a-b", "B"));
    direct.save_conversation(&b).await.unwrap();
    a.messages.push(row("a-a", "A"));
    assert!(
        stale(via_alias.save_conversation(&a).await),
        "the alias is the same physical session"
    );
    assert_eq!(version(&dir, "s1").await, 2);
}

#[tokio::test]
async fn an_invalid_id_is_refused_for_writes_and_creates_nothing() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    for bad in ["../x", "a/b", "a b"] {
        assert!(
            matches!(
                s.save_conversation(&conv(bad, &[("u1", "x")])).await,
                Err(StorageError::InvalidId(_))
            ),
            "{bad}"
        );
        assert!(s.load_conversation(bad).await.unwrap().is_none());
    }
    assert!(
        !sessions(&dir).exists(),
        "no directory, file or authority entry was created"
    );
}

// ── AC2: complete transactions ──────────────────────────────────────────────────

async fn sidecar(s: &FileSystemStorage, id: &str) -> SessionMeta {
    s.load_session_meta(id).await.unwrap().unwrap()
}

#[tokio::test]
async fn truncate_and_a_stale_save_are_ordered_in_both_directions() {
    let dir = TempDir::new().unwrap();
    let (s, other) = (store(&dir), store(&dir));
    let mut seed = conv(
        "s1",
        &[
            ("u1", "one"),
            ("a1", "two"),
            ("u2", "three"),
            ("a2", "four"),
        ],
    );
    s.save_conversation(&seed).await.unwrap();
    let mut meta = sidecar(&s, "s1").await;
    meta.extra.insert("future".into(), serde_json::json!(7));
    s.save_session_meta("s1", &meta).await.unwrap();
    // Truncate first: the stale capture would resurrect the cut rows and is refused.
    let stale_clone = seed.capture();
    let cut = other.truncate_conversation("s1", 1).await.unwrap();
    assert_eq!(ids(&cut), ["u1", "a1"]);
    assert!(stale(s.save_conversation(&stale_clone).await));
    assert_eq!(
        ids(&s.load_conversation("s1").await.unwrap().unwrap()),
        ["u1", "a1"]
    );
    let side = sidecar(&s, "s1").await;
    assert_eq!((side.message_count, side.title.as_str()), (2, "t"));
    assert_eq!(
        side.extra.get("future"),
        Some(&serde_json::json!(7)),
        "unknown sidecar fields survive"
    );
    assert!(
        !s.checkpoints_file("s1").exists(),
        "a flat text-only truncate creates no log"
    );
    // Save first (the newest accepted state differs inside the retained prefix), then truncate
    // reads that state, not an older copy.
    seed = s.load_conversation("s1").await.unwrap().unwrap();
    seed.messages[0].content = "one, edited".into();
    seed.messages.push(row("u3", "five"));
    s.save_conversation(&seed).await.unwrap();
    let cut = other.truncate_conversation("s1", 1).await.unwrap();
    assert_eq!(cut.messages[0].content, "one, edited");
    assert_eq!(ids(&cut), ["u1", "a1"]);
    assert_eq!(sidecar(&s, "s1").await.message_count, 2);
}

#[tokio::test]
async fn revert_to_checkpoint_prunes_by_id_under_the_same_discipline() {
    let dir = TempDir::new().unwrap();
    let (s, other) = (store(&dir), store(&dir));
    let seed = conv("s1", &[("u1", "one"), ("a1", "two")]);
    s.save_conversation(&seed).await.unwrap();
    let cp1 = s.create_checkpoint("s1").await.unwrap();
    let mut grown = s.load_conversation("s1").await.unwrap().unwrap();
    grown.messages.push(row("u2", "three"));
    grown.messages.push(row("a2", "four"));
    s.save_conversation(&grown).await.unwrap();
    let cp2 = s.create_checkpoint("s1").await.unwrap();
    assert_eq!((cp1, cp2), (CheckpointId(1), CheckpointId(2)));
    let before = grown.capture();
    let reverted = other.revert_to_checkpoint("s1", cp1).await.unwrap();
    assert_eq!(ids(&reverted), ["u1", "a1"]);
    assert!(stale(s.save_conversation(&before).await));
    let log = s.list_checkpoints("s1").await.unwrap();
    assert_eq!(
        log.iter().map(|c| c.id).collect::<Vec<_>>(),
        [CheckpointId(1)]
    );
    assert_eq!(sidecar(&s, "s1").await.message_count, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_checkpoints_migrate_once_and_overlapping_creates_get_unique_ids() {
    let dir = TempDir::new().unwrap();
    let (s, other) = (store(&dir), store(&dir));
    s.save_conversation(&conv("s1", &[("u1", "one"), ("a1", "two")]))
        .await
        .unwrap();
    let (a, b) = tokio::join!(s.create_checkpoint("s1"), other.create_checkpoint("s1"));
    let mut got = vec![a.unwrap().0, b.unwrap().0];
    got.sort();
    assert_eq!(got, [1, 2], "overlapping first creates never share an id");
    assert_eq!(s.detect_layout("s1").await, SessionLayout::Directory);
    assert!(
        !s.session_path("s1").exists() && !s.session_meta_path("s1").exists(),
        "no flat fallback"
    );
    let loaded = s.load_conversation("s1").await.unwrap().unwrap();
    assert_eq!(ids(&loaded), ["u1", "a1"]);
    let log = s.list_checkpoints("s1").await.unwrap();
    assert!(log.iter().all(|c| c.message_index == 1));
    // A later ordinary save and a genuine already-directory checkpoint still work.
    let mut live = loaded;
    live.messages.push(row("u2", "three"));
    s.save_conversation(&live).await.unwrap();
    assert_eq!(s.create_checkpoint("s1").await.unwrap(), CheckpointId(3));
    assert_eq!(
        s.list_checkpoints("s1")
            .await
            .unwrap()
            .last()
            .unwrap()
            .message_index,
        2
    );
}

#[tokio::test]
async fn a_stale_capture_cannot_create_a_checkpoint() {
    let dir = TempDir::new().unwrap();
    let (s, other) = (store(&dir), store(&dir));
    let mut live = conv("s1", &[("u1", "one")]);
    s.save_conversation(&live).await.unwrap();
    live.messages.push(row("a1", "two"));
    let producer = live.capture();
    other.truncate_conversation("s1", 0).await.unwrap(); // a competing accepted writer
    let before = tree(&sessions(&dir));
    assert!(stale(s.create_checkpoint_for(&producer).await));
    assert_eq!(
        tree(&sessions(&dir)),
        before,
        "no checkpoint, migration or log was written"
    );
    // The accepted lineage checkpoints.
    let fresh = s.load_conversation("s1").await.unwrap().unwrap();
    assert_eq!(
        s.create_checkpoint_for(&fresh).await.unwrap(),
        CheckpointId(1)
    );
}

#[tokio::test]
async fn a_turn_capture_still_checkpoints_after_its_own_lineage_saved_twice() {
    // A daemon turn checkpoints from its capture before each tool round, while the forwarder
    // saves the live holder (the committed tool call, then its late result) in between.
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let mut live = conv("s1", &[("u1", "one")]);
    s.save_conversation(&live).await.unwrap();
    let turn = live.capture();
    s.create_checkpoint_for(&turn).await.unwrap();
    live.messages.push(row("a1", "tool call"));
    s.save_conversation(&live).await.unwrap();
    live.messages.push(row("u2", "tool result"));
    s.save_conversation(&live).await.unwrap();
    assert_eq!(
        s.create_checkpoint_for(&turn).await.unwrap(),
        CheckpointId(2),
        "the lineage's own later saves never fence its turn"
    );
    // A competing writer still does.
    store(&dir).truncate_conversation("s1", 0).await.unwrap();
    assert!(stale(s.create_checkpoint_for(&turn).await));
}

// ── AC2: publication outcomes, crash and recovery ───────────────────────────────

#[tokio::test]
async fn a_stop_before_publication_leaves_the_old_state_and_fresh_progress() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let mut live = conv("s1", &[("u1", "base")]);
    s.save_conversation(&live).await.unwrap();
    live.messages.push(row("a1", "lost with the process"));
    fault::arm(&sessions(&dir), "prepared", fault::Action::Fail);
    let attempt = live.capture();
    assert!(matches!(
        s.save_conversation(&attempt).await,
        Err(StorageError::Indeterminate(_))
    ));
    // "Restart": a new instance finds the prepared marker, proves nothing was published.
    let after = store(&dir);
    assert_eq!(
        ids(&after.load_conversation("s1").await.unwrap().unwrap()),
        ["u1"]
    );
    assert_eq!(version(&dir, "s1").await, 1);
    assert!(
        record(&dir, "s1").await.unwrap().prepared.is_none(),
        "the marker is cleared"
    );
    let mut reloaded = after.load_conversation("s1").await.unwrap().unwrap();
    reloaded.messages.push(row("a1", "retry"));
    after.save_conversation(&reloaded).await.unwrap();
    assert_eq!(version(&dir, "s1").await, 2);
}

#[tokio::test]
async fn a_stop_after_publication_keeps_the_new_bytes_and_fences_old_captures() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let mut live = conv("s1", &[("u1", "base")]);
    s.save_conversation(&live).await.unwrap();
    let old = live.capture();
    live.messages.push(row("a1", "published"));
    fault::arm(&sessions(&dir), "published", fault::Action::Fail);
    assert!(matches!(
        s.save_conversation(&live).await,
        Err(StorageError::Indeterminate(_))
    ));
    // New process: the record still says "prepared"; the file proves the publication.
    let after = store(&dir);
    assert!(
        stale(after.save_conversation(&old).await),
        "an older snapshot cannot overwrite recovered history"
    );
    let loaded = after.load_conversation("s1").await.unwrap().unwrap();
    assert_eq!(ids(&loaded), ["u1", "a1"]);
    assert_eq!(
        version(&dir, "s1").await,
        2,
        "the fence advanced to the published version"
    );
    assert_eq!(
        sidecar(&after, "s1").await.message_count,
        2,
        "the missing sidecar effect ran once"
    );
    let mut next = loaded;
    next.messages.push(row("u2", "fresh progress"));
    after.save_conversation(&next).await.unwrap();
}

#[tokio::test]
async fn a_stop_between_publication_and_the_sidecar_is_completed_by_the_next_opener() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let mut live = conv("s1", &[("u1", "base")]);
    s.save_conversation(&live).await.unwrap();
    live.messages.push(row("a1", "two"));
    fault::arm(&sessions(&dir), "visible", fault::Action::Fail);
    assert!(s.save_conversation(&live).await.is_err());
    assert_eq!(
        sidecar(&s, "s1").await.message_count,
        1,
        "the sidecar is stale after the stop"
    );
    assert_eq!(
        ids(&store(&dir).load_conversation("s1").await.unwrap().unwrap()),
        ["u1", "a1"]
    );
    assert_eq!(sidecar(&s, "s1").await.message_count, 2);
    assert!(record(&dir, "s1").await.unwrap().pending.is_empty());
}

#[tokio::test]
async fn a_failed_required_effect_is_a_partial_commit_that_blocks_further_writes_until_repaired() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let mut live = conv("s1", &[("u1", "base")]);
    live.messages[0].images = vec![ImageReference {
        file_name: "0123456789abcdef.png".into(),
        media_type: "image/png".into(),
        original_size: 1,
    }];
    s.save_conversation(&live).await.unwrap();
    let sidecar_path = s.meta_file_in_dir("s1");
    std::fs::remove_file(&sidecar_path).unwrap();
    std::fs::create_dir(&sidecar_path).unwrap();
    live.messages.push(row("a1", "published anyway"));
    assert!(matches!(
        s.save_conversation(&live).await,
        Err(StorageError::PartialCommit(_))
    ));
    assert_eq!(
        version(&dir, "s1").await,
        2,
        "the accepted version advanced with the visible bytes"
    );
    // The next writer is refused until the pending effect can finish: no blind retry by id.
    live.messages.push(row("u2", "blocked"));
    assert!(matches!(
        s.save_conversation(&live).await,
        Err(StorageError::Indeterminate(_))
    ));
    assert_eq!(
        ids(&s.load_conversation("s1").await.unwrap().unwrap()),
        ["u1", "a1"],
        "reads still serve the published bytes"
    );
    std::fs::remove_dir(&sidecar_path).unwrap();
    assert!(s.load_conversation("s1").await.is_ok());
    assert_eq!(
        sidecar(&s, "s1").await.message_count,
        2,
        "recovery finished the recorded effect"
    );
    let mut next = s.load_conversation("s1").await.unwrap().unwrap();
    next.messages.push(row("u2", "after repair"));
    s.save_conversation(&next).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_started_operation_outlives_its_aborted_caller_and_blocks_same_session_writers_only() {
    let dir = TempDir::new().unwrap();
    let (s, other) = (store(&dir), store(&dir));
    let mut live = conv("s1", &[("u1", "base")]);
    s.save_conversation(&live).await.unwrap();
    s.save_conversation(&conv("elsewhere", &[("u1", "independent")]))
        .await
        .unwrap();
    live.messages.push(row("a1", "parked after publication"));
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    fault::arm(
        &sessions(&dir),
        "visible",
        fault::Action::Pause {
            reached: reached_tx,
            release: release_rx,
        },
    );
    let owner = live.capture();
    let owner_store = s.clone();
    let caller = tokio::spawn(async move { owner_store.save_conversation(&owner).await });
    reached_rx.await.unwrap();
    caller.abort();
    let _ = caller.await;
    // A same-session writer from a second adapter cannot enter before the fence is final.
    let rival_store = other.clone();
    let mut contender = Box::pin(async move {
        let mut c = rival_store.load_conversation("s1").await.unwrap().unwrap();
        c.messages.push(row("u2", "contender"));
        rival_store.save_conversation(&c).await
    });
    assert!(futures::poll!(contender.as_mut()).is_pending());
    // Another session keeps progressing.
    let mut e = other.load_conversation("elsewhere").await.unwrap().unwrap();
    e.messages.push(row("a1", "ok"));
    other.save_conversation(&e).await.unwrap();
    release_tx.send(()).unwrap();
    // The aborted caller's operation finished: the contender, based on the pre-operation
    // version it read before parking, is refused.
    let contended = contender.await;
    assert!(
        contended.is_ok() || stale(contended),
        "decided by who loaded first, never torn"
    );
    let loaded = s.load_conversation("s1").await.unwrap().unwrap();
    assert!(
        ids(&loaded).contains(&"a1".to_string()),
        "the started save completed without its caller"
    );
    assert!(record(&dir, "s1").await.unwrap().pending.is_empty());
}

// ── AC2/AC4b: fork, rewind and journals ─────────────────────────────────────────

#[tokio::test]
async fn a_fork_projects_the_accepted_flush_or_is_refused_before_any_new_id_write() {
    let dir = TempDir::new().unwrap();
    let (s, other) = (store(&dir), store(&dir));
    let mut live = conv("s1", &[("u1", "one"), ("a1", "two"), ("u2", "three")]);
    s.save_conversation(&live).await.unwrap();
    live.messages[1].content = "two, flushed".into();
    s.save_conversation(&live).await.unwrap(); // the accepted flush
    let new_id = s.fork_conversation(&live, 1).await.unwrap();
    let forked = s.load_conversation(&new_id).await.unwrap().unwrap();
    assert_eq!(
        forked.messages[1].content, "two, flushed",
        "exactly the accepted flush"
    );
    assert_eq!(forked.fork_source.as_ref().unwrap().conversation_id, "s1");
    assert_eq!(forked.fork_source.as_ref().unwrap().message_index, 1);
    // A competing accepted writer changes the prefix at the same index: refuse, write nothing.
    let mut b = other.load_conversation("s1").await.unwrap().unwrap();
    b.messages[1].content = "two, from B".into();
    other.save_conversation(&b).await.unwrap();
    let before = tree(&sessions(&dir));
    assert!(stale(s.fork_conversation(&live, 1).await));
    assert_eq!(
        tree(&sessions(&dir)),
        before,
        "no new-id transcript, sidecar or log"
    );
}

#[tokio::test]
async fn a_bound_rewind_validates_before_effects_and_installs_the_exact_prefix() {
    let dir = TempDir::new().unwrap();
    let (s, other) = (store(&dir), store(&dir));
    let mut live = conv("s1", &[("u1", "one"), ("a1", "two"), ("u2", "three")]);
    s.save_conversation(&live).await.unwrap();
    // A competing accepted writer: the rewind is refused before the journal or any file effect.
    let mut b = other.load_conversation("s1").await.unwrap().unwrap();
    b.messages[2].content = "three, from B".into();
    other.save_conversation(&b).await.unwrap();
    let before = tree(&sessions(&dir));
    assert!(matches!(
        s.rewind_conversation(&live, 1).await,
        Err(crate::domain::ports::RewindStep::Begin(
            StorageError::Stale(_)
        ))
    ));
    assert_eq!(tree(&sessions(&dir)), before);
    // On the accepted version it rewinds, removes its journal and binds the result.
    live = s.load_conversation("s1").await.unwrap().unwrap();
    let rewound = s.rewind_conversation(&live, 1).await.unwrap();
    assert_eq!(ids(&rewound.conversation), ["u1", "a1"]);
    assert!(s.load_rewind_txn("s1").await.unwrap().is_none());
    let mut next = rewound.conversation;
    next.messages.push(row("u9", "after"));
    s.save_conversation(&next).await.unwrap();
    // A save from before the rewind cannot put the removed row back.
    live.messages.push(row("u8", "stale tail"));
    assert!(stale(s.save_conversation(&live).await));
}

async fn journal_bytes(s: &FileSystemStorage, id: &str) -> Option<Vec<u8>> {
    tokio::fs::read(s.rewind_txn_path(id)).await.ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn startup_recovery_skips_a_live_rewind_and_never_advances_its_journal() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let live = conv("s1", &[("u1", "one"), ("a1", "two"), ("u2", "three")]);
    s.save_conversation(&live).await.unwrap();
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    fault::arm(
        &sessions(&dir),
        "rewind_begun",
        fault::Action::Pause {
            reached: reached_tx,
            release: release_rx,
        },
    );
    let rewinder = s.clone();
    let owner = live.capture();
    let rewind = tokio::spawn(async move { rewinder.rewind_conversation(&owner, 1).await });
    reached_rx.await.unwrap();
    let held = journal_bytes(&s, "s1")
        .await
        .expect("the live journal exists");
    let restart = store(&dir);
    restart.reconcile_pending_txns().await.unwrap();
    assert_eq!(
        journal_bytes(&s, "s1").await.unwrap(),
        held,
        "the live journal is untouched"
    );
    assert_eq!(
        peek_version(&dir, "s1"),
        1,
        "no row or version effect from the contender"
    );
    release_tx.send(()).unwrap();
    let rewound = rewind.await.unwrap().unwrap();
    assert_eq!(ids(&rewound.conversation), ["u1", "a1"]);
    assert!(journal_bytes(&s, "s1").await.is_none());
    restart.reconcile_pending_txns().await.unwrap();
}

#[tokio::test]
async fn abandoned_bound_journals_recover_only_what_the_record_proves() {
    // Nothing started: the journal is removed, rows and version untouched.
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let live = conv("s1", &[("u1", "one"), ("a1", "two"), ("u2", "three")]);
    s.save_conversation(&live).await.unwrap();
    fault::arm(&sessions(&dir), "rewind_begun", fault::Action::Fail);
    assert!(s.rewind_conversation(&live, 1).await.is_err());
    store(&dir).reconcile_pending_txns().await.unwrap();
    assert!(journal_bytes(&s, "s1").await.is_none());
    assert_eq!(
        ids(&s.load_conversation("s1").await.unwrap().unwrap()),
        ["u1", "a1", "u2"]
    );

    // File work was marked begun: not provably idle, so the journal is kept intact.
    fault::arm(&sessions(&dir), "rewind_files_begun", fault::Action::Fail);
    let live = s.load_conversation("s1").await.unwrap().unwrap();
    assert!(s.rewind_conversation(&live, 1).await.is_err());
    let kept = journal_bytes(&s, "s1").await.unwrap();
    store(&dir).reconcile_pending_txns().await.unwrap();
    assert_eq!(journal_bytes(&s, "s1").await.unwrap(), kept);
    assert_eq!(
        ids(&s.load_conversation("s1").await.unwrap().unwrap()),
        ["u1", "a1", "u2"]
    );
    tokio::fs::remove_file(s.rewind_txn_path("s1"))
        .await
        .unwrap();

    // Files reverted, truncation missing: the exact missing action runs once.
    fault::arm(
        &sessions(&dir),
        "rewind_files_reverted",
        fault::Action::Fail,
    );
    let live = s.load_conversation("s1").await.unwrap().unwrap();
    assert!(s.rewind_conversation(&live, 1).await.is_err());
    let recover = store(&dir);
    recover.reconcile_pending_txns().await.unwrap();
    assert_eq!(
        ids(&recover.load_conversation("s1").await.unwrap().unwrap()),
        ["u1", "a1"]
    );
    assert!(journal_bytes(&s, "s1").await.is_none());
    let settled = version(&dir, "s1").await;
    recover.reconcile_pending_txns().await.unwrap();
    assert_eq!(
        version(&dir, "s1").await,
        settled,
        "a second startup is idle"
    );
}

#[tokio::test]
async fn a_journal_bound_to_another_incarnation_is_never_acted_on() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let live = conv("s1", &[("u1", "one"), ("a1", "two")]);
    s.save_conversation(&live).await.unwrap();
    fault::arm(
        &sessions(&dir),
        "rewind_files_reverted",
        fault::Action::Fail,
    );
    assert!(s.rewind_conversation(&live, 0).await.is_err());
    let text = String::from_utf8(journal_bytes(&s, "s1").await.unwrap()).unwrap();
    let recorded = record(&dir, "s1").await.unwrap();
    let forged = text.replace(&recorded.epoch, "some-other-incarnation");
    tokio::fs::write(s.rewind_txn_path("s1"), &forged)
        .await
        .unwrap();
    store(&dir).reconcile_pending_txns().await.unwrap();
    assert_eq!(journal_bytes(&s, "s1").await.unwrap(), forged.as_bytes());
    assert_eq!(
        ids(&s.load_conversation("s1").await.unwrap().unwrap()),
        ["u1", "a1"]
    );
}

// ── AC2: sidecars and previews ──────────────────────────────────────────────────

#[tokio::test]
async fn a_narrow_sidecar_update_keeps_fields_a_concurrent_save_wrote() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let mut live = conv("s1", &[("u1", "one")]);
    s.save_conversation(&live).await.unwrap();
    let stale_copy = sidecar(&s, "s1").await; // what a tab cached before the next save
    live.messages.push(row("a1", "two"));
    s.save_conversation(&live).await.unwrap();
    s.update_session_meta("s1", &|mut m| {
        m.bookmarks = vec![1];
        m
    })
    .await
    .unwrap();
    let side = sidecar(&s, "s1").await;
    assert_eq!((side.message_count, side.bookmarks.clone()), (2, vec![1]));
    assert_eq!(stale_copy.message_count, 1);
}

#[tokio::test]
async fn reads_listings_and_a_missing_delete_write_no_authority_entry() {
    let dir = TempDir::new().unwrap();
    let sessions_dir = sessions(&dir);
    std::fs::create_dir_all(&sessions_dir).unwrap();
    // A legacy session: written before authority records existed.
    let persisted = PersistedConversation::from_conversation_with_exit(
        &conv("legacy", &[("u1", "old")]),
        false,
    );
    std::fs::write(
        sessions_dir.join("legacy.meta.json"),
        serde_json::to_string_pretty(&persisted).unwrap(),
    )
    .unwrap();
    let before = tree(&sessions_dir);
    let s = store(&dir);
    let mut loaded = s.load_conversation("legacy").await.unwrap().unwrap();
    assert_eq!(ids(&loaded), ["u1"]);
    assert_eq!(s.list_conversations_read_only().await.unwrap().len(), 1);
    s.delete_conversation("never-existed").await.unwrap();
    assert_eq!(
        tree(&sessions_dir),
        before,
        "previews and no-op deletes leave every byte alone"
    );
    assert!(!sessions_dir.join(".rustain-authority").exists());
    // The first upgraded write adopts the legacy session without losing it.
    loaded.messages.push(row("a1", "new"));
    s.save_conversation(&loaded).await.unwrap();
    assert_eq!(version(&dir, "legacy").await, 1);
    assert_eq!(
        ids(&s.load_conversation("legacy").await.unwrap().unwrap()),
        ["u1", "a1"]
    );
}

// ── two real processes ──────────────────────────────────────────────────────────

/// Child half of the cross-process cases: inert unless a parent launched it with a role.
#[test]
fn child_process_role() {
    let Some(role) = crate::infrastructure::utils::env_var_trimmed("D24_ROLE") else {
        return;
    };
    let dir = PathBuf::from(crate::infrastructure::utils::env_var_trimmed("D24_SESSIONS").unwrap());
    let say = |line: String| {
        println!("D24:{line}");
        std::io::stdout().flush().unwrap();
    };
    let wait_for_parent = || {
        let mut gate = String::new();
        std::io::stdin().lock().read_line(&mut gate).unwrap();
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let s = FileSystemStorage::new(dir.clone());
        match role.as_str() {
            "writer" => {
                let mut c = s.load_conversation("shared").await.unwrap().unwrap();
                say("LOADED".into());
                wait_for_parent();
                c.messages.push(row("a-child", "from the child process"));
                let outcome = match s.save_conversation(&c).await {
                    Ok(()) => "ok".to_string(),
                    Err(StorageError::Stale(_)) => "stale".to_string(),
                    Err(other) => format!("error {other}"),
                };
                say(format!("RESULT {outcome}"));
            }
            "holder" => {
                let held = authority::acquire(&dir, "shared").await.unwrap();
                say("HELD".into());
                wait_for_parent();
                drop(held);
                say("RELEASED".into());
            }
            other => panic!("unknown role {other}"),
        }
    });
}

struct Child {
    process: std::process::Child,
    lines: std::io::BufReader<std::process::ChildStdout>,
}

impl Child {
    fn start(role: &str, sessions: &Path) -> Self {
        let mut process = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "adapters::filesystem::fence_tests::child_process_role",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("D24_ROLE", role)
            .env("D24_SESSIONS", sessions)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let lines = std::io::BufReader::new(process.stdout.take().unwrap());
        Self { process, lines }
    }

    /// The next `D24:` line the child printed (libtest chatter is skipped).
    fn say(&mut self) -> String {
        loop {
            let mut line = String::new();
            assert!(
                self.lines.read_line(&mut line).unwrap() > 0,
                "the child exited before answering"
            );
            // `--nocapture` puts libtest's own "test <name> ... " in front of the first line.
            if let Some((_, rest)) = line.trim().split_once("D24:") {
                return rest.to_string();
            }
        }
    }

    fn go(&mut self) {
        let stdin = self.process.stdin.as_mut().unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    fn finish(mut self) {
        drop(self.process.stdin.take());
        assert!(self.process.wait().unwrap().success());
    }
}

#[tokio::test]
async fn a_second_process_with_a_retained_base_is_refused_after_the_first_commits() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let mut seed = conv("shared", &[("u1", "base")]);
    s.save_conversation(&seed).await.unwrap();
    let mut child = Child::start("writer", &sessions(&dir));
    assert_eq!(
        child.say(),
        "LOADED",
        "the child holds the base before the parent commits"
    );
    seed.messages.push(row("a-parent", "parent commits first"));
    s.save_conversation(&seed).await.unwrap();
    child.go();
    assert_eq!(
        child.say(),
        "RESULT stale",
        "the child captured from the retained version"
    );
    child.finish();
    assert_eq!(
        ids(&s.load_conversation("shared").await.unwrap().unwrap()),
        ["u1", "a-parent"]
    );
    // A child that loads after the parent's commit commits normally.
    let mut next = Child::start("writer", &sessions(&dir));
    assert_eq!(next.say(), "LOADED");
    next.go();
    assert_eq!(next.say(), "RESULT ok");
    next.finish();
    assert_eq!(
        ids(&s.load_conversation("shared").await.unwrap().unwrap()),
        ["u1", "a-parent", "a-child"]
    );
    assert_eq!(version(&dir, "shared").await, 3);
}

#[tokio::test]
async fn the_session_lock_excludes_another_process_until_it_is_released() {
    let dir = TempDir::new().unwrap();
    store(&dir)
        .save_conversation(&conv("shared", &[("u1", "base")]))
        .await
        .unwrap();
    let mut holder = Child::start("holder", &sessions(&dir));
    assert_eq!(holder.say(), "HELD");
    // The other process is alive and holds the lock inode: a non-blocking attempt is refused.
    let attempt = authority::try_acquire_existing(&sessions(&dir), "shared")
        .await
        .unwrap();
    assert!(matches!(attempt, Opened::Busy));
    // A different session is independent.
    store(&dir)
        .save_conversation(&conv("other", &[("u1", "x")]))
        .await
        .unwrap();
    holder.go();
    assert_eq!(holder.say(), "RELEASED");
    holder.finish();
    assert!(matches!(
        authority::try_acquire_existing(&sessions(&dir), "shared")
            .await
            .unwrap(),
        Opened::Held(_)
    ));
}

#[tokio::test]
async fn a_killed_process_releases_the_lock_and_its_prepared_work_is_recovered() {
    let dir = TempDir::new().unwrap();
    store(&dir)
        .save_conversation(&conv("shared", &[("u1", "base")]))
        .await
        .unwrap();
    let mut holder = Child::start("holder", &sessions(&dir));
    assert_eq!(holder.say(), "HELD");
    holder.process.kill().unwrap();
    holder.process.wait().unwrap(); // reaped: the kernel released the flock with the process
    let reopened = authority::try_acquire_existing(&sessions(&dir), "shared")
        .await
        .unwrap();
    assert!(
        matches!(reopened, Opened::Held(_)),
        "process death releases the lock"
    );
}
