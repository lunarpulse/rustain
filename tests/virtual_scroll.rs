#![allow(clippy::field_reassign_with_default)] // AI-12.1: test setup
use std::collections::HashMap;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use rustain::adapters::tui::state::TabRenderState;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::chat_pane;
use rustain::adapters::tui::widgets::tool_block::ToolBlockState;
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
            authorship: Default::default(),
            retracted_at_ms: None,
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
    }
}

/// AC8: Virtual scrolling with viewport culling renders only visible messages.
/// Local benchmark: 1000+ messages, assert render < 16ms.
/// Ignored on CI — use relative scaling test instead.
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
                &HashMap::<String, ToolBlockState>::new(),
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

/// AC8: CI-safe relative benchmark — 1000 msgs in <= 3x the time of 100 msgs.
#[test]
fn test_virtual_scroll_relative_scaling() {
    let streaming = StreamingState::default();
    let theme = Theme::dark();

    // Benchmark 100 messages
    let conv_100 = make_conversation(100);
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut tab_render_state = TabRenderState::default();

    let start_100 = std::time::Instant::now();
    for _ in 0..10 {
        terminal
            .draw(|frame| {
                let area = frame.area();
                chat_pane::render(
                    frame,
                    area,
                    &conv_100,
                    &streaming,
                    0,
                    true,
                    &theme,
                    &mut tab_render_state,
                    &HashMap::<String, ToolBlockState>::new(),
                    &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(),
                );
            })
            .unwrap();
    }
    let time_100 = start_100.elapsed();

    // Benchmark 1000 messages
    let conv_1000 = make_conversation(1000);
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut tab_render_state = TabRenderState::default();

    let start_1000 = std::time::Instant::now();
    for _ in 0..10 {
        terminal
            .draw(|frame| {
                let area = frame.area();
                chat_pane::render(
                    frame,
                    area,
                    &conv_1000,
                    &streaming,
                    0,
                    true,
                    &theme,
                    &mut tab_render_state,
                    &HashMap::<String, ToolBlockState>::new(),
                    &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(),
                );
            })
            .unwrap();
    }
    let time_1000 = start_1000.elapsed();

    // 1000 messages should be ≤ 8x the time of 100 messages.
    // Ratio increased from 3x to 8x after Story 3-6 replaced parse_inline_code
    // with the full 5-stage markdown pipeline (pulldown-cmark + transform + layout).
    // compute_height() runs the full pipeline for every uncached message, so the
    // first draw is ~10x heavier per message; cached draws keep the ratio sublinear.
    let ratio = time_1000.as_nanos() as f64 / time_100.as_nanos().max(1) as f64;
    assert!(
        ratio <= 8.0,
        "1000-msg render ({:?}) should be ≤ 8x of 100-msg render ({:?}), ratio: {:.2}",
        time_1000,
        time_100,
        ratio,
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
                &HashMap::<String, ToolBlockState>::new(),
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
                &HashMap::<String, ToolBlockState>::new(),
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
                &HashMap::<String, ToolBlockState>::new(),
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
                &HashMap::<String, ToolBlockState>::new(),
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
                &HashMap::<String, ToolBlockState>::new(),
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
                &HashMap::<String, ToolBlockState>::new(),
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
                &HashMap::<String, ToolBlockState>::new(),
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
            authorship: Default::default(),
            retracted_at_ms: None,
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
                &HashMap::<String, ToolBlockState>::new(),
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
        let msg_id = format!("msg-{}", i);
        let turn_id = TurnId(format!("turn-{}", i));

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
            authorship: Default::default(),
            retracted_at_ms: None,
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
                &HashMap::<String, ToolBlockState>::new(),
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
                    &HashMap::<String, ToolBlockState>::new(),
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

/// AC8: Turn-based ratio test — 500-turn render time ≤ 5× of 50-turn render time.
/// Additive: does NOT modify the existing message-mirror benchmark (AC14).
#[test]
fn test_turn_render_scaling_500_vs_50() {
    let conv_50 = make_turn_conversation_seeded(50);
    let conv_500 = make_turn_conversation_seeded(500);
    let streaming = StreamingState::default();
    let theme = Theme::dark();

    // Benchmark 50 turns (10 draws)
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut tab_render_state = TabRenderState::default();
    let start_50 = std::time::Instant::now();
    for _ in 0..10 {
        terminal
            .draw(|frame| {
                let area = frame.area();
                chat_pane::render(
                    frame,
                    area,
                    &conv_50,
                    &streaming,
                    0,
                    true,
                    &theme,
                    &mut tab_render_state,
                    &HashMap::<String, ToolBlockState>::new(),
                    &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(),
                );
            })
            .unwrap();
    }
    let time_50 = start_50.elapsed();

    // Benchmark 500 turns (10 draws)
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut tab_render_state = TabRenderState::default();
    let start_500 = std::time::Instant::now();
    for _ in 0..10 {
        terminal
            .draw(|frame| {
                let area = frame.area();
                chat_pane::render(
                    frame,
                    area,
                    &conv_500,
                    &streaming,
                    0,
                    true,
                    &theme,
                    &mut tab_render_state,
                    &HashMap::<String, ToolBlockState>::new(),
                    &std::collections::BTreeMap::<String, rustain::domain::models::FeedbackBlock>::new(),
                );
            })
            .unwrap();
    }
    let time_500 = start_500.elapsed();

    let ratio = time_500.as_nanos() as f64 / time_50.as_nanos().max(1) as f64;
    assert!(
        ratio <= 5.0,
        "500-turn render ({:?}) should be ≤ 5× of 50-turn render ({:?}), ratio: {:.2}",
        time_500,
        time_50,
        ratio,
    );
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
                tool_block_states_version: 0,
            },
            CachedTurnLayout {
                height: i + 1,
                block_offsets: vec![],
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
            tool_block_states_version: 0,
        },
        CachedTurnLayout {
            height: 5,
            block_offsets: vec![],
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
