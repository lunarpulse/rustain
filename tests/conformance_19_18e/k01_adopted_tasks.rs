//! `K01` (AC1, `A2`–`A7`; ownership-on-rebind rows 1 and 3) and its positive
//! control `K01c` — a bound caller with adoption reads the adopted key's tasks
//! recovered at the restart that applied the binding, through BOTH restart
//! passes (the held task's orphan node and the approval-pending task's
//! pending record).
//!
//! Mutants this module turns RED (story MUTANT REGISTER): `M01` (no task
//! adoption) and `M11` (no adopted handle at all) at AC1(a); `M07` (status
//! query journaled under the original submitter) at AC1(e); `M08` (every
//! pin's adopting slot adopts for every pin) at AC1(c). `K01c` is a positive
//! control and names no mutant.

use rustain::adapters::a2a::RECIPIENT_ITEM_METADATA_KEY;
use serde_json::json;

use crate::harness::{
    BoundHost, K_P1A, K_P1A_ENV, K_P1B, K_P1B_ENV, K_P2, K_P2_ENV, K_U, K_U_ENV, ORPHAN_WARN, P1_X,
    PENDING_WARN, adopting, lines_with, listed_ids, log_cursor, log_since, no_live_inbound_node,
    park_in_flight, pin_hex, send_and_admit, send_and_leave_pending, server, setup, spec,
    status_rows, wait_pending_record,
};
use crate::{assert_never_bytes, assert_restart_projection};

/// The seed config: `K_P1A` and `K_U` unbound on an `ask` host.
fn unbound_seed() -> serde_json::Value {
    server("ask", json!({}), &[K_P1A_ENV, K_U_ENV])
}

/// `K01` — `K_P1A` is bound to `p1` with `adoptPreBinding` at the restart
/// that interrupted its tasks; `K_P1B` (same pin) and `K_P1A` both read them.
#[tokio::test(flavor = "multi_thread")]
async fn k01_adopted_recovered_tasks_are_served_through_both_passes() {
    setup();
    const HELD: &str = "e01-held";
    const PENDING: &str = "e01-pending";
    const CONTROL: &str = "e01-control";
    let pin = pin_hex(P1_X);

    // ── Seed (K06's order: the one card answered is the intended one) ──────
    let mut host = BoundHost::start(spec(unbound_seed())).await;
    let mut operator = host.attach_operator().await;
    let ri_held = send_and_admit(&host, &mut operator, &host.client(K_P1A), HELD).await;
    send_and_leave_pending(&host, &host.client(K_P1A), PENDING).await;
    send_and_leave_pending(&host, &host.client(K_U), CONTROL).await;
    wait_pending_record(&host, PENDING).await;
    wait_pending_record(&host, CONTROL).await;

    // ── The binding restart (no seeded id was polled before it) ─────────────
    drop(operator);
    let cursor = log_cursor();
    let host = host
        .restart_after_crash(spec(server(
            "ask",
            json!({ K_P1A_ENV: adopting("p1"), K_P1B_ENV: "p1", K_P2_ENV: "p2" }),
            &[K_U_ENV],
        )))
        .await;
    for (warn, task) in [
        (PENDING_WARN, PENDING),
        (PENDING_WARN, CONTROL),
        (ORPHAN_WARN, HELD),
    ] {
        assert_eq!(
            lines_with(cursor, warn, task),
            1,
            "reseed {task}: restored exactly once by its pass:\n{}",
            log_since(cursor)
        );
    }
    // Regression-only (`P7`(a)): the restart premise — nothing resumed.
    assert!(
        no_live_inbound_node(&host).await,
        "AC1(b) premise: no inbound node is live after the restart"
    );

    // ── (a) both keys of the pin read the recovered projections ─────────────
    for (who, key) in [("K_P1A", K_P1A), ("K_P1B", K_P1B)] {
        let client = host.client(key);
        let pending = client.tasks_get(10, PENDING, None).await;
        assert_restart_projection(&pending, &format!("AC1(a) {who} {PENDING}"));
        assert!(
            pending.json["result"]["metadata"][RECIPIENT_ITEM_METADATA_KEY].is_null(),
            "AC1(a) {who} {PENDING}: an approval-pending task has no item id: {}",
            pending.body
        );
        let held = client.tasks_get(11, HELD, None).await;
        assert_restart_projection(&held, &format!("AC1(a) {who} {HELD}"));
        assert_eq!(
            held.json["result"]["metadata"][RECIPIENT_ITEM_METADATA_KEY],
            ri_held.as_str(),
            "AC1(a) {who} {HELD}: the held task carries its durable item id: {}",
            held.body
        );
        let listed = listed_ids(&client.items_list(12).await.json);
        assert!(
            listed.contains(&ri_held),
            "AC1(a) {who}: the same client lists {ri_held}: {listed:?}"
        );
    }

    // ── (e) one first-observation row per task, naming the bound pin ────────
    for task in [PENDING, HELD] {
        assert_eq!(
            status_rows(&host, task),
            vec![(pin.clone(), Some("pinned".to_owned()))],
            "AC1(e) {task}: one status-query row under the bound pin"
        );
    }

    // ── (b) cancel is a projection read ─────────────────────────────────────
    for (who, key) in [("K_P1A", K_P1A), ("K_P1B", K_P1B)] {
        let client = host.client(key);
        for task in [PENDING, HELD] {
            let canceled = client.tasks_cancel(20, task, None).await;
            assert_restart_projection(&canceled, &format!("AC1(b) {who} cancel {task}"));
            let again = client.tasks_get(21, task, None).await;
            assert_restart_projection(&again, &format!("AC1(b) {who} get-after-cancel {task}"));
        }
    }

    // ── (c) another pin reads none of the three ─────────────────────────────
    let other = host.client(K_P2);
    for task in [PENDING, HELD, CONTROL] {
        assert_never_bytes(&other, 30, task, None, "AC1(c) K_P2").await;
    }

    // ── (d) the unchanged unbound key keeps exactly its own task ────────────
    let control = host.client(K_U);
    assert_restart_projection(
        &control.tasks_get(40, CONTROL, None).await,
        &format!("AC1(d) K_U {CONTROL}"),
    );
    for task in [PENDING, HELD] {
        assert_never_bytes(&control, 41, task, None, "AC1(d) K_U").await;
    }
}

/// `K01c` — the projection exists without adoption: the unchanged unbound
/// config serves `K_P1A` its own recovered task.
#[tokio::test(flavor = "multi_thread")]
async fn k01c_unchanged_unbound_config_serves_the_same_projection() {
    setup();
    const PENDING: &str = "e01c-pending";
    let mut host = BoundHost::start(spec(unbound_seed())).await;
    let operator = host.attach_operator().await;
    park_in_flight(&host, &host.client(K_P1A), PENDING).await;
    drop(operator);
    let cursor = log_cursor();
    let host = host.restart_after_crash(spec(unbound_seed())).await;
    assert_eq!(
        lines_with(cursor, PENDING_WARN, PENDING),
        1,
        "reseed {PENDING}:\n{}",
        log_since(cursor)
    );
    assert_restart_projection(
        &host.client(K_P1A).tasks_get(10, PENDING, None).await,
        &format!("K01c {PENDING}"),
    );
}
