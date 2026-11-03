//! K5 (second half) — dynamic cards, panels and producers (Story 21.11, AC5).
//!
//! Covers the consolidation/forget/peer-add/retract/apply/skill-trust cards,
//! the tool block, usage panel, result row and the team send table/thread
//! label columns. Every test enters through the public renderer the production
//! draw path uses, paints into a real `Buffer`, and reads the cells back.
//! Expected values are written from the C/G/A fixtures and from independent
//! properties of the painted rows (spanning the pane, keeping both borders,
//! whole clusters, maximal prefix of the source line), never from the
//! production clipping formulas.
//!
//! Each case has an ASCII positive control and an exact-fit / fit-minus-one
//! boundary at the real content budget (pane width minus the border and the
//! row's own prefix/suffix).
//!
//! `config::short_reason` is private and the reload handler's context
//! (`ReloadContext`, `ComposeContext`) is assembled from crate internals, so
//! the reload-failure producer (`handle_config_reload_with_two_pass` →
//! `AppEvent::ConfigReloaded.error`) is covered by the `display_width_*` unit
//! tests in `src/adapters/tui/handlers/config.rs`, not here.

use super::support::*;

use ed25519_dalek::SigningKey;
use ratatui::buffer::Buffer;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use rustain::adapters::cli::peer::rows::confirm_card_text;
use rustain::adapters::tui::handlers::team_command::{TeamSendRow, render_team_send_rows};
use rustain::adapters::tui::state::{
    ArtifactCardMode, PendingArtifactCard, PendingConsolidationCard, PendingForgetCard,
    PendingPeerAdd, PendingTeamRetract, TurnUsageRow, UsagePanelState,
};
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::result_row::{ResultRowSnapshot, render_result_row};
use rustain::adapters::tui::widgets::tool_block::{ToolBlockState, render_tool_block_lines};
use rustain::adapters::tui::widgets::{
    apply_card, consolidation_card, forget_card, inline_card, peer_add_prompt, skill_trust_prompt,
    team_retract_prompt, usage_panel,
};
use rustain::domain::clock::MockClock;
use rustain::domain::models::orchestration::SpokeResult;
use rustain::domain::models::{
    AgentId, ArtifactId, ArtifactKind, CapabilityTokenId, ContentHash, EvidenceArtifact,
    FocusState, HostBinding, MemoryEntry, MemoryFact, PeerTicket, ProvenanceTag, RecipientOutcome,
    ReviewStatus, SemanticMessageType, ToolCallInfo, ToolResultInfo, WriteDiffState,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const WIDTHS: [u16; 3] = [60, 80, 130];

/// Paint `lines` as the real bottom-anchored inline card (single or double
/// border) filling a `width`-wide pane exactly as tall as the card.
fn paint_card(width: u16, lines: Vec<Line<'_>>, decision: bool) -> Painted {
    let theme = Theme::dark();
    let height = lines.len() as u16 + 2;
    paint(width, height, |f| {
        let area = f.area();
        if decision {
            inline_card::render_bottom_anchored_decision_card(
                f.buffer_mut(),
                lines,
                theme.colors.accent,
                area,
            );
        } else {
            inline_card::render_bottom_anchored_card(
                f.buffer_mut(),
                lines,
                theme.colors.accent,
                area,
            );
        }
    })
}

/// Row `y` between the two border glyphs (left `l`, right `r`), checked to
/// span exactly `width` cells; trailing blanks removed.
fn between(p: &Painted, y: u16, width: u16, l: char, r: char) -> String {
    assert_eq!(row_cells(&p.buffer, y), width as usize, "row {y} width");
    let text: String = glyphs(&p.buffer, y).concat();
    text.strip_prefix(l)
        .and_then(|t| t.strip_suffix(r))
        .unwrap_or_else(|| panic!("row {y} lost its borders: {text:?}"))
        .trim_end()
        .to_string()
}

/// The painted `inner` row must be the longest whole-grapheme prefix of the
/// source `line` that fits `budget` cells (or the whole line when it fits).
fn assert_maximal_prefix(inner: &str, line: &str, budget: usize) {
    let want = line.trim_end();
    assert!(
        want.starts_with(inner),
        "{inner:?} is not a prefix of {want:?}"
    );
    if want.width() <= budget {
        assert_eq!(inner, want);
    } else {
        let used = inner.width();
        assert!(used <= budget, "{inner:?} overflows {budget}");
        let next = want[inner.len()..].graphemes(true).next().unwrap_or("");
        assert!(
            used + next.width() > budget || next.chars().all(char::is_whitespace),
            "{inner:?} is not maximal in {want:?} at {budget}"
        );
    }
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
    let a = |n: usize| "a".repeat(n);
    for width in WIDTHS {
        // 2 border + 6 marker + 11 cells of "## Build — ".
        let b = width as usize - 19;
        let facts: Vec<(String, String)> = vec![
            (a(b), a(b)),
            (a(b + 1), format!("{}…", a(b - 1))),
            (C.repeat(b), format!("{}…", C.repeat((b - 1) / 2))),
            (format!("{}{G}", a(b - 2)), format!("{}{G}", a(b - 2))),
            (format!("{}{G}z", a(b - 2)), format!("{}…", a(b - 2))),
            (format!("{}{A}bbb", a(b - 2)), format!("{}{A}…", a(b - 2))),
        ];
        for (fact, want) in facts {
            let card = consolidation_card_with("Build", &fact, None);
            let lines =
                consolidation_card::render_consolidation_card_lines(&card, &Theme::dark(), width);
            let p = paint_card(width, lines, false);
            let body = between(&p, 3, width, '│', '│');
            assert_eq!(
                body.strip_prefix("  [x] ## Build — "),
                Some(want.as_str()),
                "width {width} fact {fact:?}"
            );
            assert_whole_clusters(&p.buffer);
        }
        // Detail rows sit behind an 8-cell indent.
        let d = width as usize - 10;
        let details: Vec<(String, String)> = vec![
            (a(d), a(d)),
            (C.repeat(d / 2 + 1), format!("{}…", C.repeat(d / 2 - 1))),
            (format!("{}{G}z", a(d - 2)), format!("{}…", a(d - 2))),
        ];
        for (detail, want) in details {
            let card = consolidation_card_with("Build", "f", Some(&detail));
            let lines =
                consolidation_card::render_consolidation_card_lines(&card, &Theme::dark(), width);
            let p = paint_card(width, lines, false);
            let body = between(&p, 4, width, '│', '│');
            assert_eq!(
                body.strip_prefix("        "),
                Some(want.as_str()),
                "width {width}"
            );
            assert_whole_clusters(&p.buffer);
        }
    }
}

#[test]
fn k5b_consolidation_long_category_shrinks_the_excerpt_budget() {
    let category = format!("{}{G}{A}", C.repeat(20));
    let card = consolidation_card_with(&category, &"a".repeat(10), None);
    let lines = consolidation_card::render_consolidation_card_lines(&card, &Theme::dark(), 60);
    let p = paint_card(60, lines, false);
    let body = between(&p, 3, 60, '│', '│');
    // 58 content - 6 marker - 49 prefix cells leave three cells for the fact.
    assert_eq!(body, format!("  [x] ## {category} — aa…"));
    assert_whole_clusters(&p.buffer);
}

#[test]
fn k5b_forget_summary_leaves_room_for_the_key_suffix_at_60_80_130() {
    let m = |n: usize| "m".repeat(n);
    for width in WIDTHS {
        // 2 border + 8 focus/marker + 20 cells of "  [00000000deadbeef]".
        let d = width as usize - 30;
        let summaries: Vec<(String, String)> = vec![
            (m(d), m(d)),
            (m(d + 1), format!("{}…", m(d - 1))),
            (C.repeat(d), format!("{}…", C.repeat(d / 2 - 1))),
            (format!("{}{G}", m(d - 2)), format!("{}{G}", m(d - 2))),
            (format!("{}{G}z", m(d - 2)), format!("{}…", m(d - 2))),
            (format!("{}{A}zzz", m(d - 2)), format!("{}{A}…", m(d - 2))),
        ];
        for (summary, want) in summaries {
            let card = PendingForgetCard {
                conversation_id: "c1".to_string(),
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
            let lines = forget_card::render_forget_card_lines(&card, &Theme::dark(), width);
            let p = paint_card(width, lines, false);
            let body = between(&p, 3, width, '│', '│');
            let summary_part = body
                .strip_prefix(">   [x] ")
                .and_then(|r| r.strip_suffix("  [00000000deadbeef]"))
                .unwrap_or_else(|| panic!("width {width}: prefix/key suffix lost in {body:?}"));
            assert_eq!(summary_part, want, "width {width} summary {summary:?}");
            assert_whole_clusters(&p.buffer);
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
        prior_focus: FocusState::Chat,
    }
}

#[test]
fn k5b_peer_add_card_rows_clip_at_whole_graphemes_at_60_80_130() {
    let alias = format!("{}{G}{A}", C.repeat(20));
    let opaque = "a1B2c3D4".repeat(20);
    let pending = peer_pending(&alias, Some(&opaque));
    let source_lines: Vec<&str> = pending.card.lines().collect();
    assert!(source_lines.len() >= 8, "{source_lines:?}");
    for width in WIDTHS {
        let lines = peer_add_prompt::render_peer_add_lines(&pending, &Theme::dark(), width);
        assert_eq!(lines.len(), source_lines.len());
        let p = paint_card(width, lines, false);
        // 2 border + 2 indent.
        let budget = width as usize - 4;
        for (i, line) in source_lines.iter().enumerate() {
            let row = between(&p, i as u16 + 1, width, '│', '│');
            let inner = if row.is_empty() {
                ""
            } else {
                row.strip_prefix("  ")
                    .unwrap_or_else(|| panic!("indent: {row:?}"))
            };
            assert_maximal_prefix(inner, line, budget);
        }
        assert_whole_clusters(&p.buffer);
        assert_no_partial_family(&screen(&p.buffer));
        // The alias row keeps the operator-typed alias whole where it fits.
        assert!(
            screen(&p.buffer).contains(&format!("Add peer — {alias}")),
            "width {width}: alias lost"
        );
    }
}

#[test]
fn k5b_peer_add_decision_line_is_whole_at_130() {
    let pending = peer_pending("ab", None);
    let lines = peer_add_prompt::render_peer_add_lines(&pending, &Theme::dark(), 130);
    let p = paint_card(130, lines, false);
    let screen = screen(&p.buffer);
    assert!(
        screen.contains("Awaiting your decision.  [y] Confirm and pin  [n] Cancel"),
        "{screen}"
    );
}

#[test]
fn k5b_peer_add_decision_row_keeps_its_emphasis() {
    let pending = peer_pending("ab", None);
    let theme = Theme::dark();
    let lines = peer_add_prompt::render_peer_add_lines(&pending, &theme, 130);
    let p = paint_card(130, lines, false);
    let y = (0..p.buffer.area.height)
        .find(|y| row_text(&p.buffer, *y).contains("Awaiting your decision"))
        .expect("decision row");
    // Cell 3 is the `A` of "Awaiting" (border, then the 2-cell indent).
    let cell = p.buffer.cell((3, y)).expect("decision cell");
    assert_eq!(cell.symbol(), "A");
    assert!(cell.modifier.contains(ratatui::style::Modifier::BOLD));
    assert_eq!(cell.fg, theme.colors.accent);
}

pub(crate) fn retract_pending(card: &str) -> PendingTeamRetract {
    PendingTeamRetract {
        conversation_id: "conv".to_owned(),
        peer: "jun-dev".to_owned(),
        item_id: "ri_x".to_owned(),
        task: None,
        card: card.to_owned(),
        armed: true,
        prior_focus: FocusState::Chat,
    }
}

#[test]
fn k5b_retract_card_wraps_long_words_and_loses_nothing_at_60_80_130() {
    let alias = format!("{}{G}{A}", C.repeat(20));
    let long_id = "ri_V1StGXR8Z5jdHi6BmyTqWnEcZvPkLd8".repeat(6);
    let wide_id = format!("{}{G}{A}{}", C.repeat(70), "x".repeat(60));
    let keys = "Awaiting your decision.  [y] Retract  [n] Cancel";
    for id in [long_id, wide_id] {
        let card = format!(
            "Retract on {alias}'s host\n\nitem         {id}\n\n\
             This marks the item on their host. It does not delete it.\n\n{keys}"
        );
        for width in WIDTHS {
            let lines = team_retract_prompt::render_team_retract_lines(
                &retract_pending(&card),
                &Theme::dark(),
                width,
            );
            let count = lines.len() as u16;
            let p = paint_card(width, lines, true);
            let painted: Vec<String> = (1..=count)
                .map(|y| between(&p, y, width, '║', '║'))
                .collect();
            // Nothing vanishes: every non-blank character survives, in order.
            let flat: String = painted.concat().split_whitespace().collect();
            let want: String = card.split_whitespace().collect();
            assert_eq!(flat, want, "width {width}");
            assert!(
                painted.last().is_some_and(|r| r.contains("[n] Cancel")),
                "{painted:?}"
            );
            assert_whole_clusters(&p.buffer);
            assert_no_partial_family(&screen(&p.buffer));
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
        prior_focus: FocusState::Input,
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
            let lines = apply_card::render_apply_card_lines(&card, &Theme::dark(), width);
            let count = lines.len() as u16;
            let p = paint_card(width, lines, true);
            let painted: Vec<String> = (1..=count)
                .map(|y| between(&p, y, width, '║', '║'))
                .collect();
            // The file region runs from the `Files:` row to the `Workspace:` row.
            let start = painted
                .iter()
                .position(|r| r.starts_with("  Files: "))
                .unwrap();
            let end = painted
                .iter()
                .position(|r| r.starts_with("  Workspace:"))
                .unwrap();
            let listed: String = painted[start..end]
                .concat()
                .replace("Files:", "")
                .split_whitespace()
                .collect();
            let want: String = files.join(",").split_whitespace().collect();
            assert_eq!(listed, want, "width {width}");
            // The consequences and both keys are on screen, wrapped not cut.
            let flat = painted
                .iter()
                .map(|r| r.trim())
                .collect::<Vec<_>>()
                .join(" ");
            for needle in [
                "Apply state unknown — this journal predates apply records;",
                "patch may already be in the working tree.",
                "Warning: interruption can leave this patch indeterminate.",
                "Recover with: /artifact resolve <id> present|absent.",
                "[y] Apply",
                "[n] Cancel",
            ] {
                assert!(
                    flat.contains(needle),
                    "width {width}: {needle:?} in {flat:?}"
                );
            }
            assert_whole_clusters(&p.buffer);
            assert_no_partial_family(&screen(&p.buffer));
        }
    }
}

#[test]
fn k5b_apply_card_file_word_boundary_at_the_real_budget() {
    for width in WIDTHS {
        // 2 border + 9 indent leave `width - 11` cells (odd: 49/69/119).
        let l = width as usize - 11;
        let f = |n: usize| "f".repeat(n);
        let cases: Vec<(String, Vec<String>)> = vec![
            (f(l), vec![f(l)]),
            (f(l + 1), vec![f(l), f(1)]),
            (C.repeat((l - 1) / 2), vec![C.repeat((l - 1) / 2)]),
            (
                C.repeat((l - 1) / 2 + 1),
                vec![C.repeat((l - 1) / 2), C.to_string()],
            ),
            (format!("{}{G}", f(l - 2)), vec![format!("{}{G}", f(l - 2))]),
            (format!("{}{G}", f(l - 1)), vec![f(l - 1), G.to_string()]),
            (
                format!("{}{A}z", f(l - 1)),
                vec![format!("{}{A}", f(l - 1)), "z".to_string()],
            ),
        ];
        for (name, want) in cases {
            let card = apply_pending(vec![name.clone()], false);
            let lines = apply_card::render_apply_card_lines(&card, &Theme::dark(), width);
            let count = lines.len() as u16;
            let p = paint_card(width, lines, true);
            let painted: Vec<String> = (1..=count)
                .map(|y| between(&p, y, width, '║', '║'))
                .collect();
            let start = painted
                .iter()
                .position(|r| r.starts_with("  Files: "))
                .unwrap();
            let end = painted
                .iter()
                .position(|r| r.starts_with("  Workspace:"))
                .unwrap();
            let chunks: Vec<String> = painted[start..end]
                .iter()
                .map(|r| r.chars().skip(9).collect())
                .collect();
            assert_eq!(chunks, want, "width {width} name {name:?}");
        }
    }
}

// ── skill trust inspect rows ────────────────────────────────────────────────

#[test]
fn k5b_skill_inspect_rows_keep_the_fixed_78_cell_budget() {
    let a = |n: usize| "a".repeat(n);
    let cases: Vec<(String, String)> = vec![
        (a(78), a(78)),
        (a(79), format!("{}...", a(75))),
        (C.repeat(39), C.repeat(39)),
        (C.repeat(40), format!("{}...", C.repeat(37))),
        (format!("{}{G}", a(76)), format!("{}{G}", a(76))),
        (format!("{}{G}tail", a(74)), format!("{}...", a(74))),
        (format!("{}{A}zzzz", a(74)), format!("{}{A}...", a(74))),
    ];
    let theme = Theme::dark();
    for (line, want) in cases {
        let lines = skill_trust_prompt::render_inspect_lines("s", &line, 5, &theme);
        let n = lines.len() as u16;
        let p = paint(130, n, |f| {
            let area = f.area();
            Paragraph::new(lines).render(area, f.buffer_mut());
        });
        let row = row_text(&p.buffer, 1);
        let shown = row
            .strip_prefix("┃ ")
            .and_then(|r| r.strip_suffix(" ┃"))
            .unwrap_or_else(|| panic!("rails lost: {row:?}"));
        assert_eq!(shown, want, "line {line:?}");
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

fn paint_block(tc: &ToolCallInfo, collapsed: bool, width: u16) -> Painted {
    let state = ToolBlockState {
        collapsed,
        peek_active: false,
    };
    let theme = Theme::dark();
    let clock = MockClock::at_wall_ms(0);
    let lines = render_tool_block_lines(tc, &theme, &state, width, &clock);
    let n = lines.len() as u16;
    paint(width, n, |f| {
        let area = f.area();
        Paragraph::new(lines).render(area, f.buffer_mut());
    })
}

#[test]
fn k5b_tool_block_bash_summary_is_a_60_cell_display_excerpt() {
    let a = |n: usize| "a".repeat(n);
    let cases: Vec<(String, String)> = vec![
        (a(60), a(60)),
        (a(61), format!("{}...", a(57))),
        (C.repeat(30), C.repeat(30)),
        (C.repeat(60), format!("{}...", C.repeat(28))),
        (format!("{}{G}tail", a(56)), format!("{}...", a(56))),
        (format!("{}{A}zzzz", a(56)), format!("{}{A}...", a(56))),
    ];
    for (command, want) in cases {
        let p = paint_block(&bash_call("Bash", &command), true, 130);
        let row = row_text(&p.buffer, 0);
        let summary = row
            .split_once("Bash \"")
            .and_then(|(_, after)| after.split_once("\" →"))
            .map(|(s, _)| s);
        assert_eq!(summary, Some(want.as_str()), "row {row:?}");
        assert_whole_clusters(&p.buffer);
    }
}

#[test]
fn k5b_tool_block_expanded_chrome_spans_the_pane_and_keeps_raw_input() {
    // Short enough that the raw input row fits even the 60-cell pane, so every
    // (name, width) case asserts the same exact input row.
    let raw = format!("echo {}{G}{A}", C.repeat(10));
    let json_input = serde_json::json!({ "command": raw }).to_string();
    let cases: Vec<(String, String, String)> = vec![
        // (tool name, displayed name, exact raw input row text)
        ("Bash".into(), "Bash".into(), format!("│ $ {raw}")),
        (
            format!("mcp__{C}{C}__Read"),
            format!("[{C}{C}] Read"),
            format!("│ {json_input}"),
        ),
        (
            format!("mcp__{G}{A}__run"),
            format!("[{G}{A}] run"),
            format!("│ {json_input}"),
        ),
        (
            format!("mcp__{}__do", C.repeat(10)),
            format!("[{}] do", C.repeat(10)),
            format!("│ {json_input}"),
        ),
    ];
    for (name, display, input_row) in &cases {
        for width in WIDTHS {
            let p = paint_block(&bash_call(name, &raw), false, width);
            let last = p.buffer.area.bottom() - 1;
            assert_eq!(row_cells(&p.buffer, last), width as usize, "bottom border");
            assert_eq!(row_cells(&p.buffer, 0), width as usize, "{name}@{width}");
            let top = row_text(&p.buffer, 0);
            assert!(
                top.starts_with(&format!("┌─ {display} ")),
                "{name}@{width}: {top:?}"
            );
            assert!(top.ends_with(" ✓ (2.3s) ┐"), "{name}@{width}: {top:?}");
            // Raw tool input is untouched by the display chrome.
            assert_eq!(&row_text(&p.buffer, 1), input_row, "{name}@{width}");
            assert_whole_clusters(&p.buffer);
        }
    }
    // A command far wider than any pane stays whole in the expanded input
    // line; only its collapsed display summary is excerpted.
    let wide = format!("echo {}{G}{A}", C.repeat(60));
    let theme = Theme::dark();
    let lines = render_tool_block_lines(
        &bash_call("Bash", &wide),
        &theme,
        &ToolBlockState {
            collapsed: false,
            peek_active: false,
        },
        80,
        &MockClock::at_wall_ms(0),
    );
    assert_eq!(lines[1].spans[1].content.as_ref(), format!("$ {wide}"));
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
