//! `K03` (AC3, `B5`, `A8`, `A18`) and `K04` (AC4, `A7`, `A24`) — front door,
//! host B2 (B1 + the unbound key `K_U`).
//!
//! Mutants this file turns RED: `M14` (K03: `principal_bound(ApiKey)` = true →
//! the `principalBound` substring assertions), `M11` (K04: `admit_thread_access(
//! BoundApiKey, Off)` → `Err` → K_P1A's capability-less read of its legacy
//! `ri_3` is refused), `M12` (K04: `post_lookup(BoundApiKey, Some(hash),
//! matched=false)` → `Serve` → K_P1A's capability-less read of the
//! capability-bearing `ri_4` is served instead of the never-id bytes).

use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::{A2aPeerSource, A2aPeerSpec};

use crate::harness::{
    BoundHost, HostSpec, K_P1A, K_P1A_ENV, K_P1B, K_P2, K_U, Sender, item_id_for_task, listed_ids,
    received_row_for_task, sender_key, served_ids, setup, stranger, wire,
};

const UNBOUND_THREAD_REFUSED: i64 = -32040;
const NOT_FOUND: i64 = -32001;

/// The 19.18c wire shape: an unbound caller's list/thread/retract payload
/// never carries the bound flag (`M14`) and keeps the collapse disclosure
/// `false`.
fn assert_legacy_shape(body: &str, json: &serde_json::Value, what: &str) {
    assert!(!body.contains("principalBound"), "{what}: {body}");
    assert_eq!(
        json["result"]["principalCollapsed"],
        serde_json::Value::Bool(false),
        "{what}: {body}"
    );
}

/// `K03` — an unbound key on a mixed host is exactly the 19.18c caller: its
/// own pseudonym, no `principalBound`, the `threadReads` gate.
#[tokio::test(flavor = "multi_thread")]
async fn k03_an_unbound_key_keeps_the_pre_binding_wire_and_thread_gate() {
    setup();
    // `threadReads` absent: the gate is `off`.
    let mut closed = BoundHost::start(HostSpec::b2()).await;
    let _operator_closed = closed.attach_operator().await;
    let unbound = closed.client(K_U);

    // K_U mints its own legacy item; its journal row is the pre-binding shape
    // (a pseudonym address, no alias).
    let ri_u = closed.send_and_park(&unbound, "u1").await;
    let pseudonym = rustain::adapters::a2a::exec::SubmitterKey::from_api_key(K_U)
        .pseudonymous_peer_id()
        .as_str()
        .to_owned();
    let row = received_row_for_task(&closed.journal, "u1");
    assert_eq!(
        row["address"],
        serde_json::json!({
            "principal": { "kind": "a2a_pseudonym", "peer": pseudonym },
            "item": ri_u,
        }),
        "{row}"
    );
    assert!(row["alias"].is_null(), "{row}");

    // It lists (and only sees) its own item; the body has no `principalBound`.
    let listed = unbound.items_list(2).await;
    assert_eq!(
        listed_ids(&listed.json),
        vec![ri_u.clone()],
        "{}",
        listed.body
    );
    assert_legacy_shape(&listed.body, &listed.json, "K_U list");

    // `threadReads` off: a capability-less read of its own legacy thread is
    // refused with `-32040` BEFORE lookup — the same bytes for an existing and
    // a never-existing id (no existence oracle) …
    let refused = unbound.items_thread(7, &ri_u, None).await;
    assert_eq!(
        refused.json["error"]["code"], UNBOUND_THREAD_REFUSED,
        "{}",
        refused.body
    );
    assert_eq!(
        refused.body,
        unbound.items_thread(7, "ri_never", None).await.body,
        "the refusal precedes the lookup"
    );
    // … while a BOUND key on the very same host is served (the gate flips for
    // bound keys only, `K04`).
    let bound = closed.client(K_P1A);
    let ri_bound = closed.send_and_park(&bound, "b1").await;
    let served = bound.items_thread(7, &ri_bound, None).await;
    assert_eq!(
        served_ids(&served.json),
        vec![ri_bound.clone()],
        "{}",
        served.body
    );

    // Retract: lands, with the 19.18c shape; the item stays K_U's pseudonym
    // item on disk.
    let retracted = unbound.items_retract(9, &ri_u, None).await;
    assert!(retracted.json["error"].is_null(), "{}", retracted.body);
    assert_legacy_shape(&retracted.body, &retracted.json, "K_U retract");
    let marks = crate::harness::raw_payloads(&closed.journal, "recipient_item_retracted");
    assert_eq!(marks.len(), 1, "{marks:?}");
    assert_eq!(marks[0]["address"]["principal"]["kind"], "a2a_pseudonym");
    assert_eq!(marks[0]["address"]["item"], ri_u.as_str());
    // The bound key's list is its own and flagged; K_U's is not (positive
    // control for the substring assertions above).
    let bound_list = bound.items_list(2).await;
    assert_eq!(listed_ids(&bound_list.json), vec![ri_bound.clone()]);
    assert!(
        bound_list.body.contains("principalBound"),
        "{}",
        bound_list.body
    );

    // Opted-in thread reads: the SAME K_U read is served, and that body has no
    // `principalBound` either. This is the positive control for the `-32040`
    // above.
    let mut open =
        BoundHost::start(HostSpec::b2().server_key("threadReads", "credential-scoped".into()))
            .await;
    let _operator_open = open.attach_operator().await;
    let unbound_open = open.client(K_U);
    let ri_open = open.send_and_park(&unbound_open, "u2").await;
    let thread = unbound_open.items_thread(7, &ri_open, None).await;
    assert_eq!(
        served_ids(&thread.json),
        vec![ri_open.clone()],
        "{}",
        thread.body
    );
    assert_legacy_shape(&thread.body, &thread.json, "K_U thread");
    let list = unbound_open.items_list(2).await;
    assert_eq!(listed_ids(&list.json), vec![ri_open.clone()]);
    assert_legacy_shape(&list.body, &list.json, "K_U list (credential-scoped)");
    let retract = unbound_open.items_retract(9, &ri_open, None).await;
    assert!(retract.json["error"].is_null(), "{}", retract.body);
    assert_legacy_shape(
        &retract.body,
        &retract.json,
        "K_U retract (credential-scoped)",
    );
}

/// A roster entry for a production sender whose credential is the key held
/// under `K_P1A`'s env name (story `K04`: "Production sender A").
fn sender_roster(host: &BoundHost) -> Vec<A2aPeerSpec> {
    let spec: A2aPeerSpec = host.credentialed("b", K_P1A_ENV);
    assert!(matches!(spec.source, A2aPeerSource::Workspace));
    vec![spec]
}

/// `K04` — host B2, `threadReads` absent: bound keys read their own LEGACY
/// threads without opting in; the capability stays authoritative for
/// capability-bearing items.
#[tokio::test(flavor = "multi_thread")]
async fn k04_the_thread_gate_flips_for_bound_keys_and_the_capability_stays() {
    setup();
    let mut host = BoundHost::start(HostSpec::b2()).await;
    let _operator = host.attach_operator().await;
    let p1a = host.client(K_P1A);
    let p1b = host.client(K_P1B);
    let p2 = host.client(K_P2);
    let unbound = host.client(K_U);

    // 1. Raw K_P1A: a carrier-less `m3` → legacy `ri_3`; its thread is served
    // with `threadReads` absent (positive control for `M11`) — and so is
    // P1B's (one principal).
    let ri_3 = host.send_and_park(&p1a, "m3").await;
    let row = received_row_for_task(&host.journal, "m3");
    assert!(row["capability_hash"].is_null(), "a legacy item: {row}");
    for (who, client) in [("P1A", &p1a), ("P1B", &p1b)] {
        let served = client.items_thread(7, &ri_3, None).await;
        assert_eq!(
            served_ids(&served.json),
            vec![ri_3.clone()],
            "`M11` ({who}): {}",
            served.body
        );
    }
    // The same request from K_U, on K_U's OWN legacy item, is refused.
    let ri_u = host.send_and_park(&unbound, "u1").await;
    let refused = unbound.items_thread(7, &ri_u, None).await;
    assert_eq!(
        refused.json["error"]["code"], UNBOUND_THREAD_REFUSED,
        "{}",
        refused.body
    );
    // A bound key of ANOTHER pin is not served P1's legacy thread either (the
    // gate flip is not a scope flip).
    let never = p2.items_thread(7, "ri_never", None).await.body;
    assert!(never.contains(&NOT_FOUND.to_string()), "{never}");
    assert_eq!(p2.items_thread(7, &ri_3, None).await.body, never);

    // 2. Production sender A (credentialed to B2 with K_P1A's env name) sends
    // a capability-bearing item `ri_4`.
    let mut sender = Sender::new(sender_roster(&host)).await;
    let key = sender_key(&sender);
    let mut state = TuiState::new(120, 40);
    let sent = sender.send_item(&mut state, "b", &host, "question").await;
    let ri_4 = item_id_for_task(&host.journal, &sent.task);
    assert_eq!(sent.id, ri_4);
    assert!(sent.capability_hash.is_some(), "a capability-bearing item");
    let sent_row = received_row_for_task(&host.journal, &sent.task);
    assert_eq!(
        sent_row["address"]["principal"]["kind"], "a2a_bound",
        "{sent_row}"
    );

    // Raw K_P1A (same principal) with NO capability, a wrong one, and a
    // stranger's: the never-existed bytes (`M12`: the bound class never
    // widens past the capability).
    let never_4 = p1a.items_thread(7, "ri_never", None).await.body;
    assert!(never_4.contains(&NOT_FOUND.to_string()), "{never_4}");
    assert_eq!(
        p1a.items_thread(7, &ri_4, None).await.body,
        never_4,
        "`M12`: no capability on a capability-bearing item"
    );
    assert_eq!(
        p1a.items_thread(7, &ri_4, Some(&stranger(&sent.task)))
            .await
            .body,
        p1a.items_thread(7, "ri_never", Some(&stranger(&sent.task)))
            .await
            .body,
        "`M12`: a wrong capability"
    );
    // Positive control: the sender's own derivation is served to the same raw
    // client, and A's `/team thread b ri_4` renders the thread through the
    // production rail with the gate still `off`.
    let capable = p1a
        .items_thread(7, &ri_4, Some(&wire(&key, &sent.task)))
        .await;
    assert_eq!(
        served_ids(&capable.json),
        vec![ri_4.clone()],
        "{}",
        capable.body
    );
    let block = sender.read_thread_block(&mut state, "b", &ri_4).await;
    assert!(
        block.starts_with(&format!(
            "thread · b · {ri_4}\n  you       {ri_4}\n      question"
        )),
        "{block}"
    );
}
