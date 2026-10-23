//! `K02` (AC2, `A5`–`A8`, `A16`, `A21`; behaviour half of `K13`) — bound
//! principals on a TLS listener.
//!
//! Front door: host B1 (bound-only: `peerKeys = {K_P1A→p1, K_P1B→p1,
//! K_P1C→p1-alt, K_P2→p2}`, `admission: allow`, `NotifyAndWait`, operator
//! attached) is written as an `a2a.json`, parsed by both production parsers,
//! resolved by `resolve_server_auth` and served by `serve(ServeConfig…)` on
//! `0.0.0.0:0` with TLS; every caller is a raw JSON-RPC client anchored on the
//! host certificate presenting its own `x-api-key`.
//!
//! Mutants this keystone turns RED (story MUTANT REGISTER): `M09` (phase 1,
//! the on-disk address), `M10` (phase 4, P1B's thread/follow-up of P1A's
//! item), `M13` (every `principalCollapsed` assertion), `M24` (the harness
//! start: a bound-only host must be network-ready; asserted again here),
//! `M25` (phase 4, `tasks/get m1` through K_P1B), `M28` (phase 5, the
//! bound `TurnAddress`), `M54` (phase 1, the `p1-alt` alias), `M66` (phase 1,
//! bound `ri_1` journals `a2a_bound`; phase 2, P1B's list contains it).
//! `K13` (all non-test principal-forming sites) is proven by this behaviour
//! half: any site that falls back to a pseudonym splits P1A from P1B or from
//! the journal address below.

use super::harness::{
    BoundHost, HostSpec, K_P1A, K_P1B, K_P1C, K_P2, P1_X, P2_X, Reply, bound_item_address,
    bound_turn_address, follow_up_metadata, frame_turn, item_id_for_task, listed_ids, pin_hex,
    raw_payloads, received_row_for_task, replied_rows, served_ids, setup,
};

/// Story 19.18d `A8`/`P12`: a bound caller's successful list/thread/retract
/// payload carries `principalBound: true` AND `principalCollapsed: false`
/// (`M13`: collapsed is true only for loopback).
fn assert_bound_flags(reply: &Reply, what: &str) {
    assert_eq!(
        reply.json["result"]["principalBound"],
        serde_json::Value::Bool(true),
        "{what}: {}",
        reply.body
    );
    assert_eq!(
        reply.json["result"]["principalCollapsed"],
        serde_json::Value::Bool(false),
        "{what}: {}",
        reply.body
    );
}

fn retracted_rows(host: &BoundHost) -> Vec<serde_json::Value> {
    raw_payloads(&host.journal, "recipient_item_retracted")
}

/// `K02` — one host, four credentials, two pins, one shared principal.
#[tokio::test(flavor = "multi_thread")]
async fn k02_bound_keys_are_per_pin_principals_across_every_verb() {
    setup();
    let mut host = BoundHost::start(HostSpec::b1()).await;
    let mut operator = host.attach_operator().await;
    // `M24`: a bound-only host is network-ready (the harness also asserts the
    // `run_inner` bind gate on every wildcard start).
    assert!(host.security.is_network_ready());
    let (p1a, p1b, p1c, p2) = (
        host.client(K_P1A),
        host.client(K_P1B),
        host.client(K_P1C),
        host.client(K_P2),
    );
    let (pin_p1, pin_p2) = (pin_hex(P1_X), pin_hex(P2_X));
    assert_ne!(pin_p1, pin_p2);

    // ── 1. Three senders, three items; exact on-disk wire addresses and the
    // alias each credential's slot selected.
    let ri_1 = host.send_and_park(&p1a, "m1").await;
    let ri_2 = host.send_and_park(&p2, "m2").await;
    let ri_alias = host.send_and_park(&p1c, "m_alias").await;
    for (task, item, pin, alias) in [
        ("m1", &ri_1, &pin_p1, "p1"),
        ("m2", &ri_2, &pin_p2, "p2"),
        // ⛔ `M54`: the row records the SELECTED alias `p1-alt`, never a
        // canonical pin-wide alias (`p1`).
        ("m_alias", &ri_alias, &pin_p1, "p1-alt"),
    ] {
        let row = received_row_for_task(&host.journal, task);
        assert_eq!(
            row["address"],
            bound_item_address(item, pin),
            "{task}: {row}"
        );
        assert_eq!(row["alias"], alias, "{task}: {row}");
    }
    assert_eq!(
        [ri_1.as_str(), ri_alias.as_str()]
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        2,
        "two distinct items for the two credentials of one pin"
    );

    // ── 2. Lists: one pin = one principal; P2 is a different one.
    let list_p1a = p1a.items_list(2).await;
    assert_eq!(
        listed_ids(&list_p1a.json),
        vec![ri_1.clone(), ri_alias.clone()]
    );
    assert_bound_flags(&list_p1a, "P1A list");
    let list_p1b = p1b.items_list(2).await;
    assert_eq!(
        listed_ids(&list_p1b.json),
        vec![ri_1.clone(), ri_alias.clone()],
        "`M66`/`M25`: P1B lists P1A's items (and the `p1-alt` one): {}",
        list_p1b.body
    );
    assert_bound_flags(&list_p1b, "P1B list");
    let list_p1c = p1c.items_list(2).await;
    assert_eq!(
        listed_ids(&list_p1c.json),
        vec![ri_1.clone(), ri_alias.clone()]
    );
    // Positive control: P2 sees its own `ri_2`, and only that.
    let list_p2 = p2.items_list(2).await;
    assert_eq!(
        listed_ids(&list_p2.json),
        vec![ri_2.clone()],
        "{}",
        list_p2.body
    );
    assert_bound_flags(&list_p2, "P2 list");
    for foreign in [&ri_1, &ri_alias] {
        assert!(!list_p2.body.contains(foreign.as_str()), "{}", list_p2.body);
    }

    // ── 3. P2 cannot read, follow, retract, poll or cancel P1's items; every
    // miss is byte-identical to a never-existing id under the same JSON-RPC id.
    let rows_before = super::harness::room_events(&host.journal).await.len();
    let never_thread = p2.items_thread(7, "ri_never", None).await.body;
    assert!(never_thread.contains("-32001"), "{never_thread}");
    let never_retract = p2.items_retract(9, "ri_never", None).await.body;
    assert!(never_retract.contains("-32001"), "{never_retract}");
    let never_get = p2.tasks_get(11, "never-task", None).await.body;
    assert!(never_get.contains("-32001"), "{never_get}");
    let never_cancel = p2.tasks_cancel(12, "never-task", None).await.body;
    assert!(never_cancel.contains("-32001"), "{never_cancel}");
    let never_follow = p2
        .message_send(8, "p2-follow", "text", follow_up_metadata("ri_never"))
        .await
        .body;
    assert!(never_follow.contains("-32001"), "{never_follow}");
    for foreign in [&ri_1, &ri_alias] {
        assert_eq!(p2.items_thread(7, foreign, None).await.body, never_thread);
        assert_eq!(p2.items_retract(9, foreign, None).await.body, never_retract);
        assert_eq!(
            p2.message_send(8, "p2-follow", "text", follow_up_metadata(foreign))
                .await
                .body,
            never_follow
        );
    }
    for task in ["m1", "m_alias"] {
        assert_eq!(p2.tasks_get(11, task, None).await.body, never_get, "{task}");
        assert_eq!(
            p2.tasks_cancel(12, task, None).await.body,
            never_cancel,
            "{task}"
        );
    }
    // … and the other direction: P1 cannot reach P2's `ri_2`/`m2`.
    assert_eq!(p1a.items_thread(7, &ri_2, None).await.body, never_thread);
    assert_eq!(p1a.tasks_get(11, "m2", None).await.body, never_get);
    assert_eq!(
        super::harness::room_events(&host.journal).await.len(),
        rows_before,
        "no refused probe wrote a journal row"
    );
    assert!(retracted_rows(&host).is_empty(), "nothing was retracted");

    // ── 4. P1B is the same principal as P1A: it threads, follows up, polls.
    let thread_p1a = p1a.items_thread(7, &ri_1, None).await;
    assert_eq!(
        served_ids(&thread_p1a.json),
        vec![ri_1.clone()],
        "{}",
        thread_p1a.body
    );
    assert_bound_flags(&thread_p1a, "P1A thread");
    let thread_p1b = p1b.items_thread(7, &ri_1, None).await;
    assert_eq!(
        served_ids(&thread_p1b.json),
        vec![ri_1.clone()],
        "`M10`: P1B threads P1A's item: {}",
        thread_p1b.body
    );
    assert_bound_flags(&thread_p1b, "P1B thread");
    let got = p1b.tasks_get(11, "m1", None).await;
    assert!(
        got.json["error"].is_null(),
        "`M25`: P1B polls m1: {}",
        got.body
    );
    assert_eq!(got.json["result"]["id"], "m1", "{}", got.body);
    let follow = p1b
        .message_send(8, "p1b-follow", "more", follow_up_metadata(&ri_1))
        .await;
    assert!(follow.json["error"].is_null(), "`M10`: {}", follow.body);
    let follow_row = received_row_for_task(&host.journal, "p1b-follow");
    assert_eq!(follow_row["in_reply_to"], ri_1.as_str(), "{follow_row}");
    let follow_id = item_id_for_task(&host.journal, "p1b-follow");
    // The follow-up is journaled under the SAME pin, with P1B's own alias.
    assert_eq!(
        follow_row["address"],
        bound_item_address(&follow_id, &pin_p1),
        "{follow_row}"
    );
    assert_eq!(follow_row["alias"], "p1", "{follow_row}");
    // P1A reads the whole thread, follow-up included.
    let thread_after = p1a.items_thread(7, &ri_1, None).await;
    assert_eq!(
        served_ids(&thread_after.json),
        vec![ri_1.clone(), follow_id.clone()],
        "{}",
        thread_after.body
    );
    // P1A sees the `p1-alt` item as the same principal; its journal row keeps
    // the credential's own alias.
    let alias_thread = p1a.items_thread(7, &ri_alias, None).await;
    assert_eq!(served_ids(&alias_thread.json), vec![ri_alias.clone()]);
    let alias_get = p1a.tasks_get(11, "m_alias", None).await;
    assert!(alias_get.json["error"].is_null(), "{}", alias_get.body);
    assert_eq!(
        received_row_for_task(&host.journal, "m_alias")["alias"],
        "p1-alt"
    );

    // ── 5. The operator reply path writes a BOUND turn address and P1B reads
    // it in the thread (`M28`).
    let frame = operator.reply(&ri_1, "answer one").await;
    let rr_1 = frame_turn(&frame);
    let replies = replied_rows(&host.journal);
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(
        replies[0]["address"],
        bound_turn_address(&rr_1, &pin_p1),
        "`M28`: {}",
        replies[0]
    );
    assert_eq!(replies[0]["answers"], ri_1.as_str(), "{}", replies[0]);
    let thread_reply = p1b.items_thread(7, &ri_1, None).await;
    assert!(
        served_ids(&thread_reply.json).contains(&rr_1),
        "`M28`: P1B reads the reply turn: {}",
        thread_reply.body
    );
    assert!(
        thread_reply.body.contains("answer one"),
        "{}",
        thread_reply.body
    );
    assert_bound_flags(&thread_reply, "P1B thread with the reply");
    // The answer reaches P1B's poll; it is still closed to P2.
    let answered = p1b.tasks_get(11, "m1", None).await;
    assert!(answered.body.contains("answer one"), "{}", answered.body);
    let denied = p2.tasks_get(11, "m1", None).await;
    assert_eq!(denied.body, never_get);
    assert!(!denied.body.contains("answer one"));
    assert_eq!(p2.items_thread(7, &ri_1, None).await.body, never_thread);

    // ── 6. Retract: P1B retracts P1A's item and P1A retracts the `p1-alt`
    // one (one principal); the mark is journaled at the item's STORED address.
    // Positive control: P2 retracts its own item.
    let retracted_1 = p1b.items_retract(9, &ri_1, None).await;
    assert!(retracted_1.json["error"].is_null(), "{}", retracted_1.body);
    assert_bound_flags(&retracted_1, "P1B retract");
    let retracted_alias = p1a.items_retract(9, &ri_alias, None).await;
    assert!(
        retracted_alias.json["error"].is_null(),
        "{}",
        retracted_alias.body
    );
    assert_bound_flags(&retracted_alias, "P1A retract of the p1-alt item");
    let retracted_2 = p2.items_retract(9, &ri_2, None).await;
    assert!(retracted_2.json["error"].is_null(), "{}", retracted_2.body);
    assert_bound_flags(&retracted_2, "P2 retract");
    let marks = retracted_rows(&host);
    assert_eq!(marks.len(), 3, "{marks:?}");
    for (mark, item, pin) in [
        (&marks[0], &ri_1, &pin_p1),
        (&marks[1], &ri_alias, &pin_p1),
        (&marks[2], &ri_2, &pin_p2),
    ] {
        assert_eq!(mark["address"], bound_item_address(item, pin), "{mark}");
        assert_eq!(mark["principal_collapsed"], false, "{mark}");
    }
    // The foreign retract after the fact is still a never-existed answer.
    assert_eq!(p2.items_retract(9, &ri_1, None).await.body, never_retract);
}
