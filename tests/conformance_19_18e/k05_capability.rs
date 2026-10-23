//! `K05` (AC5, `A5`; ownership-on-rebind row 8) — an adopted
//! capability-bearing task still needs its capability: the binding alone
//! reaches only capability-less (legacy) tasks.
//!
//! Mutants this module turns RED (story MUTANT REGISTER): `M06` (adopted
//! tasks skip the capability) at the no-capability leg; the inherited
//! `19.18d-M12`, `19.18c-M10` and `19.18c-M26-pending` through their `also`
//! segment on the same leg.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rustain::adapters::a2a::ITEM_CAPABILITY_METADATA_KEY;
use serde_json::json;

use super::harness::{
    BoundHost, K_P1A, K_P1A_ENV, K_P1B, K_P1B_ENV, PENDING_WARN, adopting, lines_with, log_cursor,
    log_since, server, setup, spec, stranger, wait_pending_record, wire_state,
};
use super::{assert_never_bytes, assert_restart_projection};

#[tokio::test(flavor = "multi_thread")]
async fn k05_an_adopted_capability_bearing_task_needs_its_capability() {
    setup();
    const TASK: &str = "e05-cap";
    let cap = URL_SAFE_NO_PAD.encode([0x5C; 32]);
    let wrong = stranger(TASK);

    // An attached operator holds the parked approval: an unattended `ask`
    // host declines it (the daemon's headless approval gate).
    let mut host = BoundHost::start(spec(server("ask", json!({}), &[K_P1A_ENV]))).await;
    let operator = host.attach_operator().await;
    let sent = host
        .client(K_P1A)
        .message_send(1, TASK, TASK, json!({ ITEM_CAPABILITY_METADATA_KEY: cap }))
        .await;
    assert_eq!(
        wire_state(&sent),
        "auth-required",
        "AC5 seed: {}",
        sent.body
    );
    wait_pending_record(&host, TASK).await;
    let path = host.journal.path.join(".rustain/a2a-pending.json");
    let records: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(&path).expect("AC5 seed: the pending file"))
            .expect("AC5 seed: the pending file is a JSON array");
    assert!(
        records
            .iter()
            .any(|record| record["task_id"] == TASK && record["capability_hash"].is_string()),
        "AC5 seed: the pending record carries the capability hash: {records:?}"
    );
    drop(operator);

    let cursor = log_cursor();
    let host = host
        .restart_after_crash(spec(server(
            "ask",
            json!({ K_P1A_ENV: adopting("p1"), K_P1B_ENV: "p1" }),
            &[],
        )))
        .await;
    assert_eq!(
        lines_with(cursor, PENDING_WARN, TASK),
        1,
        "AC5 reseed {TASK}:\n{}",
        log_since(cursor)
    );

    for (who, key) in [("K_P1A", K_P1A), ("K_P1B", K_P1B)] {
        let client = host.client(key);
        assert_never_bytes(&client, 10, TASK, None, &format!("AC5 {who} no capability")).await;
        assert_never_bytes(
            &client,
            11,
            TASK,
            Some(&wrong),
            &format!("AC5 {who} wrong capability"),
        )
        .await;
        assert_restart_projection(
            &client.tasks_get(12, TASK, Some(&cap)).await,
            &format!("AC5 {who} tasks/get with the capability"),
        );
        assert_restart_projection(
            &client.tasks_cancel(13, TASK, Some(&cap)).await,
            &format!("AC5 {who} tasks/cancel with the capability"),
        );
    }
}
