//! `K05` (AC6, `A11`, `A12`, `P7`–`P10`, `P23`, `F6`) — adoption and the
//! orphan notice, over ONE workspace journal across restarts, every phase
//! read through a phase-local log cursor.
//!
//! Credentials (the harness table): `K` = `K_P1A` (the credential that gets
//! bound), `K2` = `K_P1B` (the second credential of the same pin), the
//! configured-unbound CONTROL = `K_U`, the unrelated bound key = `K_P2`
//! (`p2`). `P` = pin `p1`. Keys reach the server only through the written
//! `a2a.json` + the resolver's env closure.
//!
//! Mutants this keystone turns RED (story MUTANT REGISTER), with the
//! killing arithmetic at each assertion: `M04`, `M20`, `M26`, `M30`–`M35`,
//! `M52`, `M56`, `M57`, `M68`, `M72`.
//!
//! Two tests:
//! * [`k05_orphan_phases_across_restarts`] — phases (0)–(6), the loopback
//!   startup, the merged list, the M68 retract;
//! * [`k05_adoption_follow_up_edges`] — `P7`'s capability-class edge.
//!
//! Story 19.18e moved the recovered-task twin control to its own target
//! (`conformance_19_18e_a2a_task_continuity`): adopted keys' recovered tasks
//! are now served, so the twin's not-found leg is retired.
//!
//! The follow-up edges live in their own workspace on purpose: every follow-up
//! mints an `A2aBound(P)` item, and the main test's exact counts (phase (5) =
//! 4, phase (6) = 5) are over a journal whose only bound item is phase (4)'s
//! `new`.

use super::harness::{
    BoundHost, HostSpec, K_P1A, K_P1A_ENV, K_P1B, K_P1B_ENV, K_P2, K_P2_ENV, K_U, K_U_ENV, Listen,
    P1_X, SETTLE, Sender, adopting, bound_item_address, follow_up_metadata, host_log_since,
    item_id_for_task, items, listed_ids, log_cursor, log_since, park_in_flight, pin_hex,
    raw_payloads, received_row_for_task, received_rows, restart_phase_after_crash, room_events,
    sender_key, served_ids, server, setup, spec, wire,
};
use rustain::adapters::a2a::exec::RESTART_DETAIL;
use rustain::adapters::a2a::server::ORPHANED_RECIPIENT_ITEMS_WARNING;
use rustain::adapters::tui::state::TuiState;
use serde_json::{Value, json};

// ── Config builders (the `server` block of the written `a2a.json`) ──────────

/// `K` and the control unbound, `K2` not configured.
fn unbound_spec(admission: &str) -> HostSpec {
    spec(server(admission, json!({}), &[K_P1A_ENV, K_U_ENV]))
}

/// Phase (2): `K` removed; `K_P2` (`p2`, unrelated) stays bound; the control
/// stays configured unbound.
fn removed_spec() -> HostSpec {
    spec(server("allow", json!({ K_P2_ENV: "p2" }), &[K_U_ENV]))
}

/// Phase (3): `K` bound to `p1`, NO adoption.
fn bound_spec() -> HostSpec {
    spec(server(
        "allow",
        json!({ K_P2_ENV: "p2", K_P1A_ENV: "p1" }),
        &[K_U_ENV],
    ))
}

/// Phase (4): `K` and `K2` bound to `p1`, both with `adoptPreBinding`.
fn adopting_spec() -> HostSpec {
    spec(server(
        "allow",
        json!({
            K_P2_ENV: "p2",
            K_P1A_ENV: adopting("p1"),
            K_P1B_ENV: adopting("p1"),
        }),
        &[K_U_ENV],
    ))
}

/// Phases (5)/(6): `K` and `K2` removed, `K_P2` stays bound, control stays.
fn gone_spec() -> HostSpec {
    removed_spec()
}

// ── Log / journal readers ───────────────────────────────────────────────────

/// The distinctive tail of the warning (everything after `{count}`), taken
/// from the production const so a respelling cannot desynchronise the test.
fn warning_tail() -> &'static str {
    ORPHANED_RECIPIENT_ITEMS_WARNING
        .split("{count}")
        .nth(1)
        .expect("the const carries one `{count}` placeholder")
}

/// Phase assertion: the host's phase-local log carries exactly ONE orphan
/// line whose message is byte-equal to the const with `{count}` replaced by
/// `expected`; `expected == 0` means NO such line at all (absent line = 0).
fn assert_orphans(window: &str, expected: usize, phase: &str) {
    let lines: Vec<&str> = window
        .lines()
        .filter(|line| line.contains(warning_tail()))
        .collect();
    if expected == 0 {
        assert!(
            lines.is_empty(),
            "{phase}: no orphan line expected, got {lines:?}"
        );
        return;
    }
    let message = ORPHANED_RECIPIENT_ITEMS_WARNING.replace("{count}", &expected.to_string());
    assert_eq!(
        lines.len(),
        1,
        "{phase}: exactly one orphan line (count {expected}): {lines:?}"
    );
    assert!(
        lines[0].ends_with(&message),
        "{phase}: the line is the const with count {expected}, byte for byte: {}",
        lines[0]
    );
}

/// Restart `host` into `spec` and return the host plus its phase-local log.
async fn restart_phase(host: BoundHost, spec: HostSpec) -> (BoundHost, String) {
    let cursor = log_cursor();
    let host = host.restart(spec).await;
    let window = host_log_since(cursor, &host);
    (host, window)
}

/// Bounded wait until the production removal frame's tombstone is journaled
/// (the daemon answers a successful removal with no frame).
async fn wait_removed(host: &BoundHost, item: &str) {
    let deadline = tokio::time::Instant::now() + SETTLE;
    loop {
        if raw_payloads(&host.journal, "recipient_item_removed")
            .iter()
            .any(|row| row["address"]["item"] == item)
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the removal of {item} was never journaled"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

fn retracted_rows(host: &BoundHost) -> Vec<Value> {
    raw_payloads(&host.journal, "recipient_item_retracted")
}

/// One listed row by id.
fn listed_row<'a>(list: &'a Value, item: &str) -> &'a Value {
    list["result"]["items"]
        .as_array()
        .expect("listed items")
        .iter()
        .find(|row| row["itemId"] == item)
        .unwrap_or_else(|| panic!("{item} not listed: {list}"))
}

// ── The main keystone ───────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn k05_orphan_phases_across_restarts() {
    setup();
    let pin_p1 = pin_hex(P1_X);

    // ── Phase (0): a LOOPBACK-listener startup; the loopback caller mints
    // `ri_lb` (a loopback-pseudonym item) so it exists in every phase below.
    let mut host =
        BoundHost::start(unbound_spec("allow").listen(Listen::Loopback).tls(false)).await;
    let mut operator = host.attach_operator().await;
    let loopback_caller = host.client_with(None);
    let ri_lb = host.send_and_park(&loopback_caller, "k05-lb").await;
    assert_eq!(
        received_row_for_task(&host.journal, "k05-lb")["address"]["principal"]["kind"],
        "a2a_pseudonym",
        "the loopback caller's item is a pseudonym item"
    );

    // ── Seed, off loopback, `K` unbound: ri_a (live), ri_b (Removed through
    // the operator's production `/team remove` frame), ri_c (K's own item
    // retracted by K through `x-rustain-items/retract`), the control's item.
    drop(operator);
    host = host.restart(unbound_spec("allow")).await;
    operator = host.attach_operator().await;
    let (k, control) = (host.client(K_P1A), host.client(K_U));
    let ri_a = host.send_and_park(&k, "k05-a").await;
    let ri_b = host.send_and_park(&k, "k05-b").await;
    let ri_c = host.send_and_park(&k, "k05-c").await;
    let ri_ctl = host.send_and_park(&control, "k05-ctl").await;
    operator.remove_item(&ri_b).await;
    wait_removed(&host, &ri_b).await;
    let retracted = k.items_retract(9, &ri_c, None).await;
    assert!(retracted.json["error"].is_null(), "{}", retracted.body);
    assert_eq!(retracted.json["result"]["alreadyRetracted"], false);
    let pseudonym_row_a = received_row_for_task(&host.journal, "k05-a");
    assert_eq!(
        pseudonym_row_a["address"]["principal"]["kind"], "a2a_pseudonym",
        "{pseudonym_row_a}"
    );
    assert_eq!(
        received_rows(&host.journal).len(),
        5,
        "ri_lb, ri_a, ri_b, ri_c, ri_ctl and nothing else is projected"
    );

    // ── Separate LOOPBACK startup over ri_lb + K's items + the control:
    // keys are ignored on loopback, so every non-loopback pseudonym item is
    // orphaned: ri_a, ri_b, ri_c, ri_ctl = 4, never ri_lb.
    // `M32` (loopback-mode key items omitted) counts 0; `M20` (counts the
    // loopback pseudonym) counts 5.
    drop(operator);
    let (host_lb, window) = restart_phase(
        host,
        unbound_spec("allow").listen(Listen::Loopback).tls(false),
    )
    .await;
    host = host_lb;
    assert_orphans(&window, 4, "loopback startup");
    let on_loopback = host.client(K_P1A).items_list(2).await;
    assert_eq!(
        listed_ids(&on_loopback.json),
        vec![ri_lb.clone()],
        "keys are ignored on loopback: K is the loopback caller and sees only ri_lb: {}",
        on_loopback.body
    );
    assert_eq!(on_loopback.json["result"]["principalCollapsed"], true);

    // ── Seed K's recoverable in-flight task, off loopback, ask mode: no
    // durable item, so the counts below hold. (Count 0 here too: K and the
    // control are configured unbound.)
    let (host_ask, window) = restart_phase(host, unbound_spec("ask")).await;
    host = host_ask;
    assert_orphans(&window, 0, "ask-mode seeding start");
    let k = host.client(K_P1A);
    park_in_flight(&host, &k, "k05-flight").await;

    // ── Phase (1): K still unbound → 0 (ri_lb present, not counted).
    // `M20` (loopback pseudonym counted): 1. `M31` (configured-unbound items
    // counted as orphans): ri_a+ri_b+ri_c+ri_ctl = 4.
    // `M04` is killed by phases (2)–(6), where the line must exist.
    let flight_cursor = log_cursor();
    // Recorded for the failure message only: this assertion has failed on a
    // 4-vCPU CI runner and nowhere locally, and job logs are not public.
    let pending_before =
        std::fs::read_to_string(host.journal.path.join(".rustain/a2a-pending.json"))
            .unwrap_or_else(|error| format!("<unreadable: {error}>"));
    let (host_1, window) = restart_phase_after_crash(host, unbound_spec("ask")).await;
    host = host_1;
    assert_orphans(&window, 0, "phase (1)");
    assert!(
        received_rows(&host.journal)
            .iter()
            .any(|row| row["address"]["item"] == ri_lb.as_str()),
        "ri_lb is in the journal (and uncounted)"
    );
    // Positive control for the counts: the in-flight task WAS recovered (no
    // item minted for it) — and K still reaches its failed projection.
    let since_crash = log_since(flight_cursor);
    assert!(
        since_crash.contains("A2A approval-pending task failed by host restart"),
        "the pending record was reconciled at this restart — pending file before the crash: \
         {pending_before:?}; A2A/pending log lines since: {:?}",
        since_crash
            .lines()
            .filter(|line| line.contains("A2A") || line.contains("pending"))
            .map(|line| line.chars().take(200).collect::<String>())
            .collect::<Vec<_>>()
    );
    let k = host.client(K_P1A);
    let flight = k.tasks_get(11, "k05-flight", None).await;
    assert_eq!(
        flight.json["result"]["status"]["state"], "failed",
        "{}",
        flight.body
    );
    assert!(
        flight.body.contains(&RESTART_DETAIL[..40]),
        "the failed projection carries the restart detail: {}",
        flight.body
    );
    assert_eq!(
        listed_ids(&k.items_list(2).await.json),
        vec![ri_a.clone(), ri_b.clone(), ri_c.clone()],
        "K (unbound) lists exactly its own three items, never ri_lb"
    );
    assert_eq!(
        received_rows(&host.journal).len(),
        5,
        "the recovered task minted no item"
    );

    // ── Phase (2): K removed, an unrelated bound key (K_P2→p2) remains,
    // the control stays configured → exactly 3 (ri_a, ri_b, ri_c).
    // `M20` (counts ri_lb): 4. `M26` (excludes Removed/retracted): only ri_a
    // is live → 1. `M30` (removed-key items omitted): 0. `M57` (counts
    // journal events, not projected items): ri_a 1 + ri_b 2 (received,
    // removed) + ri_c 2 (received, retracted) = 5.
    let (host_2, window) = restart_phase(host, removed_spec()).await;
    host = host_2;
    assert_orphans(&window, 3, "phase (2)");
    let retired = host.client(K_P1A).items_list(2).await;
    assert!(
        retired.json["result"].is_null(),
        "K's retired key reaches nothing: {}",
        retired.body
    );
    // Positive control: the configured-unbound control is served its item.
    assert_eq!(
        listed_ids(&host.client(K_U).items_list(2).await.json),
        vec![ri_ctl.clone()]
    );

    // ── Phase (3): K bound to P WITHOUT adoption → exactly 3, and every item
    // verb on K's items is never-id-equivalent.
    // `M20`: 4. `M26`: 1. `M33`–`M35` and `M52` need adoption → phase (4).
    let (host_3, window) = restart_phase(host, bound_spec()).await;
    host = host_3;
    assert_orphans(&window, 3, "phase (3)");
    let k_bound = host.client(K_P1A);
    let list_3 = k_bound.items_list(2).await;
    assert_eq!(
        list_3.json["result"]["principalBound"], true,
        "{}",
        list_3.body
    );
    assert!(
        listed_ids(&list_3.json).is_empty(),
        "K, bound without adoption, lists none of its old items: {}",
        list_3.body
    );
    let never_thread = k_bound.items_thread(7, "ri_never", None).await.body;
    assert!(never_thread.contains("-32001"), "{never_thread}");
    let never_retract = k_bound.items_retract(9, "ri_never", None).await.body;
    assert!(never_retract.contains("-32001"), "{never_retract}");
    let never_follow = k_bound
        .message_send(8, "k05-follow", "more", follow_up_metadata("ri_never"))
        .await
        .body;
    assert!(never_follow.contains("-32001"), "{never_follow}");
    let rows_before = room_events(&host.journal).await.len();
    for old in [&ri_a, &ri_b, &ri_c] {
        assert_eq!(
            k_bound.items_thread(7, old, None).await.body,
            never_thread,
            "{old}"
        );
        assert_eq!(
            k_bound.items_retract(9, old, None).await.body,
            never_retract,
            "{old}"
        );
        assert_eq!(
            k_bound
                .message_send(8, "k05-follow", "more", follow_up_metadata(old))
                .await
                .body,
            never_follow,
            "{old}"
        );
    }
    assert_eq!(
        room_events(&host.journal).await.len(),
        rows_before,
        "no never-id-equivalent probe wrote a row"
    );

    // ── Phase (4): K and K2 bound to P with adoptPreBinding → exactly 0,
    // and all three legacy items are served through K2.
    // `M20`/`M26`/`M31`: n/a (0 expected — any miscount is a line).
    let (host_4, window) = restart_phase(host, adopting_spec()).await;
    host = host_4;
    assert_orphans(&window, 0, "phase (4)");
    let operator = host.attach_operator().await;
    let k2 = host.client(K_P1B);
    let k_again = host.client(K_P1A);
    let legacy = vec![ri_a.clone(), ri_b.clone(), ri_c.clone()];
    // `M33` (list ignores scope): K2's own scope would be empty.
    let list_legacy = k2.items_list(2).await;
    assert_eq!(
        listed_ids(&list_legacy.json),
        legacy,
        "{}",
        list_legacy.body
    );
    assert_eq!(list_legacy.json["result"]["principalBound"], true);
    assert_eq!(list_legacy.json["result"]["principalCollapsed"], false);
    assert_eq!(listed_row(&list_legacy.json, &ri_a)["state"], "received");
    assert_eq!(listed_row(&list_legacy.json, &ri_b)["state"], "removed");
    assert!(listed_row(&list_legacy.json, &ri_a)["retractedAtMs"].is_null());
    assert!(
        listed_row(&list_legacy.json, &ri_c)["retractedAtMs"].is_i64(),
        "the retracted row carries retractedAtMs: {}",
        list_legacy.body
    );
    // The thread verb through K2 (`M33`-class scope omissions, and a Removed
    // root is a tombstone, never not-found).
    for item in &legacy {
        let thread = k2.items_thread(7, item, None).await;
        assert_eq!(
            served_ids(&thread.json),
            vec![item.clone()],
            "{}",
            thread.body
        );
        assert_eq!(thread.json["result"]["principalBound"], true);
    }
    // Retract through K2: ri_b and ri_c refuse/answer exactly as they do for
    // any caller (`M35`: ri_a below would not even resolve).
    let retract_b = k2.items_retract(9, &ri_b, None).await;
    assert!(
        retract_b.body.contains("-32041"),
        "the Removed tombstone refuses a retract: {}",
        retract_b.body
    );
    let retract_c = k2.items_retract(9, &ri_c, None).await;
    assert!(retract_c.json["error"].is_null(), "{}", retract_c.body);
    assert_eq!(
        retract_c.json["result"]["alreadyRetracted"], true,
        "{}",
        retract_c.body
    );
    // A new bound item; the merged list is ONE journal-ordered set.
    // `M52` (per-scope concatenation): bound items first → [new, ri_a, ...];
    // duplicates would also repeat an id.
    let new = host.send_and_park(&k2, "k05-new").await;
    assert_eq!(
        received_row_for_task(&host.journal, "k05-new")["address"],
        bound_item_address(&new, &pin_p1)
    );
    let merged = vec![ri_a.clone(), ri_b.clone(), ri_c.clone(), new.clone()];
    for caller in [&k2, &k_again] {
        let list = caller.items_list(2).await;
        let ids = listed_ids(&list.json);
        assert_eq!(ids, merged, "{}", list.body);
        let unique: std::collections::BTreeSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "no duplicate ids: {}", list.body);
    }
    // The unrelated bound key sees none of it (control for the scope).
    assert!(listed_ids(&host.client(K_P2).items_list(2).await.json).is_empty());

    // Positive control for the count: restart on the SAME config with the
    // bound item present → still 0 (`P` is bound; `M72` would count it).
    drop(operator);
    let (host_4b, window) = restart_phase(host, adopting_spec()).await;
    host = host_4b;
    assert_orphans(&window, 0, "phase (4), bound item present");
    let k2 = host.client(K_P1B);

    // ── Phase (4) ends: K2 retracts adopted ri_a (`M68`, `F6`).
    let marks_before = retracted_rows(&host).len();
    let retract_a = k2.items_retract(9, &ri_a, None).await;
    assert!(
        retract_a.json["error"].is_null(),
        "`M68`: {}",
        retract_a.body
    );
    assert_eq!(retract_a.json["result"]["alreadyRetracted"], false);
    assert_eq!(retract_a.json["result"]["principalBound"], true);
    let list_after = k2.items_list(2).await;
    assert!(
        listed_row(&list_after.json, &ri_a)["retractedAtMs"].is_i64(),
        "`M68`: the next list shows retractedAtMs on ri_a: {}",
        list_after.body
    );
    let repeat = k2.items_retract(9, &ri_a, None).await;
    assert_eq!(
        repeat.json["result"]["alreadyRetracted"], true,
        "`M68`: a repeat retract is idempotent: {}",
        repeat.body
    );
    let marks = retracted_rows(&host);
    assert_eq!(marks.len(), marks_before + 1, "{marks:?}");
    let mark = marks.last().expect("the new mark");
    assert_eq!(
        mark["address"], pseudonym_row_a["address"],
        "`M68`: the mark is journaled under ri_a's STORED a2a_pseudonym address: {mark}"
    );
    assert_eq!(mark["address"]["principal"]["kind"], "a2a_pseudonym");
    assert_ne!(
        mark["address"],
        bound_item_address(&ri_a, &pin_p1),
        "never a rebuilt bound address"
    );

    // ── Phase (5): off loopback, K and K2 removed, K_P2 stays bound to p2,
    // the control stays configured → exactly 4: ri_a, ri_b, ri_c and phase
    // (4)'s new bound item (P has no bound slot any more).
    // `M72` (every bound item reachable): 3. `M20`: 5.
    let (host_5, window) = restart_phase(host, gone_spec()).await;
    host = host_5;
    assert_orphans(&window, 4, "phase (5)");

    // ── Phase (6): the same config on a LOOPBACK listener → exactly 5: adds
    // the control's item (keys are ignored there); ri_lb stays excluded.
    // `M72`: 4 (bound items counted reachable). `M32`: 0.
    let (host_6, window) =
        restart_phase(host, gone_spec().listen(Listen::Loopback).tls(false)).await;
    host = host_6;
    assert_orphans(&window, 5, "phase (6)");
    drop(host);
}

// ── P7: adoption keeps the 19.18c capability-class edge ─────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn k05_adoption_follow_up_edges() {
    setup();
    let pin_p1 = pin_hex(P1_X);
    let mut host = BoundHost::start(spec(server("allow", json!({}), &[K_P1A_ENV]))).await;
    let mut operator = host.attach_operator().await;
    let ri_a = host.send_and_park(&host.client(K_P1A), "k05-edge-a").await;
    drop(operator);
    host = host
        .restart(spec(server(
            "allow",
            json!({ K_P1A_ENV: adopting("p1"), K_P1B_ENV: adopting("p1") }),
            &[],
        )))
        .await;
    operator = host.attach_operator().await;
    let k2 = host.client(K_P1B);

    // Raw legacy follow-up through K2 joins ri_a's thread (`M34`: the
    // reference resolves in scope; `M56` keeps legacy → legacy joined).
    let raw = k2
        .message_send(8, "k05-edge-raw", "more", follow_up_metadata(&ri_a))
        .await;
    assert!(raw.json["error"].is_null(), "`M34`: {}", raw.body);
    let raw_row = received_row_for_task(&host.journal, "k05-edge-raw");
    assert_eq!(raw_row["in_reply_to"], ri_a.as_str(), "{raw_row}");
    let raw_id = item_id_for_task(&host.journal, "k05-edge-raw");
    assert_eq!(raw_row["address"], bound_item_address(&raw_id, &pin_p1));
    let thread = k2.items_thread(7, &ri_a, None).await;
    assert_eq!(
        served_ids(&thread.json),
        vec![ri_a.clone(), raw_id.clone()],
        "{}",
        thread.body
    );

    // A production capability-bearing follow-up (the production sender) is
    // accepted, but starts its OWN root: thread(ri_a) excludes it, and
    // reading the child returns the child (`M56`: a walk that ignores the
    // class edge would include it in ri_a's thread).
    let mut sender = Sender::new(vec![host.credentialed("b", K_P1B_ENV)]).await;
    let mut state = TuiState::new(120, 40);
    sender
        .type_command(&mut state, &format!("/team followup b {ri_a} capped"))
        .await;
    sender.settle_send(&mut state).await;
    let child = items(&room_events(&host.journal).await)
        .into_iter()
        .find(|item| item.content.contains("capped"))
        .expect("the capability-bearing follow-up was accepted");
    assert_eq!(
        child.in_reply_to.as_deref(),
        Some(ri_a.as_str()),
        "{child:?}"
    );
    assert!(
        child.capability_hash.is_some(),
        "capability-bearing: {child:?}"
    );
    let thread = k2.items_thread(7, &ri_a, None).await;
    assert_eq!(
        served_ids(&thread.json),
        vec![ri_a.clone(), raw_id.clone()],
        "`M56`: thread(ri_a) excludes the capability-bearing child: {}",
        thread.body
    );
    let capability = wire(&sender_key(&sender), &child.task);
    let own = k2.items_thread(7, &child.id, Some(&capability)).await;
    assert_eq!(
        served_ids(&own.json),
        vec![child.id.clone()],
        "reading the child returns its own root: {}",
        own.body
    );
    assert_eq!(
        own.json["result"]["root"],
        child.id.as_str(),
        "{}",
        own.body
    );
    // Without the capability the bound caller still gets the not-found bytes.
    let never = k2.items_thread(7, "ri_never", None).await.body;
    assert!(never.contains("-32001"), "{never}");
    assert_eq!(k2.items_thread(7, &child.id, None).await.body, never);
    drop(operator);
}
