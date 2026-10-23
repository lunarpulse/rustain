//! `K02` (AC2, `B3`; ownership-on-rebind row 2) and `K03` (AC3, `A4`; row
//! 11) — adoption is opt-in; the bound caller's own task wins over an adopted
//! one with the same id; two adopted owners of one id answer not-found.
//!
//! Mutants this module turns RED (story MUTANT REGISTER): `M05` (every slot
//! bound to the pin adopts) at AC2; `M02` (adopted handles before the exact
//! one) and `M10` (the exact hit served only when terminal) at AC3(i); `M03`
//! (the first of two adopted hits served) at AC3(ii). Every not-found leg has
//! its in-test positive control: the per-id reseed proof after the restart.

use rustain::adapters::a2a::exec::{CANCEL_DETAIL, RESTART_DETAIL};
use serde_json::json;

use super::harness::{
    BoundHost, K_P1A, K_P1A_ENV, K_P1B, K_P1B_ENV, ORPHAN_WARN, PENDING_WARN, SETTLE, adopting,
    detail, lines_with, log_cursor, log_since, park_in_flight, send_and_admit, server, setup, spec,
    wire_state,
};
use super::{assert_never_bytes, assert_restart_projection};

/// `K02` — a binding without `adoptPreBinding` adopts nothing: both keys of
/// the pin answer the original key's recovered tasks as never-existing.
#[tokio::test(flavor = "multi_thread")]
async fn k02_a_binding_without_adoption_answers_not_found() {
    setup();
    const HELD: &str = "e02-held";
    const PENDING: &str = "e02-pending";
    let mut host = BoundHost::start(spec(server("ask", json!({}), &[K_P1A_ENV]))).await;
    let mut operator = host.attach_operator().await;
    send_and_admit(&host, &mut operator, &host.client(K_P1A), HELD).await;
    park_in_flight(&host, &host.client(K_P1A), PENDING).await;
    drop(operator);

    let cursor = log_cursor();
    let host = host
        .restart_after_crash(spec(server(
            "ask",
            json!({ K_P1A_ENV: "p1", K_P1B_ENV: "p1" }),
            &[],
        )))
        .await;
    for (warn, task) in [(PENDING_WARN, PENDING), (ORPHAN_WARN, HELD)] {
        assert_eq!(
            lines_with(cursor, warn, task),
            1,
            "AC2 reseed {task}: restored exactly once by its pass:\n{}",
            log_since(cursor)
        );
    }

    for (who, key) in [("K_P1A", K_P1A), ("K_P1B", K_P1B)] {
        let client = host.client(key);
        for task in [PENDING, HELD] {
            assert_never_bytes(&client, 10, task, None, &format!("AC2 {who}")).await;
        }
    }
}

/// `K03`(i) — the bound caller's own live task with an adopted id wins, for
/// `tasks/get` and for `tasks/cancel`.
#[tokio::test(flavor = "multi_thread")]
async fn k03_the_bound_callers_own_task_wins_over_an_adopted_id() {
    setup();
    const DUP: &str = "e03-dup";
    // An attached operator holds every parked approval: an unattended `ask`
    // host declines it (the daemon's headless approval gate).
    let mut host = BoundHost::start(spec(server("ask", json!({}), &[K_P1A_ENV]))).await;
    let operator = host.attach_operator().await;
    park_in_flight(&host, &host.client(K_P1A), DUP).await;
    drop(operator);

    let cursor = log_cursor();
    let mut host = host
        .restart_after_crash(spec(server(
            "ask",
            json!({ K_P1A_ENV: adopting("p1"), K_P1B_ENV: "p1" }),
            &[],
        )))
        .await;
    let _operator = host.attach_operator().await;
    assert_eq!(
        lines_with(cursor, PENDING_WARN, DUP),
        1,
        "AC3(i) reseed {DUP}:\n{}",
        log_since(cursor)
    );

    // Minting stays exact: K_P1A, now bound, parks a new task with the id.
    let sent = host
        .client(K_P1A)
        .message_send(1, DUP, DUP, json!({}))
        .await;
    assert_eq!(
        wire_state(&sent),
        "auth-required",
        "AC3(i) the bound resend parks: {}",
        sent.body
    );

    for (who, key) in [("K_P1A", K_P1A), ("K_P1B", K_P1B)] {
        let got = host.client(key).tasks_get(10, DUP, None).await;
        assert_eq!(
            wire_state(&got),
            "auth-required",
            "AC3(i) {who} tasks/get {DUP}: the bound task wins: {}",
            got.body
        );
        assert_ne!(
            detail(&got),
            Some(RESTART_DETAIL),
            "AC3(i) {who} tasks/get {DUP}: not the recovered projection: {}",
            got.body
        );
    }

    let canceled = host.client(K_P1B).tasks_cancel(11, DUP, None).await;
    assert_eq!(
        wire_state(&canceled),
        "canceled",
        "AC3(i) K_P1B tasks/cancel {DUP}: the bound task is canceled: {}",
        canceled.body
    );
    assert_eq!(
        detail(&canceled),
        Some(CANCEL_DETAIL),
        "AC3(i) K_P1B tasks/cancel {DUP}: {}",
        canceled.body
    );
}

/// The pending file's records for `task`, waited for until there are `count`
/// of them (bounded): `wait_pending_record` matches a substring, so the
/// second record of one id needs its own wait.
async fn pending_submitters(host: &BoundHost, task: &str, count: usize) -> Vec<String> {
    let path = host.journal.path.join(".rustain/a2a-pending.json");
    let deadline = tokio::time::Instant::now() + SETTLE;
    loop {
        let submitters: Vec<String> = std::fs::read(&path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<Vec<serde_json::Value>>(&raw).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|record| record["task_id"] == task)
            .filter_map(|record| record["submitter"].as_str().map(str::to_owned))
            .collect();
        if submitters.len() >= count {
            return submitters;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "AC3(ii) seed: {count} pending records for {task}, got {submitters:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// `K03`(ii) — two adopted keys parked one id: neither key is served either
/// task; a single adopted task of the same pin is (the positive control).
#[tokio::test(flavor = "multi_thread")]
async fn k03_two_adopted_owners_of_one_id_answer_not_found() {
    setup();
    const AMB: &str = "e03-amb";
    const SOLO: &str = "e03-solo";
    let mut host = BoundHost::start(spec(server("ask", json!({}), &[K_P1A_ENV, K_P1B_ENV]))).await;
    let operator = host.attach_operator().await;
    park_in_flight(&host, &host.client(K_P1A), AMB).await;
    park_in_flight(&host, &host.client(K_P1B), AMB).await;
    park_in_flight(&host, &host.client(K_P1A), SOLO).await;
    let submitters = pending_submitters(&host, AMB, 2).await;
    assert!(
        submitters.len() == 2 && submitters[0] != submitters[1],
        "AC3(ii) seed: two {AMB} records under two handles before the restart: {submitters:?}"
    );
    drop(operator);

    let cursor = log_cursor();
    let host = host
        .restart_after_crash(spec(server(
            "ask",
            json!({ K_P1A_ENV: adopting("p1"), K_P1B_ENV: adopting("p1") }),
            &[],
        )))
        .await;
    for (task, count) in [(AMB, 2), (SOLO, 1)] {
        assert_eq!(
            lines_with(cursor, PENDING_WARN, task),
            count,
            "AC3(ii) reseed {task}: one line per record:\n{}",
            log_since(cursor)
        );
    }

    for (who, key) in [("K_P1A", K_P1A), ("K_P1B", K_P1B)] {
        let client = host.client(key);
        assert_never_bytes(&client, 10, AMB, None, &format!("AC3(ii) {who}")).await;
        assert_restart_projection(
            &client.tasks_get(11, SOLO, None).await,
            &format!("AC3(ii) {who} {SOLO}"),
        );
    }
}
