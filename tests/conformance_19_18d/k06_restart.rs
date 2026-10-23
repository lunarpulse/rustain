//! `K06` (AC5, `A5`, `P4`, `P6`, `F5`, `F13`) and the CALLER half of `K06-P` —
//! cold recovery of bound tasks through BOTH restart passes, and the skip of
//! malformed handles in each of them.
//!
//! Front door: a bound-only host (`peerKeys`, `admission: ask`, no consent
//! projection — the shape on which an unanswered approval has NO node, so only
//! the pending file can restore it, and an answered one has a durable waiting
//! node, so only the orphan pass can) is written as an `a2a.json`, parsed by
//! both production parsers, resolved by `resolve_server_auth`, and served by
//! `serve(ServeConfig…)` on `0.0.0.0:0` with TLS. Every caller is a raw
//! JSON-RPC client anchored on the host certificate. The restart is
//! [`BoundHost::restart`]: same workspace and journal, `with_host_binding`,
//! `NodeRecovery::reconcile`, then `serve`'s two reconcile passes.
//!
//! Mutants this module turns RED (story MUTANT REGISTER): `M15` (recovered
//! `peer:` task re-keyed under `loopback` — K_P1B's `tasks/get` answers
//! not-found), `M49` (orphan reconcile builds `A2aPseudonym(policy.peer())` —
//! the durable item id goes missing from the held task's answer), `M50`
//! (pending reconcile inserts under a re-derived handle — K_P1B's `tasks/get`
//! of the approval-pending task answers not-found), `M48` (`from_encoded`
//! accepts arbitrary prefixed handles — the malformed node/record is restored
//! instead of skipped).
//!
//! Idioms copied from `conformance_19_18d0_a2a_typed_policy_principal.rs`
//! (`:1322-1480`: `answer_card`, `await_waiting`, `pending_records`,
//! `pending_records_cleared`, the orphan-vs-pending-file restart shape).

use base64::Engine as _;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use rustain::adapters::a2a::RECIPIENT_ITEM_METADATA_KEY;
use rustain::adapters::a2a::exec::{INBOUND_NODE_PREFIX, INBOUND_SUBAGENT_TYPE, RESTART_DETAIL};
use rustain::domain::models::AgentId;
use rustain::domain::ports::InboundPeerRuntime;
use tokio_util::sync::CancellationToken;

use super::harness::{
    BoundHost, HostSpec, K_P1A, K_P1B, K_P1C, K_P2, K_U, ORPHAN_WARN, P1_X, PENDING_WARN,
    assert_not_found_like, detail, lines_with, listed_ids, log_cursor, log_since,
    no_live_inbound_node, pin_hex, received_row_for_task, received_rows, send_and_admit,
    send_and_leave_pending, setup, wire_state,
};

/// The caller-owned skip warnings (`P4`).
const NODE_SKIP_WARN: &str = "skipping malformed A2A inbound task node";
const RECORD_SKIP_WARN: &str = "skipping malformed A2A pending task record";

/// Admission `ask` over the standard bound-only roster (B1 keys, plus the
/// unbound `K_U` when `with_unbound`).
fn ask_spec(with_unbound: bool) -> HostSpec {
    let base = if with_unbound {
        HostSpec::b2()
    } else {
        HostSpec::b1()
    };
    base.server_key("admission", "ask".into())
}

fn pending_path(host: &BoundHost) -> std::path::PathBuf {
    host.journal.path.join(".rustain/a2a-pending.json")
}

fn pending_records(host: &BoundHost) -> Vec<serde_json::Value> {
    serde_json::from_slice(&std::fs::read(pending_path(host)).expect("the pending file"))
        .expect("the pending file is a JSON array")
}

/// `K06` — one bound pin, two in-flight tasks across a cold restart.
///
/// Before the restart (B1 + `ask`): K_P1A's task is approved and held after
/// `RecipientItemReceived` (its row stores alias `p1`, address
/// `A2aBound(pin)`); K_P1C's task (the `p1-alt` slot, same pin) stays
/// approval-pending (pending-file record `submitter: peer:<pin>`, no node).
/// After the cold restart, K_P1B — a third key bound to the same pin — reads
/// BOTH as `failed` with exactly `RESTART_DETAIL`; only the held one carries
/// its durable item id; K_P2 (another pin) reads neither; nothing resumes.
#[tokio::test(flavor = "multi_thread")]
async fn k06_bound_tasks_survive_a_cold_restart_through_both_passes() {
    setup();
    const HELD: &str = "k06-held";
    const PENDING: &str = "k06-pending";
    const NEVER: &str = "k06-never-submitted";
    let pin = pin_hex(P1_X);
    let handle = format!("peer:{pin}");

    let mut host = BoundHost::start(ask_spec(false)).await;
    let mut operator = host.attach_operator().await;
    let (k_a, k_b, k_c, k_other) = (
        host.client(K_P1A),
        host.client(K_P1B),
        host.client(K_P1C),
        host.client(K_P2),
    );

    // ── Before: one held task, one approval-pending task ────────────────────
    let item = send_and_admit(&host, &mut operator, &k_a, HELD).await;
    let row = received_row_for_task(&host.journal, HELD);
    assert_eq!(
        row["alias"], "p1",
        "K_P1A's Received row stores its matched slot alias: {row}"
    );
    assert_eq!(row["address"]["principal"]["kind"], "a2a_bound", "{row}");
    assert_eq!(row["address"]["boundPeer"], pin.as_str(), "{row}");

    send_and_leave_pending(&host, &k_c, PENDING).await;
    let records = pending_records(&host);
    assert!(
        records
            .iter()
            .any(|record| record["task_id"] == PENDING && record["submitter"] == handle.as_str()),
        "the approval-pending task is on the pending-file path under the pin handle: {records:?}"
    );
    assert!(
        received_rows(&host.journal)
            .iter()
            .all(|row| row["task"] != PENDING),
        "the approval-pending task has no item yet"
    );

    // Positive controls for the post-restart `failed`: both are live now, for
    // K_P1B (same pin as their submitters), and invisible to K_P2.
    let live_held = k_b.tasks_get(2, HELD, None).await;
    assert_ne!(wire_state(&live_held), "failed", "{}", live_held.body);
    assert_eq!(
        live_held.json["result"]["metadata"][RECIPIENT_ITEM_METADATA_KEY],
        item.as_str(),
        "{}",
        live_held.body
    );
    let live_pending = k_b.tasks_get(3, PENDING, None).await;
    assert_eq!(
        wire_state(&live_pending),
        "auth-required",
        "{}",
        live_pending.body
    );
    let control = k_other.tasks_get(4, NEVER, None).await;
    assert!(!control.json["error"].is_null(), "{}", control.body);
    assert_not_found_like(
        &k_other.tasks_get(5, HELD, None).await,
        &control,
        "before the restart, a different pin does not see the held task",
    );

    // ── Cold restart: the operator socket goes first, then the process ──────
    drop(operator);
    let spec = host.spec.clone();
    let cursor = log_cursor();
    let host = host.restart(spec).await;
    let (k_b, k_other) = (host.client(K_P1B), host.client(K_P2));

    // Both passes ran: each restored task logged its own pass's line.
    assert_eq!(
        lines_with(cursor, ORPHAN_WARN, HELD),
        1,
        "the orphan-node pass failed the held task:\n{}",
        log_since(cursor)
    );
    assert_eq!(
        lines_with(cursor, PENDING_WARN, PENDING),
        1,
        "the pending-file pass failed the approval-pending task:\n{}",
        log_since(cursor)
    );
    assert_eq!(
        lines_with(cursor, ORPHAN_WARN, PENDING),
        0,
        "the approval-pending task had no node, so the orphan pass never saw it"
    );
    assert_eq!(
        lines_with(cursor, PENDING_WARN, HELD),
        0,
        "the held task was restored once, by the orphan pass"
    );

    // K_P1B (a third slot on the same pin) reads both as failed with the
    // exact restart detail.
    let held = k_b.tasks_get(10, HELD, None).await;
    assert_eq!(wire_state(&held), "failed", "{}", held.body);
    assert_eq!(detail(&held), Some(RESTART_DETAIL), "{}", held.body);
    assert_eq!(
        held.json["result"]["metadata"][RECIPIENT_ITEM_METADATA_KEY],
        item.as_str(),
        "the held task carries its durable item id (M49): {}",
        held.body
    );
    let pending = k_b.tasks_get(11, PENDING, None).await;
    assert_eq!(wire_state(&pending), "failed", "{}", pending.body);
    assert_eq!(detail(&pending), Some(RESTART_DETAIL), "{}", pending.body);
    assert!(
        pending.json["result"]["metadata"][RECIPIENT_ITEM_METADATA_KEY].is_null(),
        "the approval-pending task never had an item: {}",
        pending.body
    );

    // A different pin reads neither (restored tasks stay exact-handle).
    let control = k_other.tasks_get(12, NEVER, None).await;
    for task in [HELD, PENDING] {
        assert_not_found_like(
            &k_other.tasks_get(13, task, None).await,
            &control,
            "a different pin does not see a restored task",
        );
    }

    // Neither is approvable: no node is live, the pending file is drained and
    // both answers are stable.
    assert!(no_live_inbound_node(&host).await, "nothing resumed");
    assert!(
        pending_records(&host).is_empty(),
        "the pending file was truncated after the reconcile"
    );
    for task in [HELD, PENDING] {
        let again = k_b.tasks_get(14, task, None).await;
        assert_eq!(wire_state(&again), "failed", "{}", again.body);
    }
}

/// `K06-P` (caller half) — a noncanonical handle in EACH recovery path is
/// skipped with exactly one warning, restores no task and no item, while a
/// canonical twin in the same restart IS restored.
///
/// (a) Orphan-node path: two inbound nodes are registered through the
/// production runtime (`park_pending_consent`, the way `setup_task` parks a
/// node) under node ids shaped like `mint_inbound_node_id`'s but encoding a
/// bad handle (`peer:zz`; a padded `apikey:` digest). (b) Pending-file path:
/// four records with noncanonical `submitter`s (padded `apikey:`, uppercase
/// pin, unknown prefix, `peer:zz`) are appended to `a2a-pending.json`.
///
/// `M48` → RED (`from_encoded` accepts arbitrary prefixed handles): the skip
/// warnings disappear and the malformed tasks answer `failed` to their
/// presenter-shaped handles.
#[tokio::test(flavor = "multi_thread")]
async fn k06p_malformed_handles_are_skipped_in_each_recovery_path() {
    setup();
    const TWIN_HELD: &str = "k06p-twin-held";
    const TWIN_PENDING: &str = "k06p-twin-pending";
    const NEVER: &str = "k06p-never-submitted";
    let pin = pin_hex(P1_X);

    let padded_apikey = format!("apikey:{}", URL_SAFE.encode([7u8; 32]));
    assert!(padded_apikey.ends_with('='), "the fixture is padded");
    let upper_pin = format!("peer:{}", pin.to_uppercase());
    let bad_nodes = [
        ("k06p-bad-node-zz", "peer:zz".to_owned()),
        ("k06p-bad-node-padded", padded_apikey.clone()),
    ];
    let bad_records = [
        ("k06p-bad-rec-padded", padded_apikey),
        ("k06p-bad-rec-upper", upper_pin),
        ("k06p-bad-rec-prefix", "future:opaque-handle".to_owned()),
        ("k06p-bad-rec-zz", "peer:zz".to_owned()),
    ];

    let mut host = BoundHost::start(ask_spec(true)).await;
    let mut operator = host.attach_operator().await;
    let (k_a, k_c) = (host.client(K_P1A), host.client(K_P1C));

    // Canonical twins: one durable waiting node + item (orphan pass), one
    // unanswered approval (pending-file pass).
    let twin_item = send_and_admit(&host, &mut operator, &k_a, TWIN_HELD).await;
    send_and_leave_pending(&host, &k_c, TWIN_PENDING).await;

    // (a) malformed nodes, registered the way production parks an inbound node.
    let mut bad_node_ids = Vec::new();
    for (task, handle) in &bad_nodes {
        let node = AgentId::parse(&format!(
            "{INBOUND_NODE_PREFIX}/p-{}/t-{}",
            URL_SAFE_NO_PAD.encode(handle),
            URL_SAFE_NO_PAD.encode(task)
        ))
        .expect("a syntactically valid node id");
        host.server
            .park_pending_consent(&node, INBOUND_SUBAGENT_TYPE, CancellationToken::new())
            .await
            .expect("the malformed node registers (the id is only parsed on restart)");
        bad_node_ids.push(node.as_str().to_owned());
    }
    let live: Vec<String> = host
        .node_tree
        .list()
        .await
        .iter()
        .map(|entry| entry.agent_id.as_str().to_owned())
        .collect();
    assert!(
        bad_node_ids.iter().all(|id| live.contains(id)),
        "both malformed nodes are live in the tree before the restart: {live:?}"
    );

    // (b) malformed pending records, appended next to the twin's.
    let mut records = pending_records(&host);
    assert!(
        records
            .iter()
            .any(|record| record["task_id"] == TWIN_PENDING),
        "the canonical twin is on the pending-file path: {records:?}"
    );
    for (task, handle) in &bad_records {
        records.push(serde_json::json!({ "task_id": task, "submitter": handle }));
    }
    std::fs::write(
        pending_path(&host),
        serde_json::to_vec(&records).expect("encode"),
    )
    .expect("plant the malformed pending records");

    // ── Cold restart ────────────────────────────────────────────────────────
    drop(operator);
    let spec = host.spec.clone();
    let cursor = log_cursor();
    let workspace = host.journal.path.to_string_lossy().into_owned();
    let host = host.restart(spec).await;

    // Exactly one warning per skipped record, none for the twins.
    for node in &bad_node_ids {
        assert_eq!(
            lines_with(cursor, NODE_SKIP_WARN, node),
            1,
            "one skip warning for node {node}:\n{}",
            log_since(cursor)
        );
    }
    assert_eq!(
        lines_with(cursor, RECORD_SKIP_WARN, &workspace),
        bad_records.len(),
        "one skip warning per malformed pending record:\n{}",
        log_since(cursor)
    );
    for (task, _) in bad_nodes.iter().chain(bad_records.iter()) {
        assert_eq!(
            lines_with(cursor, ORPHAN_WARN, task) + lines_with(cursor, PENDING_WARN, task),
            0,
            "{task}: a skipped record is never restored"
        );
    }

    // Positive controls: the canonical twins ARE restored, one per pass.
    assert_eq!(lines_with(cursor, ORPHAN_WARN, TWIN_HELD), 1);
    assert_eq!(lines_with(cursor, PENDING_WARN, TWIN_PENDING), 1);
    let (k_b, k_u, k_other) = (host.client(K_P1B), host.client(K_U), host.client(K_P2));
    let held = k_b.tasks_get(1, TWIN_HELD, None).await;
    assert_eq!(wire_state(&held), "failed", "{}", held.body);
    assert_eq!(detail(&held), Some(RESTART_DETAIL), "{}", held.body);
    assert_eq!(
        held.json["result"]["metadata"][RECIPIENT_ITEM_METADATA_KEY],
        twin_item.as_str(),
        "{}",
        held.body
    );
    let pending = k_b.tasks_get(2, TWIN_PENDING, None).await;
    assert_eq!(wire_state(&pending), "failed", "{}", pending.body);
    assert_eq!(detail(&pending), Some(RESTART_DETAIL), "{}", pending.body);

    // No restored task: every malformed task id answers as a never-seen id,
    // under a same-pin key, another pin's key and an unbound key.
    let control = k_b.tasks_get(3, NEVER, None).await;
    assert!(!control.json["error"].is_null(), "{}", control.body);
    for (task, _) in bad_nodes.iter().chain(bad_records.iter()) {
        for (who, client) in [("K_P1B", &k_b), ("K_P2", &k_other), ("K_U", &k_u)] {
            assert_not_found_like(
                &client.tasks_get(4, task, None).await,
                &control,
                &format!("{task} under {who}"),
            );
        }
    }

    // No restored item: the journal holds the twin's item only, and the pin's
    // list shows exactly it.
    let rows = received_rows(&host.journal);
    assert_eq!(rows.len(), 1, "only the twin minted an item: {rows:?}");
    let list = k_b.items_list(5).await;
    assert_eq!(listed_ids(&list.json), vec![twin_item], "{}", list.body);
}
