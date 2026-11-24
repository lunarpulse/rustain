//! Story Q-6 AC6 — conservation recovery is constrained to verified local
//! authority.
//!
//! Front door: `admit_workspace_authority` is what `startup` calls after it opens
//! the workspace `NodeJournal`: real journal, real writer
//! (`AuthorityLedger::journal_head` → `LedgerJournalSink` → `NodeJournal`), real
//! Ed25519 trust under a tempdir data dir, real admission outcome. Nothing here
//! injects a verdict or calls the inner replay routine.
//!
//! Distinguished outcomes: authorized first initialization; verified restart with
//! preserved spend; refusal of legacy unsigned state; refusal of a rolled-back
//! journal; crash ordering (journal ahead of the anchor); one writer per
//! workspace. All state is synthetic and generated in tempdirs; no journal is
//! tampered with beyond removing or restoring its own files.

use std::path::PathBuf;
use std::sync::Arc;

use rustain::domain::clock::{Clock, SystemClock};
use rustain::domain::models::{AgentId, Budget, CapabilityToken, LedgerConservationRecord};
use rustain::domain::ports::LedgerJournalSink;
use rustain::domain::services::authority_recovery::RecoveryRefusal;
use rustain::infrastructure::subagent::NodeJournal;
use rustain::infrastructure::subagent::authority_trust::{
    AdmissionRefusal, AdmissionStatus, AuthorityAdmission, admit_workspace_authority,
};

struct World {
    workspace: PathBuf,
    data_dir: PathBuf,
    _tmp: tempfile::TempDir,
}

fn world() -> World {
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let workspace = base.join("ws");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    World {
        workspace,
        data_dir,
        _tmp: tmp,
    }
}

/// A second workspace that shares `data_dir` — one user, two workspaces.
fn world_sharing(data_dir: &std::path::Path) -> World {
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let workspace = base.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    World {
        workspace,
        data_dir: data_dir.to_path_buf(),
        _tmp: tmp,
    }
}

fn clock() -> Arc<dyn Clock> {
    Arc::new(SystemClock::default())
}

/// What `startup` does: open the workspace journal, then admit.
async fn start(world: &World) -> (AuthorityAdmission, Arc<NodeJournal>) {
    let journal = Arc::new(NodeJournal::open_workspace(&world.workspace).await.unwrap());
    let admission =
        admit_workspace_authority(&world.workspace, &world.data_dir, journal.clone(), clock())
            .await;
    (admission, journal)
}

fn journal_file(world: &World) -> PathBuf {
    let rooms = world.workspace.join(".rustain").join("rooms");
    std::fs::read_dir(rooms)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .expect("the workspace journal exists")
}

fn anchor_file(world: &World) -> PathBuf {
    std::fs::read_dir(world.data_dir.join("authority"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "anchor"))
        .expect("the anchor exists")
}

/// Spend: delegate one child and flush through the real writer.
async fn spend_one_child(admission: &AuthorityAdmission, name: &str) {
    admission
        .ledger
        .delegate(
            &admission.root,
            CapabilityToken::r1_child_request(AgentId::parse(name).unwrap()),
        )
        .expect("a spendable root can delegate");
    admission
        .ledger
        .journal_head()
        .await
        .expect("the write-ahead flush succeeds");
}

fn available(admission: &AuthorityAdmission) -> Budget {
    admission.ledger.available(&admission.root.id).unwrap()
}

/// A refused authority holds an unspendable root: nothing can be delegated.
fn assert_unspendable(admission: &AuthorityAdmission, why: &str) {
    assert_eq!(available(admission), Budget::ZERO, "{why}: no budget");
    let attempt = admission.ledger.delegate(
        &admission.root,
        CapabilityToken::r1_child_request(AgentId::parse("after-refusal").unwrap()),
    );
    assert!(
        attempt.is_err(),
        "{why}: a refused authority delegates nothing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn k6_first_initialization_then_verified_restart_preserves_spend() {
    let world = world();

    // Authorized first initialization: no history, no anchor.
    let (first, _journal) = start(&world).await;
    assert_eq!(first.status, AdmissionStatus::FirstInitialization);
    let full = available(&first);
    assert_ne!(full, Budget::ZERO, "a new authority is spendable");
    spend_one_child(&first, "child-a").await;
    let after_spend = available(&first);
    assert_ne!(after_spend, full, "control: the spend is real");
    drop(first); // releases the single-writer lease, as a process exit does

    // Verified restart: the spend survives; nothing resets to a spendable root.
    let (second, _journal) = start(&world).await;
    assert_eq!(second.status, AdmissionStatus::VerifiedRestart);
    assert_eq!(
        available(&second),
        after_spend,
        "a verified restart restores the conserved budget exactly"
    );
    spend_one_child(&second, "child-b").await;
    let after_second = available(&second);
    assert_ne!(after_second, after_spend);
    drop(second);

    let (third, _journal) = start(&world).await;
    assert_eq!(third.status, AdmissionStatus::VerifiedRestart);
    assert_eq!(
        available(&third),
        after_second,
        "sequence continues across restarts"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn k6_legacy_unsigned_history_without_an_anchor_is_refused_not_migrated() {
    let world = world();
    // Legacy state: a conservation head from before Q-6 — no sequence, no
    // signature — written through the same journal sink the product uses.
    let journal = Arc::new(NodeJournal::open_workspace(&world.workspace).await.unwrap());
    let root = CapabilityToken::r1_root(AgentId::root());
    journal
        .journal_conservation(LedgerConservationRecord {
            token: root.id,
            total: root.budget,
            available: Budget::ZERO,
            consumed: root.budget,
            uses_remaining: None,
            settled: false,
            revoked: false,
            authority_time_ms: 5,
            ledger_seq: 0,
            authority_sig: None,
        })
        .await
        .unwrap();
    drop(journal);

    let (admission, _journal) = start(&world).await;

    assert_eq!(
        admission.status,
        AdmissionStatus::Refused(AdmissionRefusal::History(
            RecoveryRefusal::UnanchoredHistory
        ))
    );
    assert_unspendable(&admission, "legacy history");
    assert!(
        !world.data_dir.join("authority").exists()
            || std::fs::read_dir(world.data_dir.join("authority"))
                .unwrap()
                .flatten()
                .all(|entry| entry.path().extension().is_none_or(|ext| ext != "anchor")),
        "refusal must not establish an authority over unverified history"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn k6_a_journal_rolled_back_behind_the_anchor_is_refused() {
    let world = world();
    let (first, _journal) = start(&world).await;
    spend_one_child(&first, "child-a").await;
    drop(first);

    // The workspace journal loses its history (removed, as a rollback would);
    // the independent anchor still says conservation reached a later sequence.
    std::fs::write(journal_file(&world), b"").unwrap();

    let (admission, _journal) = start(&world).await;
    assert_eq!(
        admission.status,
        AdmissionStatus::Refused(AdmissionRefusal::History(RecoveryRefusal::RolledBack))
    );
    assert_unspendable(&admission, "rolled-back journal");
}

#[tokio::test(flavor = "multi_thread")]
async fn k6_a_record_the_authority_key_did_not_sign_is_refused() {
    let world = world();
    let (first, journal) = start(&world).await;
    spend_one_child(&first, "child-a").await;
    // A forged head at the next sequence: unsigned, but claims a restored budget.
    let root = first.root.clone();
    journal
        .journal_conservation(LedgerConservationRecord {
            token: root.id,
            total: root.budget,
            available: root.budget,
            consumed: Budget::ZERO,
            uses_remaining: None,
            settled: false,
            revoked: false,
            authority_time_ms: 1,
            ledger_seq: 99,
            authority_sig: Some(vec![7; 64]),
        })
        .await
        .unwrap();
    drop(first);

    let (admission, _journal) = start(&world).await;
    assert!(
        matches!(
            admission.status,
            AdmissionStatus::Refused(AdmissionRefusal::History(
                RecoveryRefusal::InvalidSignature | RecoveryRefusal::SequenceGap
            ))
        ),
        "{:?}",
        admission.status
    );
    assert_unspendable(&admission, "forged head");
}

#[tokio::test(flavor = "multi_thread")]
async fn k6_a_journal_ahead_of_the_anchor_after_a_crash_is_admitted_and_the_anchor_catches_up() {
    let world = world();
    let (first, _journal) = start(&world).await;
    let anchor_before = std::fs::read(anchor_file(&world)).unwrap();
    spend_one_child(&first, "child-a").await;
    let after_spend = available(&first);
    drop(first);
    // Crash between "record durable" and "anchor raised": the anchor is the one
    // written before the flush.
    let anchor = anchor_file(&world);
    let advanced = std::fs::read(&anchor).unwrap();
    assert_ne!(
        advanced, anchor_before,
        "control: the flush raised the anchor"
    );
    std::fs::write(&anchor, &anchor_before).unwrap();

    let (second, _journal) = start(&world).await;

    assert_eq!(second.status, AdmissionStatus::VerifiedRestart);
    assert_eq!(available(&second), after_spend);
    assert_eq!(
        std::fs::read(&anchor).unwrap(),
        advanced,
        "the verified start catches the anchor up to the durable journal"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn k6_a_second_live_process_over_the_same_workspace_is_refused() {
    let world = world();
    let (first, _journal) = start(&world).await;
    assert_eq!(first.status, AdmissionStatus::FirstInitialization);

    let (second, _journal) = start(&world).await;

    assert_eq!(
        second.status,
        AdmissionStatus::Refused(AdmissionRefusal::Busy)
    );
    assert_unspendable(&second, "second writer");
    // The first authority is unaffected.
    spend_one_child(&first, "child-a").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn k6_an_anchor_for_another_workspace_does_not_admit_this_one() {
    // Establish workspace A, spend, then present A's anchor under workspace B's
    // name: the signature binds the workspace, so B is unverifiable.
    let world_a = world();
    let (first, _journal) = start(&world_a).await;
    spend_one_child(&first, "child-a").await;
    drop(first);

    // Same user key, different workspace: only the workspace binding differs.
    let world_b = world_sharing(&world_a.data_dir);
    let (b_first, _journal) = start(&world_b).await;
    drop(b_first);
    let anchors: Vec<PathBuf> = std::fs::read_dir(world_a.data_dir.join("authority"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "anchor"))
        .collect();
    assert_eq!(anchors.len(), 2, "one anchor per workspace");
    let (a_anchor, b_anchor) = {
        let id_a = rustain::infrastructure::paths::workspace_hash(&world_a.workspace);
        let a = anchors
            .iter()
            .find(|path| path.to_string_lossy().contains(&id_a))
            .unwrap()
            .clone();
        let b = anchors.iter().find(|path| **path != a).unwrap().clone();
        (a, b)
    };
    std::fs::copy(&a_anchor, &b_anchor).unwrap();

    let (admission, _journal) = start(&world_b).await;
    assert_eq!(
        admission.status,
        AdmissionStatus::Refused(AdmissionRefusal::History(
            RecoveryRefusal::UnverifiableAnchor
        ))
    );
    assert_unspendable(&admission, "foreign anchor");
}

#[test]
fn the_diagnostics_are_categories_without_record_or_key_content() {
    for refusal in [
        AdmissionRefusal::Busy,
        AdmissionRefusal::StoreUnavailable,
        AdmissionRefusal::History(RecoveryRefusal::UnanchoredHistory),
        AdmissionRefusal::History(RecoveryRefusal::RolledBack),
    ] {
        let text = refusal.diagnostic();
        assert!(text.contains("no budget is granted"), "{text}");
        assert!(!text.contains("signing.key"), "{text}");
    }
}
