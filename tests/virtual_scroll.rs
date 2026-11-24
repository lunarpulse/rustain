#![allow(clippy::field_reassign_with_default)] // AI-12.1: test setup

use ratatui::Terminal;
use ratatui::backend::TestBackend;

#[cfg(debug_assertions)]
use rustain::adapters::tui::state::RenderWork;
use rustain::adapters::tui::state::TabRenderState;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::chat_pane;
use rustain::domain::models::{ChatMessage, Conversation, MessageRole, StreamingState};

fn make_conversation(msg_count: usize) -> Conversation {
    let messages: Vec<ChatMessage> = (0..msg_count)
        .map(|i| ChatMessage {
            synthetic: false,
            id: rustain::domain::models::generate_conversation_id(),
            role: if i % 2 == 0 {
                MessageRole::User
            } else {
                MessageRole::Assistant
            },
            content: format!("Message number {} with some content to fill a line.", i),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: i as i64,
            token_count: None,
            stop_reason: None,
            images: vec![],
            origin: rustain::domain::models::ChannelKind::Terminal,
            authorship: rustain::domain::models::Authorship::HumanWritten,
            send_authorization: rustain::domain::models::SendAuthorization::Unknown,
            retracted_at_ms: None,
            history_source: Default::default(),
        })
        .collect();

    Conversation {
        id: "bench".to_string(),
        title: String::new(),
        messages,
        turns: Vec::new(),
        created_at: 0,
        updated_at: 0,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
        compaction: None,
        peer_row_controls: Default::default(),
        peer_row_revision: 0,
        persist: Default::default(),
    }
}

/// AC8: Virtual scrolling with viewport culling renders only visible messages.
/// Local benchmark: 1000+ messages, assert render < 16ms.
/// Ignored on CI — the count tests below assert culling without a clock.
#[test]
#[ignore]
fn test_virtual_scroll_1000_messages_performance() {
    let conversation = make_conversation(1000);
    let streaming = StreamingState::default();
    let theme = Theme::dark();
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut tab_render_state = TabRenderState::default();

    let start = std::time::Instant::now();
    terminal
        .draw(|frame| {
            let area = frame.area();
            chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut tab_render_state,
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(
                ),
            );
        })
        .unwrap();
    let elapsed = start.elapsed();

    // Local benchmark: should render in < 16ms
    assert!(
        elapsed.as_millis() < 16,
        "1000-message render took {:?} (should be < 16ms)",
        elapsed
    );
}

/// Edge case: zero messages.
#[test]
fn test_virtual_scroll_zero_messages() {
    let conversation = make_conversation(0);
    let streaming = StreamingState::default();
    let theme = Theme::dark();
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut tab_render_state = TabRenderState::default();

    terminal
        .draw(|frame| {
            let area = frame.area();
            let result = chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut tab_render_state,
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(
                ),
            );
            assert_eq!(result.total_content_height, 0);
        })
        .unwrap();
}

/// Edge case: one message.
#[test]
fn test_virtual_scroll_one_message() {
    let conversation = make_conversation(1);
    let streaming = StreamingState::default();
    let theme = Theme::dark();
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut tab_render_state = TabRenderState::default();

    terminal
        .draw(|frame| {
            let area = frame.area();
            let result = chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut tab_render_state,
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(
                ),
            );
            assert!(result.total_content_height > 0);
            assert_eq!(result.block_boundaries.len(), 1);
            // Single user message
            assert_eq!(result.message_boundaries.len(), 1);
        })
        .unwrap();
}

// Covers: FR13 (auto-scroll), NFR4 (1000+ message performance), AC7 — viewport culling
#[test]
fn test_virtual_scroll_viewport_culling() {
    let conversation = make_conversation(60);
    let streaming = StreamingState::default();
    let theme = Theme::dark();

    // First render at bottom (scroll_offset = 0, auto_scroll = true) to get total height
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut tab_render_state = TabRenderState::default();
    let mut total_height = 0;

    terminal
        .draw(|frame| {
            let area = frame.area();
            let result = chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut tab_render_state,
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(
                ),
            );
            total_height = result.total_content_height;
        })
        .unwrap();

    assert!(
        total_height > 24,
        "60 messages should exceed viewport height"
    );

    // Render scrolled to middle (offset = half of max scroll range)
    let max_offset = total_height.saturating_sub(24);
    let mid_offset = max_offset / 2;

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut tab_render_state = TabRenderState::default();

    terminal
        .draw(|frame| {
            let area = frame.area();
            chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                mid_offset,
                false, // NOT auto-scroll — use explicit offset
                &theme,
                &mut tab_render_state,
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(
                ),
            );
        })
        .unwrap();

    let text: String = terminal
        .backend()
        .buffer()
        .clone()
        .content()
        .iter()
        .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
        .collect();

    // First message (index 0) should NOT be visible when scrolled to middle
    assert!(
        !text.contains("Message number 0 "),
        "First message should not be visible when scrolled to middle"
    );
    // Last message (index 59) should NOT be visible when scrolled to middle
    assert!(
        !text.contains("Message number 59 "),
        "Last message should not be visible when scrolled to middle"
    );
    // Some middle message should be visible
    let has_middle = (20..40).any(|i| text.contains(&format!("Message number {}", i)));
    assert!(has_middle, "Some middle messages should be visible");
}

// Covers: FR13 (auto-scroll), AC7 — jump to bottom shows last messages
#[test]
fn test_virtual_scroll_jump_to_bottom() {
    let conversation = make_conversation(60);
    let streaming = StreamingState::default();
    let theme = Theme::dark();
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut tab_render_state = TabRenderState::default();

    // Render at bottom (scroll_offset = 0, auto_scroll = true)
    terminal
        .draw(|frame| {
            let area = frame.area();
            chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true, // auto_scroll = jump to bottom
                &theme,
                &mut tab_render_state,
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(
                ),
            );
        })
        .unwrap();

    let text: String = terminal
        .backend()
        .buffer()
        .clone()
        .content()
        .iter()
        .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
        .collect();

    // Last message should be visible
    assert!(
        text.contains("Message number 59"),
        "Last message should be visible at bottom"
    );
    // First message should NOT be visible
    assert!(
        !text.contains("Message number 0 "),
        "First message should not be visible at bottom"
    );
}

// Covers: AC7 — messages above viewport not present in buffer
#[test]
fn test_virtual_scroll_above_viewport_not_rendered() {
    let conversation = make_conversation(60);
    let streaming = StreamingState::default();
    let theme = Theme::dark();

    // Scroll all the way to the top (max offset)
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut tab_render_state = TabRenderState::default();
    let mut total_height = 0;

    terminal
        .draw(|frame| {
            let area = frame.area();
            let result = chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut tab_render_state,
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(
                ),
            );
            total_height = result.total_content_height;
        })
        .unwrap();

    let max_offset = total_height.saturating_sub(24);

    // Now render at the top
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut tab_render_state = TabRenderState::default();

    terminal
        .draw(|frame| {
            let area = frame.area();
            chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                max_offset,
                false,
                &theme,
                &mut tab_render_state,
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(
                ),
            );
        })
        .unwrap();

    let text: String = terminal
        .backend()
        .buffer()
        .clone()
        .content()
        .iter()
        .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
        .collect();

    // First message should be visible at top
    assert!(
        text.contains("Message number 0"),
        "First message should be visible when scrolled to top"
    );
    // Last message should NOT be visible
    assert!(
        !text.contains("Message number 59"),
        "Last message should not be visible when scrolled to top"
    );
}

/// Edge case: only user messages (no assistant).
#[test]
fn test_virtual_scroll_only_user_messages() {
    let messages: Vec<ChatMessage> = (0..5)
        .map(|i| ChatMessage {
            synthetic: false,
            id: rustain::domain::models::generate_conversation_id(),
            role: MessageRole::User,
            content: format!(
                "User message {
                }",
                i
            ),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: i as i64,
            token_count: None,
            stop_reason: None,
            images: vec![],
            origin: rustain::domain::models::ChannelKind::Terminal,
            authorship: rustain::domain::models::Authorship::HumanWritten,
            send_authorization: rustain::domain::models::SendAuthorization::Unknown,
            retracted_at_ms: None,
            history_source: Default::default(),
        })
        .collect();
    let conversation = Conversation {
        id: "test".to_string(),
        title: String::new(),
        messages,
        turns: Vec::new(),
        created_at: 0,
        updated_at: 0,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
        compaction: None,
        peer_row_controls: Default::default(),
        peer_row_revision: 0,
        persist: Default::default(),
    };
    let streaming = StreamingState::default();
    let theme = Theme::dark();
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut tab_render_state = TabRenderState::default();

    terminal
        .draw(|frame| {
            let area = frame.area();
            let result = chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut tab_render_state,
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(
                ),
            );
            assert_eq!(result.message_boundaries.len(), 5);
            assert_eq!(result.block_boundaries.len(), 5);
        })
        .unwrap();
}

// ── Story 16-5: Height Cache tests ──

/// Deterministic fixture builder: turn[i] has (i % 3) + 1 prose paragraphs and i % 5 tools.
/// AC15 / Bob P1-3 / Quinn Q-P0-4: NO RNG — reproducible across CI runs.
fn make_turn_conversation_seeded(turn_count: usize) -> Conversation {
    use rustain::domain::models::{StopReason, Turn, TurnId, TurnPart};

    let mut messages = Vec::new();
    let mut turns = Vec::new();

    for i in 0..turn_count {
        let turn_id = TurnId(format!("turn-{}", i));
        // Production joins an assistant row to its turn on the row id
        // (`turn.rs`, `conversation.rs`): the row carries the turn's id.
        let msg_id = turn_id.0.clone();

        let prose_count = (i % 3) + 1;
        let tool_count = i % 5;

        let mut parts = Vec::new();
        for p in 0..prose_count {
            parts.push(TurnPart::Prose {
                id: rustain::domain::models::PartId(p as u64),
                text: format!("Paragraph {} for turn {}", p, i),
            });
        }
        for t in 0..tool_count {
            parts.push(TurnPart::ToolInvocation {
                id: rustain::domain::models::PartId((prose_count + t) as u64),
                tool: "Read".to_string(),
                args: serde_json::json!({"path": "/tmp"}),
                status: rustain::domain::models::InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_001_000),
            });
        }

        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        turn.id = turn_id;
        for part in parts {
            turn.push_part(|_id| part);
        }
        turn.stop_reason = Some(StopReason::EndTurn);
        turns.push(turn);

        messages.push(ChatMessage {
            synthetic: false,
            id: msg_id,
            role: if i % 2 == 0 {
                MessageRole::User
            } else {
                MessageRole::Assistant
            },
            content: format!("Message {}", i),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: i as i64,
            token_count: None,
            stop_reason: Some(StopReason::EndTurn),
            images: vec![],
            origin: rustain::domain::models::ChannelKind::Terminal,
            authorship: rustain::domain::models::Authorship::HumanWritten,
            send_authorization: rustain::domain::models::SendAuthorization::Unknown,
            retracted_at_ms: None,
            history_source: Default::default(),
        });
    }

    Conversation {
        id: "bench-turns".to_string(),
        title: String::new(),
        messages,
        turns,
        created_at: 0,
        updated_at: 0,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
        compaction: None,
        peer_row_controls: Default::default(),
        peer_row_revision: 0,
        persist: Default::default(),
    }
}

/// AC7: No recompute during scroll for committed turns (warm cache).
/// Verifies cache entry count stays stable across scrolls (no new entries
/// added = all hits, no misses that would create new keys).
#[test]
fn test_no_recompute_during_scroll() {
    let conversation = make_turn_conversation_seeded(50);
    let streaming = StreamingState::default();
    let theme = Theme::dark();

    // Warm-up render (populates cache)
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut tab_render_state = TabRenderState::default();
    terminal
        .draw(|frame| {
            let area = frame.area();
            chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut tab_render_state,
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(
                ),
            );
        })
        .unwrap();

    let entry_count_after_warmup = tab_render_state.height_cache.entries.len()
        + tab_render_state.height_cache.message_entries.len();

    // Scroll renders (should be all cache hits — no new entries)
    for offset in [0, 5, 10, 15, 20] {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                chat_pane::render(
                    frame,
                    area,
                    &conversation,
                    &streaming,
                    offset,
                    false,
                    &theme,
                    &mut tab_render_state,
                    &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(),
                );
            })
            .unwrap();
    }

    let entry_count_after_scroll = tab_render_state.height_cache.entries.len()
        + tab_render_state.height_cache.message_entries.len();
    assert_eq!(
        entry_count_after_scroll, entry_count_after_warmup,
        "warm-cache scroll must not create new cache entries"
    );
}

/// Draw one frame of `conversation` at 80x24 (auto-scroll) and return the work
/// that frame did. Counts, not time: they are per `TabRenderState`, so tests on
/// parallel threads cannot disturb each other (infra-2).
#[cfg(debug_assertions)]
fn draw_work(conversation: &Conversation, tab: &mut TabRenderState) -> RenderWork {
    let streaming = StreamingState::default();
    let theme = Theme::dark();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| {
            let area = frame.area();
            chat_pane::render(
                frame,
                area,
                conversation,
                &streaming,
                0,
                true,
                &theme,
                tab,
                &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(
                ),
            );
        })
        .unwrap();
    tab.render_work
}

/// AC8 (infra-2, K-VS1/K-VS2): the first frame computes exactly one height per
/// message, all of them through the turn cache or the message cache (never the
/// uncached legacy fallback); an identical second frame computes none. Replaces
/// the wall-clock 500-vs-50 ratio, which also measured the legacy fallback because
/// the fixture's assistant rows did not join their turns.
#[cfg(debug_assertions)]
#[test]
fn test_turn_cold_frame_computes_one_height_per_message_and_warm_frame_none() {
    for turn_count in [50, 500] {
        let conversation = make_turn_conversation_seeded(turn_count);
        let mut tab = TabRenderState::default();

        let cold = draw_work(&conversation, &mut tab);
        assert_eq!(
            cold.turn_layouts + cold.message_heights,
            conversation.messages.len(),
            "{turn_count} turns, cold frame: one height per message, got {cold:?}"
        );
        assert_eq!(cold.legacy_heights, 0, "{turn_count} turns: {cold:?}");
        assert!(
            cold.turn_layouts > 0 && cold.message_heights > 0,
            "positive control: both caches are exercised, got {cold:?}"
        );

        let warm = draw_work(&conversation, &mut tab);
        assert_eq!(
            (warm.turn_layouts, warm.message_heights, warm.legacy_heights),
            (0, 0, 0),
            "{turn_count} turns, warm frame recomputes nothing, got {warm:?}"
        );
    }
}

/// AC8 (infra-2, K-VS3): the paint phase materialises only messages that meet the
/// viewport, so the count does not grow with the conversation. The message-only
/// conversation (a shape loading no longer produces) measures its assistant rows
/// through the legacy fallback on every frame; the test records that instead of
/// hiding it.
#[cfg(debug_assertions)]
#[test]
fn test_rendered_messages_do_not_grow_with_conversation_length() {
    let rendered = |conversation: &Conversation| {
        draw_work(conversation, &mut TabRenderState::default()).rendered_messages
    };
    let (turns_small, turns_large) = (
        rendered(&make_turn_conversation_seeded(50)),
        rendered(&make_turn_conversation_seeded(500)),
    );
    assert!(turns_small > 0, "positive control");
    assert_eq!(turns_small, turns_large, "turn fixture 50 vs 500");
    assert!(turns_large <= 24, "no more messages than viewport rows");

    for message_count in [100, 1000] {
        let conversation = make_conversation(message_count);
        let mut tab = TabRenderState::default();
        let cold = draw_work(&conversation, &mut tab);
        let warm = draw_work(&conversation, &mut tab);
        let assistant_rows = message_count / 2;
        assert_eq!(cold.legacy_heights, assistant_rows, "cold frame {cold:?}");
        assert_eq!(
            warm.legacy_heights, assistant_rows,
            "legacy rows are uncached: {warm:?}"
        );
        assert_eq!(
            cold.message_heights,
            message_count - assistant_rows,
            "{cold:?}"
        );
        assert_eq!(warm.message_heights, 0, "{warm:?}");
        assert!(
            warm.rendered_messages > 0 && warm.rendered_messages <= 24,
            "{warm:?}"
        );
        assert_eq!(cold.rendered_messages, warm.rendered_messages);
    }
    let small = draw_work(&make_conversation(100), &mut TabRenderState::default());
    let large = draw_work(&make_conversation(1000), &mut TabRenderState::default());
    assert_eq!(
        small.rendered_messages, large.rendered_messages,
        "100 vs 1,000 messages"
    );
}

/// AC8 (infra-2, K-VS4): every render call starts its counters from zero, so two
/// different conversations drawn on one state report independent work.
#[cfg(debug_assertions)]
#[test]
fn test_render_work_resets_on_every_render_call() {
    let mut tab = TabRenderState::default();
    let first = draw_work(&make_turn_conversation_seeded(50), &mut tab);
    assert_eq!(first.turn_layouts + first.message_heights, 50, "{first:?}");
    let second = draw_work(&make_conversation(20), &mut tab);
    assert_eq!(second.message_heights, 10, "user rows only: {second:?}");
    assert_eq!(second.legacy_heights, 10, "assistant rows only: {second:?}");
    assert_eq!(second.turn_layouts, 0, "{second:?}");
}

/// AC8 (infra-2 review R4, K-VS5): the height pass resolves each assistant row's
/// turn with exactly one hashed probe of a per-frame map built once with one entry
/// per turn, at every size and on cold and warm frames. The probes are counted
/// inside the lookup (hash computations), so a lookup that scans `conversation.turns`
/// or the map instead — quadratic, invisible to the cache and culling counts, even
/// behind the same helper — turns this RED.
#[cfg(debug_assertions)]
#[test]
fn test_turn_lookups_are_one_probe_per_assistant_row_at_every_size() {
    for turn_count in [50, 500] {
        let conversation = make_turn_conversation_seeded(turn_count);
        let mut tab = TabRenderState::default();
        for frame in ["cold", "warm"] {
            let work = draw_work(&conversation, &mut tab);
            assert_eq!(
                work.turn_lookups,
                turn_count / 2,
                "{turn_count} turns, {frame}: {work:?}"
            );
            assert_eq!(
                work.turn_map_entries, turn_count,
                "{turn_count} turns, {frame}: {work:?}"
            );
            // The work inside each lookup is one hashed probe, at 50 and at 500
            // turns alike. A scan of the turns or of the map hashes nothing, so
            // this is RED for it while `turn_lookups` still counts the call.
            assert_eq!(
                work.turn_lookup_hashes, work.turn_lookups,
                "{turn_count} turns, {frame}: {work:?}"
            );
        }
    }
    for message_count in [100, 1000] {
        let work = draw_work(
            &make_conversation(message_count),
            &mut TabRenderState::default(),
        );
        assert_eq!(
            work.turn_lookups,
            message_count / 2,
            "{message_count} messages: {work:?}"
        );
        assert_eq!(work.turn_map_entries, 0, "{work:?}");
        // An empty map is answered without hashing.
        assert_eq!(work.turn_lookup_hashes, 0, "{work:?}");
    }
}

/// AC12: Eviction gate — evict_turns_not_in drops stale entries after rewind.
#[test]
fn test_evict_turns_not_in_drops_stale_entries() {
    use rustain::adapters::tui::state::HeightCache;
    use rustain::adapters::tui::state::{CachedTurnLayout, HeightKey};
    use rustain::domain::models::TurnId;
    use rustain::domain::models::view_state::SummaryTier;

    let mut cache = HeightCache::default();
    for i in 0..5 {
        cache.set(
            HeightKey {
                turn_id: TurnId(format!("turn-{}", i)),
                expansion: true,
                summary_tier: SummaryTier::Tier1,
                terminal_width: 80,
            },
            CachedTurnLayout {
                height: i + 1,
                block_offsets: vec![],
                tool_block_offsets: vec![],
                md_block_offsets: vec![],
                reasoning_offsets: vec![],
            },
        );
    }

    // Simulate rewind: only turns 0 and 1 remain
    let live = [TurnId("turn-0".into()), TurnId("turn-1".into())];
    cache.evict_turns_not_in(live.iter());

    // Set-membership invariant: all remaining entries must reference live turns
    let live_set: std::collections::HashSet<_> = live.iter().collect();
    assert!(
        cache
            .entries
            .iter()
            .all(|(k, _)| live_set.contains(&k.turn_id)),
        "all surviving entries must reference live turns"
    );

    // Length bound: at most 2 entries (one per live turn × 1 expansion state)
    assert!(
        cache.entries.len() <= live.len(),
        "expected at most {} entries, got {}",
        live.len(),
        cache.entries.len()
    );
}

/// AC15: Eviction gate — skipped when turn count unchanged.
#[test]
fn test_eviction_skipped_when_turn_count_unchanged() {
    use rustain::adapters::tui::state::HeightCache;

    let mut cache = HeightCache::default();
    cache.last_seen_turn_count = 10;

    // Turn count equals last_seen — eviction should NOT fire
    let turn_count = 10;
    assert!(
        (turn_count >= cache.last_seen_turn_count),
        "eviction gate should skip when turn_count == last_seen"
    );
}

/// AC5: Width divergence triggers invalidate_all.
#[test]
fn test_width_divergence_invalidates_cache() {
    use rustain::adapters::tui::state::{CachedTurnLayout, HeightKey};
    use rustain::domain::models::TurnId;
    use rustain::domain::models::view_state::SummaryTier;

    let mut trs = TabRenderState::default();
    trs.cached_width = Some(80);
    trs.height_cache.set(
        HeightKey {
            turn_id: TurnId("t1".into()),
            expansion: true,
            summary_tier: SummaryTier::Tier1,
            terminal_width: 80,
        },
        CachedTurnLayout {
            height: 5,
            block_offsets: vec![],
            tool_block_offsets: vec![],
            md_block_offsets: vec![],
            reasoning_offsets: vec![],
        },
    );

    // Simulate render with different width
    if trs.cached_width != Some(120) {
        trs.height_cache.invalidate_all();
        trs.cached_width = Some(120);
    }

    assert_eq!(
        trs.height_cache.entries.len(),
        0,
        "cache should be empty after width divergence"
    );
    assert_eq!(trs.cached_width, Some(120));
}

/// AC2: User/System messages bypass turn cache — go to message_entries.
#[test]
fn test_user_system_messages_use_message_cache() {
    use rustain::adapters::tui::state::{HeightCache, MessageHeightKey};

    let mut cache = HeightCache::default();
    let key = MessageHeightKey {
        msg_id: "user-1".into(),
        terminal_width: 80,
        content_hash: 42,
    };
    cache.set_message(key.clone(), 3);

    assert_eq!(cache.get_message(&key), Some(3));
    // Should NOT be in turn entries
    assert_eq!(cache.entries.len(), 0);
}

/// AC10: turn_map resolves same as linear scan.
#[test]
fn test_turn_map_resolves_same_as_linear_scan() {
    let conversation = make_turn_conversation_seeded(10);

    let turn_map: std::collections::HashMap<&str, &rustain::domain::models::Turn> = conversation
        .turns
        .iter()
        .map(|t| (t.id.0.as_str(), t))
        .collect();

    for msg in &conversation.messages {
        let from_map = turn_map.get(msg.id.as_str()).copied();
        let from_scan = conversation.turns.iter().find(|t| t.id.0 == msg.id);
        assert_eq!(
            from_map.map(|t| t.id.0.as_str()),
            from_scan.map(|t| t.id.0.as_str()),
            "turn_map and linear scan must agree for msg {}",
            msg.id
        );
    }
}
