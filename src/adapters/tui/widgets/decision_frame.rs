//! The decision frame body (Story 21.6 A4): one L3 `Surface::Decision` painted in
//! 21-4's `frame_rect`, never bottom-anchored over the Stage.
//!
//! Rows, top to bottom: the breadcrumb `Needs you › <kind> <source> | <N> pending` (and,
//! when the frame interrupted another, `Interrupted: <breadcrumb>`), the kind's
//! pinned header, its delimited body, the open text field (`Feedback: …`, `Answer: …`)
//! pinned above the rail, and the rail generated from the registry. The body shrinks
//! first; the rail is never dropped. Untrusted text is sanitized on paint (the effect and
//! any copy keep the original bytes).
//!
//! The kind-specific bodies are the line builders of the old cards, borderless
//! (`ask_user_question`, `skill_trust_prompt`, `plan_approval`, `plan_deviation_card`,
//! `task_skip_cascade_card`, `cancel_plan_confirm_card`, `delegation_card`,
//! `exceptional_spawn_gate`, `agent_inspector::kill_lines`); the heavy ones (a question, a
//! skill file, a plan's contents) are wrapped once per item, width and colours and cached
//! on the queue, so a repaint or a scroll key never re-wraps a megabyte.

use std::sync::Arc;

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_segmentation::UnicodeSegmentation;

use crate::adapters::tui::decision_queue::{
    self, ConsentPayload, DecisionItem, DecisionKey, DecisionKind, DecisionSubject,
    PendingDelegationCard, PermissionInput, PermissionPayload, QuestionPayload, Validity, rail_of,
};
use crate::adapters::tui::keymap::{self, Context, table_card::CardCmd, table_overlay::OverlayCmd};
use crate::adapters::tui::notification_log::{LogFacts, footer_lines};
use crate::adapters::tui::presentation::text_field::TextField;
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::tool_inspector::source::COPY_MAX_BYTES;
use crate::adapters::tui::widgets::exceptional_spawn_gate::{SpawnGateSnapshot, gate_lines};
use crate::adapters::tui::widgets::modal::wrapped;
use crate::adapters::tui::widgets::permission_prompt::{
    ExactInput, FrameRows, PermissionCard, card_rows, review_rows,
};
use crate::adapters::tui::widgets::{
    ModalPart, agent_inspector, ask_user_question, cancel_plan_confirm_card, delegation_card,
    fit_modal, plan_approval, plan_deviation_card, skill_trust_prompt, task_skip_cascade_card,
};
use crate::domain::models::Plan;
use crate::domain::services::approval_presentation;
use crate::domain::services::peer_text::{sanitize_display_line, sanitize_tool_text_block};

/// Cells inside the frame's border at `terminal_width` (mirrors
/// `presentation::geometry::frame_rect`: a full sheet below 100 columns, 90 % above).
#[must_use]
pub(crate) fn inner_width(terminal_width: u16) -> usize {
    let width = if terminal_width < 100 {
        terminal_width
    } else {
        (u32::from(terminal_width) * 9 / 10) as u16
    };
    usize::from(width.saturating_sub(2))
}

/// The rows of one frame, before layout: shared by the painter and the scroll bound,
/// so the computed body height is the painted one (height == paint).
struct Parts {
    /// The kind's pinned header (a permission's tool and source).
    header: Vec<Line<'static>>,
    /// The scrollable body, already cut at the scroll offset.
    body: Vec<Line<'static>>,
    /// The open field's row, pinned above the rail.
    field: Option<Line<'static>>,
    /// The key rail.
    rail: Vec<Line<'static>>,
}

/// The consent card text without its own key line: the frame generates that rail
/// from the registry, and the daemon's copy is built from the same rows.
fn consent_text(payload: &ConsentPayload) -> String {
    let rail_line = format!(
        "{} Allow once  {} Always allow  {} Decline",
        keymap::render::rail(Context::Consent, CardCmd::Yes),
        keymap::render::rail(Context::Consent, CardCmd::Always),
        keymap::render::rail(Context::Consent, CardCmd::No),
    );
    payload
        .input_preview
        .lines()
        .filter(|line| *line != rail_line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text a plain kind's body wraps: the stale copy once the source settled
/// elsewhere, else the sanitized source text.
fn body_text(item: &DecisionItem) -> String {
    match (&item.validity, &item.subject) {
        (Validity::Stale(copy), _) => copy.0.to_owned(),
        (Validity::Live, DecisionSubject::Consent(payload)) => {
            sanitize_tool_text_block(&consent_text(payload)).into_owned()
        }
        (Validity::Live, _) => sanitize_tool_text_block(&item.detail).into_owned(),
    }
}

fn card_of(payload: &PermissionPayload) -> PermissionCard<'_> {
    PermissionCard {
        source: &payload.source,
        tool_name: &payload.tool_name,
        risk: payload.risk,
        input: match &payload.input {
            PermissionInput::Ready(value) => ExactInput::Ready(value),
            PermissionInput::Receiving { summary } => ExactInput::Receiving { summary },
        },
    }
}
/// Prepared once per request, input version, width and body colours.
#[derive(Debug)]
pub(crate) struct PreparedPermission {
    id: crate::domain::models::tool_call::RequestId,
    input: PermissionInput,
    width: usize,
    colors: [Color; 5],
    card: FrameRows,
    review: FrameRows,
}

fn prepared_permission(
    state: &TuiState,
    payload: &PermissionPayload,
    width: usize,
    theme: &Theme,
) -> Arc<PreparedPermission> {
    let colors = [
        theme.colors.accent,
        theme.colors.fg_secondary,
        theme.colors.fg_primary,
        theme.colors.tool_name,
        theme.colors.subagent_attribution,
    ];
    let mut cache = state.decisions.permission_layout.borrow_mut();
    if let Some(prepared) = cache.as_ref() {
        let same_input = match (&prepared.input, &payload.input) {
            (PermissionInput::Ready(a), PermissionInput::Ready(b)) => Arc::ptr_eq(a, b),
            (
                PermissionInput::Receiving { summary: a },
                PermissionInput::Receiving { summary: b },
            ) => a == b,
            _ => false,
        };
        if prepared.id == payload.request_id
            && prepared.width == width
            && prepared.colors == colors
            && same_input
        {
            return Arc::clone(prepared);
        }
    }
    let card = card_of(payload);
    let prepared = Arc::new(PreparedPermission {
        id: payload.request_id.clone(),
        input: payload.input.clone(),
        width,
        colors,
        card: card_rows(&card, width as u16, theme, true),
        review: review_rows(&card, width as u16, theme, true),
    });
    *cache = Some(Arc::clone(&prepared));
    prepared
}

/// The wrapped body of one item: prepared once per item, width, text and colours for the
/// kinds whose text is large and model- or file-originated.
#[derive(Debug)]
pub(crate) struct PreparedBody {
    key: DecisionKey,
    width: usize,
    /// Identity of the text the rows were wrapped from (address, length, mode).
    signature: (usize, usize, usize),
    colors: [Color; 5],
    rows: Vec<Line<'static>>,
}

fn body_colors(theme: &Theme) -> [Color; 5] {
    [
        theme.colors.accent,
        theme.colors.fg_primary,
        theme.colors.fg_secondary,
        theme.colors.fg_muted,
        theme.colors.tool_status_error,
    ]
}

/// True for the kinds whose body is cached on the queue.
fn is_heavy(item: &DecisionItem) -> bool {
    matches!(
        item.subject,
        DecisionSubject::Question(_)
            | DecisionSubject::SkillTrust(_)
            | DecisionSubject::PlanApproval(_)
            | DecisionSubject::PlanCard(_)
    )
}

/// Identity of the text a heavy body is wrapped from. A payload that is replaced in place
/// clears the cache explicitly (`raise`, `open_inspect`, `set_plan_contents`).
fn signature(item: &DecisionItem, state: &TuiState) -> (usize, usize, usize) {
    let of = |text: &str, mode: usize| (text.as_ptr() as usize, text.len(), mode);
    match &item.subject {
        DecisionSubject::Question(payload) => of(&payload.question, 0),
        DecisionSubject::SkillTrust(payload) => {
            match (&payload.inspect_content, payload.inspecting) {
                (Some(content), true) => of(content, 1),
                _ => of(
                    &payload.skill_name,
                    2 + (state.decisions.count(DecisionKind::SkillTrust) << 2),
                ),
            }
        }
        DecisionSubject::PlanApproval(payload) => of(&payload.contents, 3),
        DecisionSubject::PlanCard(payload) => of(&payload.plan_snapshot.title, 4),
        _ => (0, 0, 0),
    }
}

fn clean(text: &str) -> String {
    sanitize_tool_text_block(text).into_owned()
}

fn clean_line(text: &str) -> String {
    sanitize_display_line(text)
}

/// A plan with every model-written string sanitized for paint.
fn sanitized_plan(plan: &Plan) -> Plan {
    let mut plan = plan.clone();
    plan.title = clean_line(&plan.title);
    for task in &mut plan.tasks {
        task.title = clean_line(&task.title);
        task.description = clean(&task.description);
    }
    plan
}

/// Wrap `lines` to `width` cells, word-aware, keeping each span's style. A line that fits
/// is kept as it is; a word wider than the frame is cut on grapheme boundaries.
pub(crate) fn wrap_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let cells: usize = line.spans.iter().map(|s| geom::cells(&s.content)).sum();
        if cells <= width {
            out.push(line);
        } else {
            wrap_line(&line, width, &mut out);
        }
    }
    out
}

fn wrap_line(line: &Line<'static>, width: usize, out: &mut Vec<Line<'static>>) {
    // One entry per grapheme: (span index, byte range in that span, cells).
    struct Cell {
        span: usize,
        start: usize,
        end: usize,
        cells: usize,
        space: bool,
    }
    let mut graphemes: Vec<Cell> = Vec::new();
    for (span, s) in line.spans.iter().enumerate() {
        for (start, g) in s.content.grapheme_indices(true) {
            graphemes.push(Cell {
                span,
                start,
                end: start + g.len(),
                cells: geom::cells(g),
                space: g == " ",
            });
        }
    }
    let emit = |row: &[Cell], out: &mut Vec<Line<'static>>| {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut run: Option<(usize, usize, usize)> = None; // (span, start, end)
        for cell in row {
            match run {
                Some((span, start, end)) if span == cell.span && end == cell.start => {
                    run = Some((span, start, cell.end));
                }
                Some((span, start, end)) => {
                    spans.push(Span::styled(
                        line.spans[span].content[start..end].to_owned(),
                        line.spans[span].style,
                    ));
                    run = Some((cell.span, cell.start, cell.end));
                }
                None => run = Some((cell.span, cell.start, cell.end)),
            }
        }
        if let Some((span, start, end)) = run {
            spans.push(Span::styled(
                line.spans[span].content[start..end].to_owned(),
                line.spans[span].style,
            ));
        }
        out.push(Line::from(spans));
    };
    let mut row: Vec<Cell> = Vec::new();
    let mut used = 0usize;
    for g in graphemes {
        if used > 0 && used + g.cells > width {
            // Break at the last space when the row has one, else mid-word.
            match row.iter().rposition(|c| c.space) {
                Some(at) if at > 0 => {
                    let carry: Vec<Cell> = row.split_off(at + 1);
                    row.pop(); // the breaking space itself
                    emit(&row, out);
                    row = carry;
                    used = row.iter().map(|c| c.cells).sum();
                }
                _ => {
                    emit(&row, out);
                    row.clear();
                    used = 0;
                }
            }
        }
        used += g.cells;
        row.push(g);
    }
    if !row.is_empty() {
        emit(&row, out);
    }
}

/// The unscrolled body of a live, non-permission item at `width`.
fn build_body(
    item: &DecisionItem,
    state: &TuiState,
    theme: &Theme,
    width: usize,
) -> Vec<Line<'static>> {
    let normal = Style::default().fg(theme.colors.fg_primary);
    match &item.subject {
        DecisionSubject::Question(payload) => {
            ask_user_question::question_lines(&clean(&payload.question), width, theme)
        }
        DecisionSubject::SkillTrust(payload) => {
            match (&payload.inspect_content, payload.inspecting) {
                (Some(content), true) => skill_trust_prompt::inspect_lines(
                    &clean_line(&payload.skill_name),
                    &clean(content),
                    width,
                    theme,
                ),
                _ => wrap_lines(
                    skill_trust_prompt::trust_lines(
                        &clean_line(&payload.skill_name),
                        state
                            .decisions
                            .count(DecisionKind::SkillTrust)
                            .saturating_sub(1),
                        theme,
                    ),
                    width,
                ),
            }
        }
        DecisionSubject::PlanApproval(payload) => {
            let path = std::path::PathBuf::from(clean_line(&payload.plan_path.to_string_lossy()));
            wrap_lines(
                plan_approval::lines(
                    &path,
                    &clean(&payload.summary),
                    &clean(&payload.contents),
                    width,
                    theme,
                ),
                width,
            )
        }
        DecisionSubject::PlanCard(payload) => wrap_lines(
            plan_approval::card_lines(&sanitized_plan(&payload.plan_snapshot), theme),
            width,
        ),
        DecisionSubject::PlanDeviation(payload) => wrap_lines(
            plan_deviation_card::lines(
                payload.original,
                payload.original,
                &payload.changed,
                &clean(&payload.summary),
            ),
            width,
        ),
        DecisionSubject::SkipCascade(payload) => {
            wrap_lines(task_skip_cascade_card::lines(&payload.0), width)
        }
        DecisionSubject::CancelPlan(payload) => wrap_lines(
            cancel_plan_confirm_card::lines(
                &clean_line(&payload.title),
                payload.pending,
                payload.completed,
            ),
            width,
        ),
        DecisionSubject::Delegation(card) => {
            let mut card: PendingDelegationCard = (**card).clone();
            card.suggestion.agent_name = clean_line(&card.suggestion.agent_name);
            wrap_lines(delegation_card::lines(&card, theme), width)
        }
        DecisionSubject::SpawnGate(gate) => wrap_lines(
            gate_lines(&SpawnGateSnapshot {
                requested: gate.requested,
                threshold: gate.threshold,
                adjusted: gate.adjusted,
            }),
            width,
        ),
        DecisionSubject::AgentKill(payload) => wrap_lines(
            agent_inspector::kill_lines(&clean_line(&payload.label), theme),
            width,
        ),
        _ => wrapped(&body_text(item), normal, width),
    }
}

/// The whole body of a live, non-permission item, from the queue's cache when it is heavy.
fn body_all(
    item: &DecisionItem,
    state: &TuiState,
    theme: &Theme,
    width: usize,
) -> Arc<PreparedBody> {
    let key = item.key();
    let colors = body_colors(theme);
    let signature = signature(item, state);
    let heavy = is_heavy(item);
    if heavy
        && let Some(hit) = state.decisions.body_layout.borrow().as_ref()
        && hit.key == key
        && hit.width == width
        && hit.colors == colors
        && hit.signature == signature
    {
        return Arc::clone(hit);
    }
    let prepared = Arc::new(PreparedBody {
        key,
        width,
        signature,
        colors,
        rows: build_body(item, state, theme, width),
    });
    if heavy {
        *state.decisions.body_layout.borrow_mut() = Some(Arc::clone(&prepared));
    }
    prepared
}

/// The open text field as one row: `<label><text>` with the caret while it takes keys.
fn field_line(
    label: &'static str,
    field: &TextField,
    caret: bool,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let view = field.view(width.saturating_sub(geom::cells(label)));
    let normal = Style::default().fg(theme.colors.fg_primary);
    let mut spans = vec![
        Span::styled(label, Style::default().fg(theme.colors.fg_secondary)),
        Span::styled(view.before.to_owned(), normal),
    ];
    if caret {
        let shown = if view.caret == " " { "▋" } else { view.caret };
        spans.push(if shown == "▋" {
            Span::styled("▋", normal)
        } else {
            Span::styled(shown.to_owned(), normal.add_modifier(Modifier::REVERSED))
        });
    } else if view.caret != " " {
        spans.push(Span::styled(view.caret.to_owned(), normal));
    }
    spans.push(Span::styled(view.after.to_owned(), normal));
    Line::from(spans)
}

/// The open Deny + feedback field as one row: `Feedback: <text>` with the caret.
fn field_row(payload: &PermissionPayload, width: usize, theme: &Theme) -> Option<Line<'static>> {
    Some(field_line(
        "Feedback: ",
        payload.feedback.as_ref()?,
        true,
        width,
        theme,
    ))
}

/// The question's answer as one row: `Answer: <text>`, with the caret while it is open.
fn answer_row(payload: &QuestionPayload, width: usize, theme: &Theme) -> Line<'static> {
    field_line("Answer: ", &payload.field, payload.editing, width, theme)
}

fn parts(
    item: &DecisionItem,
    state: &TuiState,
    theme: &Theme,
    width: usize,
    visible: usize,
) -> Parts {
    let queue = &state.decisions;
    let live = !queue.inert(state);
    let normal = Style::default().fg(theme.colors.fg_primary);
    let muted = Style::default().fg(theme.colors.fg_muted);
    let accent = Style::default().fg(theme.colors.accent);
    // Keys are inert for 500 ms after the first paint: they paint muted until then.
    let key_style = if live { accent } else { muted };

    if let (Validity::Live, DecisionSubject::Permission(payload)) = (&item.validity, &item.subject)
    {
        let prepared = prepared_permission(state, payload, width, theme);
        let cached = if payload.reviewing {
            &prepared.review
        } else {
            &prepared.card
        };
        // Construct only the small rail afresh, so its inert colours remain current.
        let card = PermissionCard {
            input: ExactInput::Receiving { summary: "" },
            ..card_of(payload)
        };
        let rail_rows = if payload.reviewing {
            review_rows(&card, width as u16, theme, live)
        } else {
            card_rows(&card, width as u16, theme, live)
        };
        let first = queue.scroll().min(cached.body.len().saturating_sub(1));
        let field = field_row(payload, width, theme);
        let rail = if field.is_some() {
            let compose = |command: CardCmd, label: &str| -> Vec<Span<'static>> {
                vec![
                    Span::styled(
                        keymap::render::rail(Context::DecisionCompose, command),
                        key_style,
                    ),
                    Span::styled(format!(" {label}  "), normal),
                ]
            };
            let mut spans = compose(CardCmd::Send, "Send");
            spans.extend(compose(CardCmd::Back, "Back"));
            footer_lines(spans, width)
        } else {
            rail_rows.rail
        };
        return Parts {
            header: cached.header.clone(),
            body: cached
                .body
                .iter()
                .skip(first)
                .take(visible)
                .cloned()
                .collect(),
            field,
            rail,
        };
    }

    let mut rail: Vec<Span<'static>> = Vec::new();
    if matches!(item.validity, Validity::Stale(_)) {
        rail.push(Span::styled(
            keymap::render::rail(Context::Decision, OverlayCmd::PaneEsc),
            key_style,
        ));
        rail.push(Span::styled(" Close  ", normal));
    } else {
        for (key, label) in rail_of(item) {
            rail.push(Span::styled(key, key_style));
            rail.push(Span::styled(format!(" {label}  "), normal));
        }
    }
    let stale_or_notice = matches!(item.validity, Validity::Stale(_)) || queue.feedback().is_some();
    let body_rows: Vec<Line<'static>> = if stale_or_notice {
        let text = queue
            .feedback()
            .map_or_else(|| body_text(item), str::to_owned);
        slice_rows(&wrapped(&text, normal, width), queue.scroll(), visible)
    } else {
        let all = body_all(item, state, theme, width);
        slice_rows(&all.rows, queue.scroll(), visible)
    };
    let field = match (&item.validity, &item.subject) {
        (Validity::Live, DecisionSubject::Question(payload)) => {
            Some(answer_row(payload, width, theme))
        }
        _ => None,
    };
    Parts {
        header: Vec::new(),
        body: body_rows,
        field,
        rail: footer_lines(rail, width),
    }
}

/// The rows from the scroll offset on, at most `visible` of them (only those are cloned).
fn slice_rows(rows: &[Line<'static>], scroll: usize, visible: usize) -> Vec<Line<'static>> {
    let first = scroll.min(rows.len().saturating_sub(1));
    rows.iter().skip(first).take(visible).cloned().collect()
}

/// How many rows the body wraps to at the terminal's width (the scroll bound).
#[must_use]
pub(crate) fn body_rows(item: &DecisionItem, state: &TuiState) -> usize {
    let width = inner_width(state.terminal_width);
    let theme = &state.theme;
    match (&item.validity, &item.subject) {
        (Validity::Live, DecisionSubject::Permission(payload)) => {
            let prepared = prepared_permission(state, payload, width, theme);
            if payload.reviewing {
                prepared.review.body.len()
            } else {
                prepared.card.body.len()
            }
        }
        (Validity::Stale(_), _) => wrapped(&body_text(item), Style::default(), width).len(),
        _ => body_all(item, state, theme, width).rows.len(),
    }
}

/// The breadcrumb's source: the item's own summary, else its source identity.
fn crumb_source(item: &DecisionItem) -> &str {
    if item.summary.is_empty() {
        &item.source
    } else {
        &item.summary
    }
}

/// Paint the focused item into `area`. False when nothing was painted (no
/// decidable focused item, or the area is too small to hold a frame): the host
/// only starts the 500 ms inert clock from a frame the user could really see.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    state: &TuiState,
    facts: &LogFacts,
    theme: &Theme,
    title: &str,
    interrupted: Option<&str>,
) -> bool {
    let queue = &state.decisions;
    let Some(item) = decision_queue::current(state, facts) else {
        return false;
    };
    if area.width < 12 || area.height < 5 {
        return false;
    }
    let inner_width = usize::from(area.width.saturating_sub(2));
    let muted = Style::default().fg(theme.colors.fg_muted);
    let accent = Style::default().fg(theme.colors.accent);

    let mut crumb = format!("Needs you › {}", item.kind.title());
    let source = crumb_source(item);
    if !source.is_empty() {
        crumb.push(' ');
        crumb.push_str(&sanitize_tool_text_block(source));
    }
    crumb.push_str(&format!(" | {} pending", queue.len()));
    let mut crumbs = wrapped(&crumb, muted, inner_width);
    if let Some(interrupted) = interrupted {
        crumbs.extend(wrapped(
            &format!("Interrupted: {}", sanitize_tool_text_block(interrupted)),
            muted,
            inner_width,
        ));
    }

    let Parts {
        header,
        body,
        field,
        mut rail,
    } = parts(item, state, theme, inner_width, usize::from(area.height));
    if let Some(field) = field {
        rail.insert(0, field);
    }

    let passive = queue
        .items()
        .filter(|other| !std::ptr::eq(*other, item) && !other.actionable(facts))
        .count();
    // The breadcrumb yields before the body or the rail does: a 60x16 terminal with a
    // full composer leaves a five-row frame (title, one body row, the rail).
    let mut layout = vec![ModalPart::Notice(crumbs)];
    if passive > 0 {
        layout.push(ModalPart::Notice(vec![Line::from(Span::styled(
            format!("{passive} in another tab: open it to decide"),
            muted,
        ))]));
    }
    if !header.is_empty() {
        layout.push(ModalPart::Notice(header));
    }
    layout.push(ModalPart::Rows {
        lines: body,
        selected: (0, 0),
    });
    layout.push(ModalPart::Footer(rail));

    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(accent)
        .title(format!(" {title} "))
        .style(Style::default().bg(theme.colors.bg_surface));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let lines = fit_modal(layout, usize::from(inner.height), usize::from(inner.width));
    frame.render_widget(
        Paragraph::new(lines).style(
            Style::default()
                .fg(theme.colors.fg_primary)
                .bg(theme.colors.bg_surface),
        ),
        inner,
    );
    true
}

/// What `c` copies: the item's original text, or nothing when it is above the copy limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CopySource {
    Text(String),
    TooLarge,
}

/// An upper bound of the bytes of a JSON value's text, stopping once `limit` is passed
/// (a huge exact input is refused without building its rows).
fn json_bytes(value: &serde_json::Value, limit: usize) -> usize {
    let mut total = 0usize;
    let mut stack = vec![value];
    while let Some(value) = stack.pop() {
        total = total.saturating_add(match value {
            serde_json::Value::String(text) => text.len(),
            serde_json::Value::Array(items) => {
                stack.extend(items);
                2
            }
            serde_json::Value::Object(map) => {
                for (key, inner) in map {
                    total = total.saturating_add(key.len());
                    stack.push(inner);
                }
                2
            }
            _ => 8,
        });
        if total > limit {
            return total;
        }
    }
    total
}

/// The bytes of the big strings an item carries (what a copy would have to build).
fn source_len(item: &DecisionItem, limit: usize) -> usize {
    match &item.subject {
        DecisionSubject::Question(payload) => payload.question.len(),
        DecisionSubject::SkillTrust(payload) => payload
            .inspect_content
            .as_ref()
            .filter(|_| payload.inspecting)
            .map_or(payload.skill_name.len(), String::len),
        // `source_text` copies the contents alone, the summary only when there are none.
        DecisionSubject::PlanApproval(payload) if payload.contents.is_empty() => {
            payload.summary.len()
        }
        DecisionSubject::PlanApproval(payload) => payload.contents.len(),
        DecisionSubject::PlanCard(payload) => payload.plan_snapshot.tasks.iter().fold(
            payload.plan_snapshot.title.len(),
            |total, task| {
                total
                    .saturating_add(task.title.len())
                    .saturating_add(task.description.len())
            },
        ),
        DecisionSubject::PlanDeviation(payload) => payload.summary.len(),
        DecisionSubject::Permission(payload) => match &payload.input {
            PermissionInput::Ready(value) => json_bytes(value, limit),
            PermissionInput::Receiving { summary } => summary.len(),
        },
        DecisionSubject::Consent(payload) => payload.input_preview.len(),
        _ => item.detail.len(),
    }
}

fn plain(lines: &[Line<'_>]) -> String {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The item's original body text (never sanitized, never wrapped).
fn source_text(item: &DecisionItem, theme: &Theme) -> String {
    match &item.subject {
        DecisionSubject::Permission(payload) => match &payload.input {
            PermissionInput::Ready(value) => {
                approval_presentation::body_lines(&payload.tool_name, value)
                    .iter()
                    .map(approval_presentation::PresentedLine::joined)
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            PermissionInput::Receiving { summary } => summary.clone(),
        },
        DecisionSubject::Consent(payload) => consent_text(payload),
        DecisionSubject::Question(payload) => payload.question.clone(),
        DecisionSubject::SkillTrust(payload) => match &payload.inspect_content {
            Some(content) if payload.inspecting => content.clone(),
            _ => format!("{}\n{}", payload.skill_name, payload.skill_file.display()),
        },
        DecisionSubject::PlanApproval(payload) if payload.contents.is_empty() => {
            payload.summary.clone()
        }
        DecisionSubject::PlanApproval(payload) => payload.contents.clone(),
        DecisionSubject::PlanCard(payload) => {
            plain(&plan_approval::card_lines(&payload.plan_snapshot, theme))
        }
        DecisionSubject::PlanDeviation(payload) => plain(&plan_deviation_card::lines(
            payload.original,
            payload.original,
            &payload.changed,
            &payload.summary,
        )),
        DecisionSubject::SkipCascade(payload) => plain(&task_skip_cascade_card::lines(&payload.0)),
        DecisionSubject::CancelPlan(payload) => plain(&cancel_plan_confirm_card::lines(
            &payload.title,
            payload.pending,
            payload.completed,
        )),
        DecisionSubject::Delegation(card) => plain(&delegation_card::lines(card, theme)),
        DecisionSubject::SpawnGate(gate) => plain(&gate_lines(&SpawnGateSnapshot {
            requested: gate.requested,
            threshold: gate.threshold,
            adjusted: gate.adjusted,
        })),
        DecisionSubject::AgentKill(payload) => {
            plain(&agent_inspector::kill_lines(&payload.label, theme))
        }
        DecisionSubject::Text | DecisionSubject::LargeImage(_) | DecisionSubject::Delete(_) => {
            item.detail.clone()
        }
    }
}

/// `c` on a decision frame: the item's original text, all or nothing at the copy limit
/// (nothing is built above it).
#[must_use]
pub(crate) fn copy_source(item: &DecisionItem, state: &TuiState) -> CopySource {
    if source_len(item, COPY_MAX_BYTES) > COPY_MAX_BYTES {
        return CopySource::TooLarge;
    }
    let text = source_text(item, &state.theme);
    if text.len() > COPY_MAX_BYTES {
        CopySource::TooLarge
    } else {
        CopySource::Text(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::ToolRisk;
    use crate::domain::models::tool_call::{ApprovalSource, RequestId};

    #[test]
    fn story21_6_permission_layout_reuses_and_invalidates_by_input_and_width() {
        let state = TuiState::new(80, 24);
        let mut payload = PermissionPayload::new(
            RequestId::new(),
            ApprovalSource::ForegroundTurn {
                conversation_id: "c1".into(),
            },
            "Bash".into(),
            ToolRisk::Elevated,
            PermissionInput::Receiving {
                summary: "receiving".into(),
            },
        );
        let first = prepared_permission(&state, &payload, 78, &state.theme);
        assert!(Arc::ptr_eq(
            &first,
            &prepared_permission(&state, &payload, 78, &state.theme)
        ));
        payload.input = PermissionInput::Ready(Arc::new(serde_json::json!({"command": "ready"})));
        let ready = prepared_permission(&state, &payload, 78, &state.theme);
        assert!(!Arc::ptr_eq(&first, &ready));
        assert!(
            ready
                .review
                .body
                .iter()
                .any(|line| line.to_string().contains("command: ready"))
        );
        assert!(Arc::ptr_eq(
            &ready,
            &prepared_permission(&state, &payload, 78, &state.theme)
        ));
        let resized = prepared_permission(&state, &payload, 58, &state.theme);
        assert!(!Arc::ptr_eq(&ready, &resized));
    }

    #[test]
    fn story21_6_completed_input_replaces_receiving_cache() {
        use crate::adapters::tui::decision_queue::{self, DecisionSubject, Responder};
        let mut state = TuiState::new(80, 24);
        let id = RequestId::new();
        let payload = PermissionPayload::new(
            id.clone(),
            ApprovalSource::ForegroundTurn {
                conversation_id: "c1".into(),
            },
            "Bash".into(),
            ToolRisk::Elevated,
            PermissionInput::Receiving {
                summary: "pending".into(),
            },
        );
        decision_queue::raise_permission(&mut state, payload, Responder::Local);
        let DecisionSubject::Permission(payload) = &state.decisions.items().next().unwrap().subject
        else {
            panic!("permission")
        };
        let receiving = prepared_permission(&state, payload, 78, &state.theme);
        assert!(decision_queue::complete_input(
            &mut state,
            &id,
            serde_json::json!({"command": "complete"})
        ));
        let DecisionSubject::Permission(payload) = &state.decisions.items().next().unwrap().subject
        else {
            panic!("permission")
        };
        let ready = prepared_permission(&state, payload, 78, &state.theme);
        assert!(!Arc::ptr_eq(&receiving, &ready));
        assert!(
            ready
                .review
                .body
                .iter()
                .any(|line| line.to_string().contains("command: complete"))
        );
    }

    #[test]
    fn story21_6_scroll_bound_reuses_prepared_review() {
        use crate::adapters::tui::decision_queue::{self, Responder};
        let mut state = TuiState::new(80, 24);
        let mut payload = PermissionPayload::new(
            RequestId::new(),
            ApprovalSource::ForegroundTurn {
                conversation_id: "c1".into(),
            },
            "Bash".into(),
            ToolRisk::Elevated,
            PermissionInput::Ready(Arc::new(
                serde_json::json!({"command": "one\ntwo\nthree\nfour\nfive"}),
            )),
        );
        payload.reviewing = true;
        decision_queue::raise_permission(&mut state, payload, Responder::Local);
        let item = state.decisions.items().next().unwrap();
        let rows = body_rows(item, &state);
        let first = state
            .decisions
            .permission_layout
            .borrow()
            .as_ref()
            .unwrap()
            .clone();
        assert_eq!(body_rows(item, &state), rows);
        let second = state
            .decisions
            .permission_layout
            .borrow()
            .as_ref()
            .unwrap()
            .clone();
        assert!(Arc::ptr_eq(&first, &second));
    }

    fn text_of(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn story21_6_wrapping_keeps_words_whole_and_each_spans_style() {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let line = Line::from(vec![
            Span::raw("New skill: "),
            Span::styled("\"review-code\"", bold),
        ]);
        let rows = wrap_lines(vec![line.clone()], 14);
        assert_eq!(text_of(&rows), ["New skill:", "\"review-code\""]);
        assert_eq!(
            rows[1].spans[0].style, bold,
            "the styled word keeps its style"
        );
        assert_eq!(
            wrap_lines(vec![line.clone()], 80),
            vec![line],
            "a fitting line is kept"
        );
        let long = wrap_lines(vec![Line::from("x".repeat(25))], 10);
        assert_eq!(text_of(&long).concat(), "x".repeat(25));
        assert!(text_of(&long).iter().all(|row| row.chars().count() <= 10));
    }

    #[test]
    fn story21_6_the_json_bound_stops_early() {
        let big = serde_json::json!({"a": "x".repeat(100), "b": ["y".repeat(100)]});
        assert!(json_bytes(&big, 50) > 50);
        assert!(json_bytes(&serde_json::json!({"a": "x"}), 50) < 50);
    }

    fn paint(state: &TuiState, width: u16, height: u16) -> String {
        use ratatui::{Terminal, backend::TestBackend};
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let theme = state.theme.clone();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, width, height.saturating_sub(4));
                render(
                    frame,
                    area,
                    state,
                    &LogFacts::default(),
                    &theme,
                    "Decision: Question",
                    None,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn story21_6_a_long_question_keeps_a_body_row_its_answer_field_and_the_rail_at_60x16() {
        let mut state = TuiState::new(60, 16);
        let (tx, _rx) = tokio::sync::oneshot::channel();
        decision_queue::raise_question(&mut state, "c", "tu-1".into(), "word ".repeat(200), tx);
        for (width, height) in [(60u16, 16u16), (80, 24), (130, 30)] {
            state.terminal_width = width;
            let text = paint(&state, width, height);
            assert!(
                text.contains("word"),
                "{width}x{height}: a body row\n{text}"
            );
            assert!(
                text.contains("Answer: ▋"),
                "{width}x{height}: the field\n{text}"
            );
            for pair in ["[Enter] Send", "[Ctrl+D] Skip question", "[Esc] Back"] {
                assert!(text.contains(pair), "{width}x{height}: {pair}\n{text}");
            }
        }
    }

    #[test]
    fn story21_6_a_heavy_body_is_wrapped_once_per_item_and_width() {
        let mut state = TuiState::new(80, 24);
        let (tx, _rx) = tokio::sync::oneshot::channel();
        decision_queue::raise_question(&mut state, "c", "tu-1".into(), "word ".repeat(400), tx);
        let item = state.decisions.items().next().unwrap();
        let rows = body_rows(item, &state);
        let first = state
            .decisions
            .body_layout
            .borrow()
            .as_ref()
            .unwrap()
            .clone();
        assert_eq!(body_rows(item, &state), rows);
        assert!(Arc::ptr_eq(
            &first,
            state.decisions.body_layout.borrow().as_ref().unwrap()
        ));
        state.terminal_width = 40;
        let item = state.decisions.items().next().unwrap();
        assert!(
            body_rows(item, &state) > rows,
            "a narrower frame wraps to more rows"
        );
        assert!(!Arc::ptr_eq(
            &first,
            state.decisions.body_layout.borrow().as_ref().unwrap()
        ));
    }

    /// ESC/CSI, OSC 8, BEL, C1 (0x85, 0x9b) and a bidi override.
    const HOSTILE: &str = "x\u{1b}[2Jy\u{1b}]8;;http://evil\u{7}z\u{85}w\u{202e}v\u{9b}u";
    /// What `HOSTILE` must become, spelled out independently of the sanitizer: whole
    /// CSI/OSC sequences, the C1 controls (0x9b swallows its final byte) and the bidi override go.
    const CLEANED: &str = "xyzwv";

    fn has_control(rows: &[String]) -> bool {
        rows.iter().any(|row| {
            row.chars().any(|c| {
                c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            })
        })
    }

    /// The prepared rows (`build_body`, the Line spans BEFORE ratatui's cell buffer
    /// would strip any control) of the one item `raise` queues, at a width nothing wraps at.
    fn prepared(raise: impl FnOnce(&mut TuiState)) -> Vec<String> {
        let mut state = TuiState::new(200, 40);
        raise(&mut state);
        let item = state.decisions.items().next().expect("queued");
        text_of(&build_body(item, &state, &state.theme, 198))
    }

    #[test]
    fn story21_6_model_text_is_sanitized_in_the_prepared_rows_before_any_cell_is_painted() {
        use crate::adapters::tui::decision_queue::{
            CancelPlanPayload, PlanDeviationPayload, raise_agent_kill, raise_cancel_plan,
            raise_delegation, raise_model_skill_trust, raise_plan_deviation, raise_question,
        };
        use crate::domain::models::AgentId;
        use crate::domain::models::plan::PlanDeviationKind;
        use crate::domain::services::delegation_decider::{DelegationReason, DelegationSuggestion};

        let question = prepared(|state| {
            let (tx, _rx) = tokio::sync::oneshot::channel();
            raise_question(state, "c1", "tu".into(), HOSTILE.into(), tx);
        });
        assert_eq!(question, [CLEANED]);
        let skill = prepared(|state| {
            let (tx, _rx) = tokio::sync::oneshot::channel();
            raise_model_skill_trust(state, HOSTILE.into(), "/w/SKILL.md".into(), tx);
        });
        assert_eq!(
            skill[0],
            format!("New project skill detected: \"{CLEANED}\"")
        );
        let deviation = prepared(|state| {
            raise_plan_deviation(
                state,
                "c1",
                PlanDeviationPayload {
                    plan_id: "p1".into(),
                    kind: PlanDeviationKind::AutoSkipBlockedTasks { source_task: 1 },
                    original: 3,
                    changed: vec![2, 3],
                    summary: HOSTILE.into(),
                },
            );
        });
        assert_eq!(
            deviation,
            [
                "Plan deviation — reapproval required",
                "3 tasks → 3 tasks. Skipped: 2, 3",
                CLEANED
            ]
        );
        let cancel = prepared(|state| {
            raise_cancel_plan(
                state,
                "c1",
                CancelPlanPayload {
                    plan_id: "p1".into(),
                    title: HOSTILE.into(),
                    pending: 2,
                    completed: 1,
                },
            );
        });
        assert_eq!(
            cancel,
            [CLEANED, "2 task(s) remaining, 1 completed (will be kept)"]
        );
        let delegation = prepared(|state| {
            raise_delegation(
                state,
                PendingDelegationCard {
                    conversation_id: "c1".into(),
                    plan_id: "p1".into(),
                    task_number: 2,
                    suggestion: DelegationSuggestion {
                        task_number: 2,
                        agent_name: HOSTILE.into(),
                        reason: DelegationReason::Heuristic,
                        auto_proceed: false,
                    },
                },
            );
        });
        assert_eq!(
            delegation,
            [
                "Delegate Task 2?".to_owned(),
                format!("→ {CLEANED}  (heuristic)")
            ]
        );
        let kill = prepared(|state| {
            raise_agent_kill(state, "c1", AgentId::new(), HOSTILE.into());
        });
        assert_eq!(kill, [format!("\u{26A0} Kill {CLEANED} and its subtree?")]);
        for (name, rows) in [
            ("question", &question),
            ("skill", &skill),
            ("deviation", &deviation),
            ("cancel", &cancel),
            ("delegation", &delegation),
            ("kill", &kill),
        ] {
            assert!(!has_control(rows), "{name}: a control survived {rows:?}");
        }
    }

    /// The sanitation control: the same text through the same builders WITHOUT the
    /// sanitizer keeps its controls in the spans, so the checks above can fail.
    #[test]
    fn story21_6_an_unsanitized_builder_keeps_the_controls_the_checks_look_for() {
        let theme = Theme::dark();
        let raw = text_of(&ask_user_question::question_lines(HOSTILE, 198, &theme));
        assert!(has_control(&raw), "{raw:?}");
        assert_ne!(raw, [CLEANED]);
        assert!(has_control(&text_of(&wrap_lines(
            vec![Line::from(HOSTILE)],
            198
        ))));
        assert!(!has_control(&[CLEANED.to_owned(), "plain".to_owned()]));
        // And the sanitizer itself, by value.
        assert_eq!(clean(HOSTILE), CLEANED);
        assert_eq!(
            clean_line(&format!(" {HOSTILE}\n\t{HOSTILE} ")),
            format!("{CLEANED} {CLEANED}")
        );
    }
}
