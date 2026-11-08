//! K5 (second half) — dynamic cards, panels and producers (Story 21.11, AC5).
//!
//! Covers the consolidation/forget/peer-add/retract/apply/skill-trust cards,
//! the tool block, usage panel, result row and the team send table/thread
//! label columns. Decision cases raise queue items and paint the production
//! frame into a real `Buffer`; the other cases use their production renderers.
//! C/G/A fixtures exercise exact and over-boundary inputs, whole clusters,
//! wrapped facts and reachable action rails at 60, 80 and 130 cells.
//!
//! `config::short_reason` is private and the reload handler's context
//! (`ReloadContext`, `ComposeContext`) is assembled from crate internals, so
//! the reload-failure producer (`handle_config_reload_with_two_pass` →
//! `AppEvent::ConfigReloaded.error`) is covered by the `display_width_*` unit
//! tests in `src/adapters/tui/handlers/config.rs`, not here.

use super::support::*;

use ed25519_dalek::SigningKey;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::{Paragraph, Widget};
use rustain::adapters::cli::peer::rows::confirm_card_text;
use rustain::adapters::tui::decision_queue::{
    self, ArtifactCardMode, PendingArtifactCard, PendingConsolidationCard, PendingForgetCard,
    PendingPeerAdd, Responder, RetractTarget, TeamRetractPayload,
};
use rustain::adapters::tui::handlers::team_command::{TeamSendRow, render_team_send_rows};
use rustain::adapters::tui::state::{TuiState, TurnUsageRow, UsagePanelState};
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::result_row::{ResultRowSnapshot, render_result_row};
use rustain::adapters::tui::widgets::tool_block::render_tool_block_lines;
use rustain::adapters::tui::widgets::{decision_frame, skill_trust_prompt, usage_panel};
use rustain::domain::clock::MockClock;
use rustain::domain::models::orchestration::SpokeResult;
use rustain::domain::models::{
    AgentId, ArtifactId, ArtifactKind, CapabilityTokenId, ContentHash, EvidenceArtifact,
    HostBinding, MemoryEntry, MemoryFact, PeerTicket, ProvenanceTag, RecipientOutcome,
    ReviewStatus, SemanticMessageType, ToolCallInfo, ToolResultInfo, WriteDiffState,
};
use unicode_width::UnicodeWidthStr;

const WIDTHS: [u16; 3] = [60, 80, 130];

/// Paint a raised item through the production decision frame, with enough height to
/// exercise wrapped bodies without hiding the rail behind the scroll viewport.
pub(crate) fn paint_decision(
    width: u16,
    conversation: &str,
    raise: impl FnOnce(&mut TuiState),
) -> Buffer {
    paint_decision_in_area(width, conversation, Rect::new(0, 0, width, 100), raise)
}

fn paint_decision_in_area(
    width: u16,
    conversation: &str,
    area: Rect,
    raise: impl FnOnce(&mut TuiState),
) -> Buffer {
    let mut state = TuiState::new(width, 100);
    let facts = crate::conformance_21_6_decision_queue::support::facts(conversation);
    decision_queue::prune_closed(&mut state, &facts);
    raise(&mut state);
    let item = state.decisions.items().next().expect("raised item");
    let key = decision_queue::DecisionKey {
        kind: item.kind,
        source: item.source.clone(),
        conversation_id: item.conversation_id.clone(),
    };
    if rustain::adapters::tui::decision_queue::current(
        &state,
        &crate::conformance_21_6_decision_queue::support::facts(conversation),
    )
    .is_none()
    {
        assert!(decision_queue::open(&mut state, &key));
    }
    let facts = crate::conformance_21_6_decision_queue::support::facts(conversation);
    let p = paint(width, 100, |frame| {
        assert!(decision_frame::render(
            frame,
            area,
            &state,
            &facts,
            &Theme::dark(),
            "Decision",
            None,
        ));
    });
    for y in 0..100 {
        assert!(UnicodeWidthStr::width(row_text(&p.buffer, y).as_str()) <= usize::from(width));
        assert_eq!(
            row_cells(&p.buffer, y),
            usize::from(width),
            "row {y} at {width}"
        );
    }
    assert_whole_clusters(&p.buffer);
    assert_no_partial_family(&screen(&p.buffer));
    p.buffer
}

/// Frame text without wrapping whitespace, for checking that long words survive.
pub(crate) fn compact(buffer: &Buffer) -> String {
    (1..buffer.area.height - 1)
        .map(|y| {
            let row = glyphs(buffer, y);
            row[1..row.len() - 1].concat()
        })
        .collect::<String>()
        .split_whitespace()
        .collect()
}

/// Display column (cell x) where `needle` starts on row `y`, read from the
/// painted cells.
fn col_of(buffer: &Buffer, y: u16, needle: &str) -> Option<u16> {
    let mut cells: Vec<(u16, String)> = Vec::new();
    let mut x = buffer.area.left();
    while x < buffer.area.right() {
        let symbol = buffer.cell((x, y)).map_or(" ", |c| c.symbol()).to_string();
        let step = symbol.width().max(1) as u16;
        cells.push((x, symbol));
        x += step;
    }
    (0..cells.len()).find_map(|i| {
        let rest: String = cells[i..].iter().map(|(_, s)| s.as_str()).collect();
        rest.starts_with(needle).then_some(cells[i].0)
    })
}

// ── consolidation / forget ──────────────────────────────────────────────────

fn consolidation_card_with(
    category: &str,
    fact: &str,
    detail: Option<&str>,
) -> PendingConsolidationCard {
    PendingConsolidationCard {
        conversation_id: "c1".to_string(),
        proposals: vec![(
            MemoryFact {
                category: category.into(),
                fact: fact.into(),
                detail: detail.map(str::to_string),
            },
            true,
        )],
    }
}

#[test]
fn k5b_consolidation_fact_and_detail_fit_their_cells_at_60_80_130() {
    for width in WIDTHS {
        for text in [
            "a".repeat(usize::from(width) - 2),
            "a".repeat(usize::from(width) - 1),
            C.repeat(usize::from(width)),
            format!("{}{G}{A}tail", "a".repeat(usize::from(width) - 4)),
        ] {
            let card = consolidation_card_with("Build", &text, Some(&text));
            let buf = paint_decision(width, "c1", |state| {
                decision_queue::raise_consolidation(state, card);
            });
            let flat = compact(&buf);
            assert!(
                flat.contains(&format!("[x]##Build—{text}")),
                "{width}: {flat}"
            );
            assert!(flat.matches(&text).count() >= 2, "{width}: {flat}");
            for label in ["promote", "decline"] {
                assert!(flat.to_lowercase().contains(label), "{width}: {flat}");
            }
        }
    }
}

#[test]
fn k5b_consolidation_long_category_and_fact_wrap_without_loss() {
    let category = format!("{}{G}{A}", C.repeat(20));
    for width in WIDTHS {
        let card = consolidation_card_with(&category, "abcdefghij", None);
        let buf = paint_decision(width, "c1", |state| {
            decision_queue::raise_consolidation(state, card);
        });
        assert!(compact(&buf).contains(&format!("##{category}—abcdefghij")));
    }
}

#[test]
fn k5b_forget_summary_and_key_remain_reachable_at_60_80_130() {
    for width in WIDTHS {
        for summary in [
            "m".repeat(usize::from(width) - 2),
            "m".repeat(usize::from(width) - 1),
            C.repeat(usize::from(width)),
            format!("{}{G}{A}tail", "m".repeat(usize::from(width) - 4)),
        ] {
            let card = PendingForgetCard {
                conversation_id: "c1".into(),
                candidates: vec![(
                    0xdead_beef,
                    MemoryEntry {
                        timestamp: chrono::Local::now(),
                        summary: summary.clone(),
                        context: None,
                    },
                    true,
                )],
                focused_index: 0,
            };
            let buf = paint_decision(width, "c1", |state| {
                decision_queue::raise_forget(state, card);
            });
            let flat = compact(&buf);
            assert!(flat.contains(&summary), "{width}: {flat}");
            assert!(flat.contains("[00000000deadbeef]"), "{width}: {flat}");
            assert!(flat.contains("toggle"), "{width}: {flat}");
            assert!(
                flat.contains("forgetselected") && flat.contains("cancel"),
                "{width}: {flat}"
            );
        }
    }
}

// ── peer add / retract ──────────────────────────────────────────────────────

pub(crate) fn peer_pending(alias: &str, suggested: Option<&str>) -> PendingPeerAdd {
    let ticket = PeerTicket::mint(
        &SigningKey::from_bytes(&[7; 32]),
        vec![b"opaque-address-one".to_vec()],
        4_102_444_800,
        suggested.map(str::to_owned),
    )
    .expect("minting a ticket for a well-formed key must succeed");
    let peer_id = ticket.peer_id().expect("peer id");
    let card = confirm_card_text(alias, &ticket, &peer_id);
    PendingPeerAdd {
        conversation_id: "c1".to_owned(),
        alias: alias.to_owned(),
        ticket,
        peer_id,
        card,
        reach: None,
        reach_refresh: false,
    }
}

#[test]
fn k5b_peer_add_card_rows_wrap_at_whole_graphemes_at_60_80_130() {
    let alias = format!("{}{G}{A}", C.repeat(20));
    let opaque = "a1B2c3D4".repeat(20);
    for width in WIDTHS {
        let pending = peer_pending(&alias, Some(&opaque));
        let body: String = pending
            .card
            .split("  [y]")
            .next()
            .unwrap()
            .split_whitespace()
            .collect();
        let buf = paint_decision(width, "c1", |state| {
            decision_queue::raise_peer_add(state, pending);
        });
        let flat = compact(&buf);
        assert!(flat.contains(&body), "{width}: {flat}");
        assert!(
            flat.contains(&format!("Addpeer—{alias}")),
            "{width}: {flat}"
        );
        assert!(
            flat.contains("Confirmandpin") && flat.contains("Cancel"),
            "{flat}"
        );
    }
}

#[test]
fn k5b_peer_add_decision_line_and_frame_rail_are_visible() {
    for width in WIDTHS {
        let buf = paint_decision(width, "c1", |state| {
            decision_queue::raise_peer_add(state, peer_pending("ab", None));
        });
        let flat = compact(&buf);
        assert!(flat.contains("Awaitingyourdecision."), "{flat}");
        assert!(
            flat.contains("Confirmandpin") && flat.contains("Cancel"),
            "{flat}"
        );
    }
}

#[test]
fn k5b_peer_add_decision_row_keeps_its_emphasis() {
    let buf = paint_decision(130, "c1", |state| {
        decision_queue::raise_peer_add(state, peer_pending("ab", None));
    });
    let y = (0..buf.area.height)
        .find(|y| row_text(&buf, *y).contains("Awaiting your decision"))
        .expect("decision row");
    let x = col_of(&buf, y, "Awaiting").expect("decision text");
    let cell = buf.cell((x, y)).expect("decision cell");
    assert!(cell.modifier.contains(ratatui::style::Modifier::BOLD));
    assert_eq!(cell.fg, Theme::dark().colors.accent);
}

pub(crate) fn retract_pending(card: &str) -> TeamRetractPayload {
    TeamRetractPayload {
        conversation_id: "conv".to_owned(),
        target: RetractTarget::Item {
            peer: "jun-dev".into(),
            item: "ri_x".into(),
        },
        task: None,
        card: card.to_owned(),
        armed: true,
        request: None,
    }
}

#[test]
fn k5b_retract_card_wraps_long_words_and_loses_nothing_at_60_80_130() {
    let alias = format!("{}{G}{A}", C.repeat(20));
    let long_id = "ri_V1StGXR8Z5jdHi6BmyTqWnEcZvPkLd8".repeat(6);
    let wide_id = format!("{}{G}{A}{}", C.repeat(70), "x".repeat(60));
    for id in [long_id, wide_id] {
        let card = format!(
            "Retract on {alias}'s host\n\nitem         {id}\n\n\
             This marks the item on their host. It does not delete it."
        );
        for width in WIDTHS {
            let buf = paint_decision(width, "conv", |state| {
                decision_queue::raise_team_retract(state, retract_pending(&card), Responder::Local);
            });
            let flat = compact(&buf);
            let want: String = card.split_whitespace().collect();
            assert!(flat.contains(&want), "width {width}: {flat}");
            assert!(
                flat.contains("Retract") && flat.contains("Cancel"),
                "{flat}"
            );
        }
    }
}

// ── apply card ──────────────────────────────────────────────────────────────

pub(crate) fn apply_pending(files: Vec<String>, predates: bool) -> PendingArtifactCard {
    let hash = |seed: &str| ContentHash::parse_hex(&seed.repeat(64)).expect("hash");
    PendingArtifactCard {
        conversation_id: "conversation".to_owned(),
        artifact: EvidenceArtifact {
            id: ArtifactId::from(hash("e")),
            kind: ArtifactKind::Patch,
            producer: AgentId::parse("spoke-1").expect("agent"),
            content_hash: hash("0"),
            authority: CapabilityTokenId::default(),
            provenance: vec![ProvenanceTag::UserOriginated],
            depends_on: Vec::new(),
            review: Some(ReviewStatus::Pending),
            host: HostBinding::new("host-A", "workspace"),
        },
        files,
        workspace: std::path::PathBuf::from("/workspace"),
        predates_apply_records: predates,
        mode: ArtifactCardMode::Apply,
    }
}

#[test]
fn k5b_apply_card_never_clips_files_or_consequences_at_60_80_130() {
    let cjk: Vec<String> = (0..14).map(|_| format!("{C}{C}/{C}{C}.rs")).collect();
    let unspaced = vec![format!("{}{G}{A}{}.rs", C.repeat(60), "x".repeat(40))];
    for files in [cjk, unspaced] {
        for width in WIDTHS {
            let card = apply_pending(files.clone(), true);
            let buf = paint_decision(width, "conversation", |state| {
                decision_queue::raise_artifact(state, card);
            });
            let flat = compact(&buf);
            assert!(flat.contains(&files.join("")), "{width}: {flat}");
            for needle in [
                "Apply state unknown — this journal predates apply records;",
                "patch may already be in the working tree.",
                "Warning: interruption can leave this patch indeterminate.",
                "Recover with: /artifact resolve <id> present|absent.",
                "Apply",
                "Cancel",
            ] {
                let needle: String = needle.split_whitespace().collect();
                assert!(flat.contains(&needle), "{width}: {needle:?} in {flat:?}");
            }
        }
    }
}

#[test]
fn k5b_apply_card_file_word_boundary_at_the_real_budget() {
    for width in WIDTHS {
        // `frame_rect` gives the same inner width as decision_frame::inner_width:
        // a full sheet below 100 columns, otherwise a centered 90% frame.
        let area =
            rustain::adapters::tui::presentation::geometry::frame_rect(Rect::new(0, 0, width, 100));
        let inner = usize::from(area.width - 2);
        let budget = inner - "  ".width();
        let f = |n: usize| "f".repeat(n);
        let cjk_fit = format!("{}{}", C.repeat(budget / 2), f(budget % 2));
        let cases = [
            (f(budget), vec![format!("  {}", f(budget))]),
            (f(budget + 1), vec![String::new(), f(budget + 1)]),
            (cjk_fit.clone(), vec![format!("  {cjk_fit}")]),
            (
                format!("{cjk_fit}{C}"),
                vec![String::new(), format!("{cjk_fit}{C}")],
            ),
            (
                format!("{}{G}", f(budget - 2)),
                vec![format!("  {}{G}", f(budget - 2))],
            ),
            (
                format!("{}{G}", f(budget - 1)),
                vec![String::new(), format!("{}{G}", f(budget - 1))],
            ),
            (
                format!("{}{A}z", f(budget - 1)),
                vec![String::new(), format!("{}{A}z", f(budget - 1))],
            ),
            (f(inner + 1), vec![String::new(), f(inner), f(1)]),
        ];
        for (name, expected) in cases {
            let card = apply_pending(vec![name.clone()], false);
            let buf = paint_decision_in_area(width, "conversation", area, |state| {
                decision_queue::raise_artifact(state, card);
            });
            let painted = rows(&buf);
            let start = painted
                .iter()
                .position(|row| row.contains("  Files:"))
                .unwrap();
            let end = painted
                .iter()
                .position(|row| row.contains("  Workspace:"))
                .unwrap();
            let chunks: Vec<String> = painted[start + 1..end]
                .iter()
                .map(|row| {
                    row.split_once('│')
                        .and_then(|(_, inner)| inner.rsplit_once('│'))
                        .expect("file row within frame borders")
                        .0
                        .trim_end()
                        .to_owned()
                })
                .collect();
            assert_eq!(chunks, expected, "width {width} name {name:?}");
            assert!(
                chunks
                    .iter()
                    .map(|chunk| chunk.trim())
                    .collect::<String>()
                    .contains(&name),
                "{width}: {name:?}"
            );
        }
    }
}

// ── skill trust inspect rows ────────────────────────────────────────────────

#[test]
fn k5b_skill_inspect_rows_wrap_by_cells_and_never_truncate() {
    // 21-6 6-C: the old fixed 78-cell budget ellipsized (`...`); the frame body wraps every
    // line on grapheme boundaries at the frame's inner width and shows the whole file.
    let a = |n: usize| "a".repeat(n);
    let cases: Vec<String> = vec![
        a(78),
        a(79),
        C.repeat(39),
        C.repeat(40),
        format!("{}{G}", a(76)),
        format!("{}{G}tail", a(74)),
        format!("{}{A}zzzz", a(74)),
    ];
    let theme = Theme::dark();
    for line in cases {
        let lines = skill_trust_prompt::inspect_lines("s", &line, 78, &theme);
        let n = lines.len() as u16;
        let p = paint(78, n, |f| {
            let area = f.area();
            Paragraph::new(lines).render(area, f.buffer_mut());
        });
        let rows: Vec<String> = (1..n).map(|y| row_text(&p.buffer, y)).collect();
        for row in &rows {
            assert!(UnicodeWidthStr::width(row.trim_end()) <= 78, "{row:?}");
            assert!(!row.contains("..."), "nothing is ellipsized: {row:?}");
        }
        let joined: String = rows.iter().map(|row| row.trim_end()).collect();
        assert_eq!(joined, line, "the line is shown whole, wrapped");
        // Exact fit stays one row; one cell over wraps to a second.
        let fits = UnicodeWidthStr::width(line.as_str()) <= 78;
        assert_eq!(rows.len(), if fits { 1 } else { 2 }, "line {line:?}");
        assert_whole_clusters(&p.buffer);
    }
}

// ── tool block ──────────────────────────────────────────────────────────────

fn bash_call(name: &str, command: &str) -> ToolCallInfo {
    ToolCallInfo {
        id: "test_id".to_string(),
        name: name.to_string(),
        input: serde_json::json!({ "command": command }),
        result: Some(ToolResultInfo {
            content: "ok".to_string(),
            is_error: false,
            diff: WriteDiffState::NotAWrite,
            images: Vec::new(),
        }),
        started_at_ms: Some(1_000_000),
        completed_at_ms: Some(1_002_300),
        status: None,
        provider_id: None,
    }
}

fn paint_block(tc: &ToolCallInfo, width: u16) -> Painted {
    let theme = Theme::dark();
    let clock = MockClock::at_wall_ms(0);
    let lines = render_tool_block_lines(tc, &theme, width, &clock);
    let n = lines.len() as u16;
    paint(width, n, |f| {
        let area = f.area();
        Paragraph::new(lines).render(area, f.buffer_mut());
    })
}

#[test]
fn k5b_tool_block_bash_target_is_a_grapheme_safe_display_excerpt() {
    // Story 21.9 D14: the 60-cell `...` summary became a fit. At 40 cells the
    // elapsed piece drops first, then the command keeps its head inside the
    // 31 cells the lead, glyph, verb and spaces leave; the cut is whole
    // graphemes with a one-cell `…`.
    const WIDTH: u16 = 40;
    const BUDGET: usize = 31;
    let a = |n: usize| "a".repeat(n);
    let cases: Vec<(String, String)> = vec![
        (a(20), a(20)),
        (a(BUDGET), a(BUDGET)),
        (a(BUDGET + 1), format!("{}…", a(BUDGET - 1))),
        (C.repeat(15), C.repeat(15)),
        (C.repeat(16), format!("{}…", C.repeat(15))),
    ];
    for (command, want) in cases {
        let p = paint_block(&bash_call("Bash", &command), WIDTH);
        let row = row_text(&p.buffer, 0);
        let target = row
            .strip_prefix("┄ ✓ Bash ")
            .unwrap_or_else(|| panic!("row {row:?}"));
        let target = target.strip_suffix(" · 2.3s").unwrap_or(target);
        assert_eq!(target, want, "row {row:?}");
        assert_whole_clusters(&p.buffer);
    }
    // Clusters (ZWJ family, combining accent) are never cut: the excerpt is a
    // prefix of the command up to the `…`, whatever the cell widths are.
    for command in [
        format!("{}{G}tail", a(28)),
        format!("{}{G}{G}{G}tail", a(26)),
        format!("{}{A}zzzz", a(29)),
        format!("{}{A}{A}{A}{A}zzzz", a(28)),
    ] {
        let p = paint_block(&bash_call("Bash", &command), WIDTH);
        let row = row_text(&p.buffer, 0);
        let target = row
            .strip_prefix("┄ ✓ Bash ")
            .unwrap_or_else(|| panic!("row {row:?}"));
        let target = target.strip_suffix(" · 2.3s").unwrap_or(target);
        let kept = target.strip_suffix('…').unwrap_or(target);
        assert!(command.starts_with(kept), "{kept:?} of {command:?}");
        assert!(
            unicode_width::UnicodeWidthStr::width(target) <= BUDGET,
            "{target:?}"
        );
        assert_whole_clusters(&p.buffer);
    }
}

#[test]
fn k5b_tool_block_compact_header_keeps_display_names_and_source_input() {
    // AC4 deletes expanded chrome and raw input rows from the Stage, not the
    // display-name, grapheme-boundary or immutable-source contracts.
    let raw = format!("echo {}{G}{A}", C.repeat(10));
    let cases: Vec<(String, String)> = vec![
        ("Bash".into(), "Bash".into()),
        (format!("mcp__{C}{C}__Read"), format!("[{C}{C}] Read")),
        (format!("mcp__{G}{A}__run"), format!("[{G}{A}] run")),
        (
            format!("mcp__{}__do", C.repeat(10)),
            format!("[{}] do", C.repeat(10)),
        ),
    ];
    for (name, display) in &cases {
        for width in WIDTHS {
            let call = bash_call(name, &raw);
            let p = paint_block(&call, width);
            assert_eq!(p.buffer.area.height, 1, "{name}@{width}");
            assert_eq!(row_cells(&p.buffer, 0), width as usize, "{name}@{width}");
            let header = row_text(&p.buffer, 0);
            assert!(
                header.starts_with(&format!("┄ ✓ {display} ")),
                "{name}@{width}: {header:?}"
            );
            assert_eq!(call.input["command"].as_str(), Some(raw.as_str()));
            assert_whole_clusters(&p.buffer);
        }
    }
    let wide = format!("echo {}{G}{A}", C.repeat(60));
    let call = bash_call("Bash", &wide);
    let p = paint_block(&call, 80);
    assert_eq!(p.buffer.area.height, 1);
    assert_eq!(call.input["command"].as_str(), Some(wide.as_str()));
    assert_whole_clusters(&p.buffer);
}

// ── usage panel ─────────────────────────────────────────────────────────────

fn usage_state(model: &str) -> UsagePanelState {
    let mut state = UsagePanelState::new();
    state.initialize();
    state.turn_rows.push(TurnUsageRow {
        turn_index: 0,
        model: model.to_string(),
        tokens_in: 1200,
        tokens_out: 340,
        cost_usd: None,
    });
    state.missing_pricing_models.push(model.to_string());
    state
}

#[test]
fn k5b_usage_panel_model_column_counts_cells_at_60_80_130() {
    let m = |n: usize| "m".repeat(n);
    let cases: Vec<(String, String)> = vec![
        (m(24), m(24)),
        (m(25), format!("{}…", m(24))),
        (C.repeat(9), C.repeat(9)),
        (C.repeat(13), format!("{}…", C.repeat(12))),
        (format!("{}{G}", m(22)), format!("{}{G}", m(22))),
        (format!("{}{G}", m(23)), format!("{}…", m(23))),
        (format!("{}{A}zz", m(23)), format!("{}{A}…", m(23))),
    ];
    let theme = Theme::dark();
    for width in WIDTHS {
        for (model, want) in &cases {
            let state = usage_state(model);
            let p = paint(width, 40, |f| {
                usage_panel::render(f, f.area(), &state, &theme)
            });
            let all = rows(&p.buffer);
            let row = all
                .iter()
                .find(|r| r.contains("[t-0] "))
                .unwrap_or_else(|| panic!("no turn row at {width}: {all:?}"));
            let shown = row
                .split_once("[t-0] ")
                .and_then(|(_, after)| after.split_once(" · ↑"))
                .map(|(m, _)| m)
                .unwrap_or_else(|| panic!("model column clipped at {width}: {row:?}"));
            assert_eq!(shown, want, "width {width} model {model:?}");
            assert_whole_clusters(&p.buffer);
        }
    }
}

// ── result row ──────────────────────────────────────────────────────────────

fn paint_result_row(label: &str, summary: &str, width: u16) -> (String, String) {
    let line = render_result_row(
        &ResultRowSnapshot {
            agent_label: label.to_string(),
            result: SpokeResult::Completed {
                summary: summary.to_string(),
            },
            slot: 0,
            is_self: false,
            rerun_count: 0,
            rerunning: false,
        },
        width,
    );
    let p = paint(width, 1, |f| {
        let area = f.area();
        Paragraph::new(line).render(area, f.buffer_mut());
    });
    assert_whole_clusters(&p.buffer);
    let row = row_text(&p.buffer, 0);
    let rest: String = row.chars().skip(3).collect();
    let mut parts = rest.split(" · ");
    let name = parts.next().unwrap_or_default().to_string();
    let salience = parts.next().unwrap_or_default().to_string();
    assert_eq!(parts.next(), Some("\u{21B5}"), "drill clipped: {row:?}");
    (name, salience)
}

#[test]
fn k5b_result_row_applies_the_byte_cap_and_the_cell_budget_independently() {
    let x = |n: usize| "x".repeat(n);
    // Remaining cells with a 5-cell label: width - 15 (60 -> 45, 80 -> 65).
    for width in [60u16, 80] {
        let r = width as usize - 15;
        let cases: Vec<(String, String)> = vec![
            (x(r), x(r)),
            (x(r + 1), x(r)),
            (C.repeat(r.div_ceil(2)), C.repeat((r - 1) / 2)),
            (format!("{}{G}tail", x(r - 1)), x(r - 1)),
            (format!("{}{A}zz", x(r - 1)), format!("{}{A}", x(r - 1))),
        ];
        for (summary, want) in cases {
            let (name, salience) = paint_result_row("agent", &summary, width);
            assert_eq!(name, "agent");
            assert_eq!(salience, want, "width {width} summary {summary:?}");
        }
    }
    // 200 cells of room: only the 120-byte salience ceiling can bite.
    let cases: Vec<(String, String)> = vec![
        (x(300), x(120)),
        (C.repeat(80), C.repeat(40)),
        (G.repeat(7), G.repeat(6)),
        (A.repeat(41), A.repeat(40)),
        (format!("{}{A}zz", x(118)), x(118)),
    ];
    for (summary, want) in cases {
        let (_, salience) = paint_result_row("agent", &summary, 200);
        assert_eq!(salience, want, "summary {summary:?}");
        assert!(salience.len() <= 120);
    }
    // Label: 20 display cells at whole graphemes.
    for (label, want) in [
        (x(21), x(20)),
        (C.repeat(20), C.repeat(10)),
        (format!("{}{G}", x(19)), x(19)),
        (format!("{}{A}y", x(19)), format!("{}{A}", x(19))),
    ] {
        let (name, _) = paint_result_row(&label, "ok", 130);
        assert_eq!(name, want, "label {label:?}");
    }
}

// ── team send rows ──────────────────────────────────────────────────────────

#[test]
fn k5b_team_send_rows_align_outcomes_at_60_80_130() {
    let make = |alias: &str, outcome: Option<RecipientOutcome>| TeamSendRow {
        alias: alias.to_owned(),
        outcome,
        message_type: SemanticMessageType::Unknown,
    };
    let wide = [
        make("田中", Some(RecipientOutcome::Delivered)),
        make("ab", Some(RecipientOutcome::Delivered)),
        make(G, Some(RecipientOutcome::Delivered)),
        make(A, Some(RecipientOutcome::Delivered)),
    ];
    let ascii = [
        make("abcd", Some(RecipientOutcome::Delivered)),
        make("ab", Some(RecipientOutcome::Delivered)),
    ];
    for width in WIDTHS {
        for table in [&wide[..], &ascii[..]] {
            let text = render_team_send_rows(table);
            let n = text.lines().count() as u16;
            let p = paint(width, n, |f| {
                let area = f.area();
                Paragraph::new(text.as_str()).render(area, f.buffer_mut());
            });
            // `  ● ` + the 4-cell widest alias + two spaces: column 10.
            let cols: Vec<Option<u16>> =
                (0..n).map(|y| col_of(&p.buffer, y, "delivered")).collect();
            assert!(
                cols.iter().all(|c| *c == Some(10)),
                "width {width}: {cols:?}"
            );
            assert_whole_clusters(&p.buffer);
        }
    }
}

#[cfg(feature = "a2a")]
#[test]
fn k5b_team_thread_turn_ids_align_at_60_80_130() {
    use rustain::adapters::a2a::send::{ServedThread, ServedTurn, ThreadRead};
    use rustain::adapters::tui::handlers::team_command::render_team_thread;
    use rustain::domain::models::{Authorship, ReplyKind};

    let turn = |id: &str, reply: bool| ServedTurn {
        turn_id: id.to_owned(),
        reply,
        task: (!reply).then(|| "t-1".to_owned()),
        in_reply_to: None,
        removed: false,
        content: Some("body".to_owned()),
        content_hash: None,
        kind: ReplyKind::Answer,
        status: None,
        retro: None,
        authorship: Authorship::HumanWritten,
        retracted_at_ms: None,
    };
    let c10 = C.repeat(10);
    let c7 = C.repeat(7);
    // (alias, expected turn-id column): widest of alias / `another sender`
    // (14) / the 8-cell floor, plus the 2-cell indent and 2-cell gap.
    for (alias, col) in [
        (c10.as_str(), 24u16),
        (c7.as_str(), 18),
        ("田中", 18),
        ("ab", 18),
        (G, 18),
        (A, 18),
    ] {
        let read = ThreadRead::Read {
            thread: ServedThread {
                root: "ri_q".to_owned(),
                turns: vec![
                    turn("ti_one", false),
                    turn("ti_two", true),
                    turn("ti_three", false),
                ],
                omitted_before: 0,
                principal_collapsed: false,
                principal_bound: false,
            },
            mine: vec![false, false, true],
        };
        let text = render_team_thread(alias, "ri_q", &read, false);
        for width in WIDTHS {
            let n = text.lines().count() as u16;
            let p = paint(width, n, |f| {
                let area = f.area();
                Paragraph::new(text.as_str()).render(area, f.buffer_mut());
            });
            for id in ["ti_one", "ti_two", "ti_three"] {
                let found: Vec<u16> = (0..n).filter_map(|y| col_of(&p.buffer, y, id)).collect();
                assert_eq!(found, vec![col], "alias {alias:?} width {width} id {id}");
            }
            assert_whole_clusters(&p.buffer);
        }
    }
}
