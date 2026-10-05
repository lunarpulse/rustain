//! `K08` (AC8, `A13`, `P18`) — the sender's TUI front door against host B2:
//! `/team retract <peer> <item>` (the confirmation card body) and `/team
//! thread <peer> <item>` (the thread block) omit the co-holder sentence for a
//! sender whose credential is BOUND on the host and retain it for an UNBOUND
//! sender on the same host (the positive control).
//!
//! Both surfaces are reached through the production sender
//! (`A2aEgress::compose`) and the operator's `/team` rail
//! (`submit_message_for_test`); the retract card is the output of
//! `transparency_bridge::team_retract_preview`, raised through
//! `open_team_retract_card` exactly as the event loop does.
//!
//! Mutants this file turns RED (every one per surface, so each atomic mutant
//! has its own failing assertion):
//! * `M43` (retract-card gate ignores `principal_bound`) and `M45`
//!   (`PreviewedItem` parser always `false`) → the bound card assertion in
//!   `k08_the_retract_card_...` shows the sentence.
//! * `M44` (thread-block gate ignores it) and `M46` (`ServedThread` parser
//!   always `false`) → the bound block assertion in
//!   `k08_the_thread_block_...` shows the sentence.
//! * `M16` (both gates render regardless) → both bound assertions.
//! * `M17` (both parsers default `true` when the key is absent) → the
//!   unbound-sender assertions (the sentence is lost), on both surfaces.

use rustain::adapters::tui::handlers::team_command::{
    one_credential_sentence, open_team_retract_card,
};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::AppEvent;
use rustain::domain::models::A2aPeerSpec;

use crate::harness::{
    BoundHost, HostSpec, Item, K_P1A, K_P1A_ENV, K_U, K_U_ENV, Operator, SETTLE, Sender,
    sender_key, setup, wire,
};

/// The roster alias both production senders address the host by.
const PEER: &str = "b";

/// One production sender holding a credential, its TUI state, and the item it
/// sent through `/team send`.
struct Side {
    sender: Sender,
    state: TuiState,
    item: Item,
}

impl Side {
    /// A sender credentialed with the key held under `env`, which sends one
    /// item to `host` (story `K04`: "Production sender A").
    async fn send(host: &BoundHost, env: &str, text: &str) -> Self {
        let spec: A2aPeerSpec = host.credentialed(PEER, env);
        let mut sender = Sender::new(vec![spec]).await;
        let mut state = TuiState::new(120, 40);
        let item = sender.send_item(&mut state, PEER, host, text).await;
        assert!(
            item.capability_hash.is_some(),
            "a capability-bearing item: {item:?}"
        );
        Self {
            sender,
            state,
            item,
        }
    }

    /// Typed `/team retract b <item>` → the real confirm-time read → the
    /// handler raises the card; returns the card body the operator sees.
    /// (`conformance_19_18c_a2a_item_capability.rs:1329-1343`.)
    async fn retract_card(&mut self) -> String {
        let line = format!("/team retract {PEER} {}", self.item.id);
        self.sender.type_command(&mut self.state, &line).await;
        let preview = tokio::time::timeout(SETTLE, async {
            loop {
                match self.sender.domain_rx.recv().await {
                    Some(AppEvent::TeamRetractPreviewReady { preview, .. }) => break preview,
                    Some(_) => {}
                    None => panic!("the domain channel closed before the preview arrived"),
                }
            }
        })
        .await
        .expect("the preview spawn publishes TeamRetractPreviewReady");
        open_team_retract_card(&mut self.state, "conv", preview);
        let pending = self
            .state
            .pending_team_retract
            .take()
            .expect("the confirm-time read raised a card, not a sentence");
        assert!(pending.armed, "an armed card: {}", pending.card);
        pending.card
    }

    /// Typed `/team thread b <item>` rendered through the operator's rail.
    async fn thread_block(&mut self) -> String {
        let item = self.item.id.clone();
        self.sender
            .read_thread_block(&mut self.state, PEER, &item)
            .await
    }
}

/// Host B2 with the bound sender (`K_P1A`'s env name) and the unbound sender
/// (`K_U`'s env name) on it, each having sent one capability-bearing item.
/// The raw wire is checked first, so a failing surface assertion below is
/// attributable to the sender's parser/gate and never to the host: the bound
/// caller's payload carries `principalBound: true`, the unbound caller's
/// carries no such key.
async fn two_senders() -> (BoundHost, Operator, Side, Side) {
    let mut host = BoundHost::start(HostSpec::b2()).await;
    let operator = host.attach_operator().await;
    let bound = Side::send(&host, K_P1A_ENV, "bound question").await;
    let unbound = Side::send(&host, K_U_ENV, "unbound question").await;

    for (who, key, side, flagged) in [
        ("bound", K_P1A, &bound, true),
        ("unbound", K_U, &unbound, false),
    ] {
        let raw = host.client(key);
        let list = raw.items_list(1).await;
        assert_eq!(
            list.body.contains("\"principalBound\":true"),
            flagged,
            "{who} list wire: {}",
            list.body
        );
        let capability = wire(&sender_key(&side.sender), &side.item.task);
        let thread = raw.items_thread(2, &side.item.id, Some(&capability)).await;
        assert_eq!(
            thread.body.contains("\"principalBound\":true"),
            flagged,
            "{who} thread wire: {}",
            thread.body
        );
        assert!(
            flagged || !thread.body.contains("principalBound"),
            "{who} thread wire: {}",
            thread.body
        );
    }
    (host, operator, bound, unbound)
}

/// `K08` (retract surface) — the `/team retract` confirmation card.
#[tokio::test(flavor = "multi_thread")]
async fn k08_the_retract_card_omits_the_co_holder_sentence_only_for_a_bound_sender() {
    setup();
    let (_host, _operator, mut bound, mut unbound) = two_senders().await;
    let sentence = one_credential_sentence(PEER);

    // Positive control first: the UNBOUND sender's card carries the sentence
    // (`M17`: the parser defaults the absent key to bound).
    let unbound_card = unbound.retract_card().await;
    assert!(
        unbound_card.contains(&sentence),
        "`M17`: an unbound sender's retract card keeps the sentence: {unbound_card}"
    );
    assert!(
        unbound_card.starts_with(&format!("Retract on {PEER}'s host\n\n")),
        "{unbound_card}"
    );

    // The BOUND sender's card is the same card minus the sentence (`M43` gate,
    // `M45` parser, `M16` umbrella).
    let bound_card = bound.retract_card().await;
    assert!(
        !bound_card.contains(&sentence),
        "`M43`/`M45`/`M16`: a bound sender's retract card omits the sentence: {bound_card}"
    );
    assert!(
        !bound_card.contains("cannot tell which operator sent"),
        "{bound_card}"
    );
    // Positive anchors: the rest of the card is intact and armed, so the
    // absence above is the gate, not a missing card.
    assert!(
        bound_card.starts_with(&format!("Retract on {PEER}'s host\n\n")),
        "{bound_card}"
    );
    assert!(
        bound_card.contains(&format!("item         {}", bound.item.id)),
        "{bound_card}"
    );
    assert!(bound_card.contains("[y] Retract"), "{bound_card}");
    assert_eq!(
        bound_card
            .replace(&bound.item.id, "<id>")
            .replace(&bound.item.task, "<task>"),
        unbound_card
            .replace(&format!("{sentence}\n\n"), "")
            .replace(&unbound.item.id, "<id>")
            .replace(&unbound.item.task, "<task>"),
        "the two cards differ by exactly the sentence paragraph"
    );
}

/// `K08` (thread surface) — the `/team thread` block.
#[tokio::test(flavor = "multi_thread")]
async fn k08_the_thread_block_omits_the_co_holder_sentence_only_for_a_bound_sender() {
    setup();
    let (_host, _operator, mut bound, mut unbound) = two_senders().await;
    let sentence = one_credential_sentence(PEER);

    // Positive control first: the UNBOUND sender's block ends with the
    // sentence (`M17`).
    let unbound_block = unbound.thread_block().await;
    assert!(
        unbound_block.starts_with(&format!(
            "thread · {PEER} · {id}\n  you       {id}\n      unbound question",
            id = unbound.item.id
        )),
        "{unbound_block}"
    );
    assert!(
        unbound_block.contains(&sentence),
        "`M17`: an unbound sender's thread block keeps the sentence: {unbound_block}"
    );

    // The BOUND sender's block is the same thread without it (`M44` gate,
    // `M46` parser, `M16` umbrella).
    let bound_block = bound.thread_block().await;
    assert!(
        bound_block.starts_with(&format!(
            "thread · {PEER} · {id}\n  you       {id}\n      bound question",
            id = bound.item.id
        )),
        "{bound_block}"
    );
    assert!(
        !bound_block.contains(&sentence),
        "`M44`/`M46`/`M16`: a bound sender's thread block omits the sentence: {bound_block}"
    );
    assert!(
        !bound_block.contains("cannot tell which operator sent"),
        "{bound_block}"
    );
    assert_eq!(
        bound_block,
        format!(
            "thread · {PEER} · {id}\n  you       {id}\n      bound question",
            id = bound.item.id
        ),
        "a bound block is the bare thread"
    );
}
