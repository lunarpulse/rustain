//! Story 11.4a (AC-R0) — `/memory forget <fuzzy text>` dispatch + card
//! resolution, extracted from `event_loop.rs` (handlers-extraction pattern,
//! Story 8.0a) to respect the AC-4 line budget. Domain-only deps (no `&AppState`):
//! each function returns the `AppEvent`s to emit and the caller pumps them
//! through the event bus and performs the async `MemoryPort` calls.

use crate::adapters::tui::state::{PendingForgetCard, TuiState};
use crate::domain::errors::MemoryError;
use crate::domain::events::AppEvent;
use crate::domain::models::tab::ConversationId;
use crate::domain::models::{MemoryEntry, NoticeLevel};

/// How many fuzzy matches the confirm card lists at most.
pub const FORGET_CANDIDATE_LIMIT: usize = 10;

/// Parse a `/memory forget <text>` command. Returns the (possibly empty) fuzzy
/// query for a forget command, or `None` if `cmd_name`/`cmd_arg` isn't one — so
/// the caller can use it as an `else if let Some(query) = …` dispatch arm,
/// intercepted BEFORE the adapter-override path (like `/memory consolidate`).
pub fn parse_forget_query(cmd_name: &str, cmd_arg: Option<&str>) -> Option<String> {
    if cmd_name != "memory" {
        return None;
    }
    let arg = cmd_arg.map(str::trim)?;
    if arg == "forget" {
        return Some(String::new());
    }
    arg.strip_prefix("forget ")
        .map(|rest| rest.trim().to_string())
}

fn notice(conversation_id: &ConversationId, level: NoticeLevel, message: String) -> AppEvent {
    AppEvent::SystemNotice {
        conversation_id: Some(conversation_id.clone()),
        level,
        message,
    }
}

/// Build the dispatch events from a `forget_candidates` lookup. `result` is
/// `None` when the query was empty (usage notice); otherwise it carries the
/// adapter's fuzzy matches. On matches, sets the confirm card on `state` (nothing
/// is purged until the user confirms — AC-R0) and emits no event.
pub fn handle_forget_command(
    state: &mut TuiState,
    conversation_id: &ConversationId,
    query: &str,
    result: Option<Result<Vec<(u64, MemoryEntry)>, MemoryError>>,
) -> Vec<AppEvent> {
    state.needs_redraw = true;
    match result {
        None => vec![notice(
            conversation_id,
            NoticeLevel::Info,
            "Usage: /memory forget <text> — names the memory to permanently remove from search."
                .to_string(),
        )],
        Some(Err(e)) => vec![notice(
            conversation_id,
            NoticeLevel::Warning,
            format!("Could not search memory to forget: {e}"),
        )],
        Some(Ok(c)) if c.is_empty() => vec![notice(
            conversation_id,
            NoticeLevel::Info,
            format!("No memory matches \"{query}\" — nothing to forget."),
        )],
        Some(Ok(c)) => {
            if state.pending_forget_card.is_some() {
                return vec![notice(
                    conversation_id,
                    NoticeLevel::Info,
                    "A forget card is already open — resolve it first.".to_string(),
                )];
            }
            state.pending_forget_card = Some(PendingForgetCard {
                conversation_id: conversation_id.clone(),
                candidates: c.into_iter().map(|(k, e)| (k, e, true)).collect(),
                focused_index: 0,
            });
            Vec::new()
        }
    }
}

/// Resolve the forget card on a y/n keypress: take the card and emit
/// `MemoryForgetResolved` with the selected keys (`accept`) or empty (decline).
/// Uses `card.conversation_id` (the tab that issued `/memory forget`) so a
/// cross-tab switch does not apply the forget to the wrong conversation.
/// `None` if no card is pending (defensive — the key intercept should gate this).
pub fn resolve_forget_card(
    state: &mut TuiState,
    _conversation_id: &ConversationId,
    accept: bool,
) -> Option<AppEvent> {
    let card = state.pending_forget_card.take()?;
    state.needs_redraw = true;
    let keys: Vec<u64> = if accept {
        card.candidates
            .into_iter()
            .filter(|(_, _, selected)| *selected)
            .map(|(k, _, _)| k)
            .collect()
    } else {
        Vec::new()
    };
    Some(AppEvent::MemoryForgetResolved {
        conversation_id: card.conversation_id,
        keys,
    })
}

/// The `SystemNotice` summarising a resolved forget. `result` is `None` when the
/// user cancelled (empty key set), `Some(Ok)` on a completed purge, `Some(Err)`
/// when the purge failed (the tombstone still landed first — AC-R6 — so the next
/// refresh converges; the copy reflects the visible state honestly).
pub fn forget_result_notice(
    conversation_id: &ConversationId,
    count: usize,
    result: Option<Result<(), MemoryError>>,
) -> AppEvent {
    match result {
        None => notice(
            conversation_id,
            NoticeLevel::Info,
            "Forget cancelled — nothing removed.".to_string(),
        ),
        Some(Ok(())) => notice(
            conversation_id,
            NoticeLevel::Info,
            format!(
                "Permanently removed {count} memor{} from search — it will not resurface, even after a reindex.",
                if count == 1 { "y" } else { "ies" }
            ),
        ),
        Some(Err(e)) => notice(
            conversation_id,
            NoticeLevel::Warning,
            format!("Could not complete removal: {e}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv() -> ConversationId {
        "c1".to_string()
    }

    fn entry(summary: &str) -> MemoryEntry {
        MemoryEntry {
            timestamp: chrono::Local::now(),
            summary: summary.to_string(),
            context: None,
        }
    }

    #[test]
    fn parse_recognises_forget_and_query() {
        assert_eq!(
            parse_forget_query("memory", Some("forget my secret")),
            Some("my secret".into())
        );
        assert_eq!(
            parse_forget_query("memory", Some("forget")),
            Some(String::new())
        );
        assert_eq!(
            parse_forget_query("memory", Some("  forget   x ")),
            Some("x".into())
        );
        // Not a forget command.
        assert_eq!(parse_forget_query("memory", Some("consolidate")), None);
        assert_eq!(parse_forget_query("context", Some("forget x")), None);
        assert_eq!(parse_forget_query("memory", None), None);
    }

    #[test]
    fn handle_sets_card_on_matches() {
        let mut state = TuiState::new(80, 24);
        let cands = vec![(7u64, entry("the secret")), (9u64, entry("secret two"))];
        let evs = handle_forget_command(&mut state, &conv(), "secret", Some(Ok(cands)));
        assert!(evs.is_empty(), "matches set a card, emit no notice");
        let card = state.pending_forget_card.as_ref().unwrap();
        assert_eq!(card.candidates.len(), 2);
        assert!(
            card.candidates.iter().all(|(_, _, sel)| *sel),
            "all selected by default"
        );
        assert_eq!(card.focused_index, 0, "focus starts at first row");
    }

    #[test]
    fn handle_emits_notice_on_empty_query_and_no_matches() {
        let mut state = TuiState::new(80, 24);
        let evs = handle_forget_command(&mut state, &conv(), "", None);
        assert_eq!(evs.len(), 1, "empty query → usage notice");
        assert!(state.pending_forget_card.is_none());

        let evs = handle_forget_command(&mut state, &conv(), "ghost", Some(Ok(Vec::new())));
        assert_eq!(evs.len(), 1, "no matches → 'nothing to forget' notice");
        assert!(state.pending_forget_card.is_none());
    }

    #[test]
    fn resolve_accept_emits_selected_keys_decline_emits_empty() {
        let mut state = TuiState::new(80, 24);
        state.pending_forget_card = Some(PendingForgetCard {
            conversation_id: conv(),
            candidates: vec![(7, entry("a"), true), (9, entry("b"), false)],
            focused_index: 0,
        });
        let ev = resolve_forget_card(&mut state, &conv(), true).unwrap();
        match ev {
            AppEvent::MemoryForgetResolved { keys, .. } => {
                assert_eq!(keys, vec![7], "only selected keys")
            }
            _ => panic!("wrong event"),
        }
        assert!(state.pending_forget_card.is_none(), "card cleared");

        // Decline path.
        state.pending_forget_card = Some(PendingForgetCard {
            conversation_id: conv(),
            candidates: vec![(7, entry("a"), true)],
            focused_index: 0,
        });
        let ev = resolve_forget_card(&mut state, &conv(), false).unwrap();
        match ev {
            AppEvent::MemoryForgetResolved { keys, .. } => {
                assert!(keys.is_empty(), "decline → no keys")
            }
            _ => panic!("wrong event"),
        }
        // No card pending → None.
        assert!(resolve_forget_card(&mut state, &conv(), true).is_none());
    }

    #[test]
    fn result_notice_wording_is_honest() {
        let cancelled = forget_result_notice(&conv(), 0, None);
        let done = forget_result_notice(&conv(), 1, Some(Ok(())));
        let done2 = forget_result_notice(&conv(), 3, Some(Ok(())));
        let failed = forget_result_notice(&conv(), 2, Some(Err(MemoryError::Other("x".into()))));
        for (ev, needle) in [
            (cancelled, "cancelled"),
            (done, "Permanently removed 1 memory"),
            (done2, "Permanently removed 3 memories"),
            (failed, "Could not complete removal"),
        ] {
            match ev {
                AppEvent::SystemNotice { message, .. } => {
                    assert!(message.contains(needle), "got: {message}")
                }
                _ => panic!("expected SystemNotice"),
            }
        }
    }

    // Story 12.0 AC7 (regression guard) — single-pending-forget-card mutual
    // exclusion. Guards the 11-4a fix: a second `/memory forget` while a card is
    // already open MUST be rejected with the "already open" notice and MUST NOT
    // replace the pending card (the multiple-pending state was the 11-4a deadlock
    // trap). `pending_forget_card` is single-threaded `TuiState` (event-loop
    // owned), so the invariant is structural — the `is_some()` gate — not an async
    // race; this test pins that gate so it cannot silently regress under the new
    // daemon concurrency (12.4 routes forget through the same single-card seam).
    #[test]
    fn second_forget_is_rejected_while_card_pending() {
        let mut state = TuiState::new(80, 24);

        // Open the first card.
        let first = vec![(7u64, entry("the secret")), (9u64, entry("secret two"))];
        let evs = handle_forget_command(&mut state, &conv(), "secret", Some(Ok(first)));
        assert!(evs.is_empty(), "first matches open a card silently");
        assert_eq!(
            state.pending_forget_card.as_ref().unwrap().candidates.len(),
            2
        );

        // A second forget with DIFFERENT matches must be rejected, card untouched.
        let second = vec![(11u64, entry("another thing")), (13u64, entry("more"))];
        let evs = handle_forget_command(&mut state, &conv(), "another", Some(Ok(second)));
        assert_eq!(evs.len(), 1, "second forget emits exactly one notice");
        match &evs[0] {
            AppEvent::SystemNotice { message, level, .. } => {
                assert_eq!(*level, NoticeLevel::Info);
                assert!(
                    message.contains("already open"),
                    "rejection notice surfaced: {message}"
                );
            }
            _ => panic!("expected SystemNotice"),
        }
        // The ORIGINAL card is intact (not replaced by the second query's matches).
        let card = state.pending_forget_card.as_ref().unwrap();
        assert_eq!(card.candidates.len(), 2, "original card unchanged");
        assert!(
            card.candidates.iter().any(|(k, _, _)| *k == 7),
            "still the FIRST card's candidates (key 7), not the second's"
        );
        assert!(
            !card.candidates.iter().any(|(k, _, _)| *k == 11),
            "the second forget's candidates did NOT leak in"
        );
    }
}
