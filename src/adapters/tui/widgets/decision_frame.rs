//! The decision frame body (Story 21.6 A4): one L3 `Surface::Decision` painted in
//! 21-4's `frame_rect`, never bottom-anchored over the Stage.
//!
//! Rows, top to bottom: the breadcrumb `Needs you › <kind> <source> | <N> pending` (and,
//! when the frame interrupted another, `Interrupted: <breadcrumb>`), the kind's
//! pinned header, its delimited body, and the rail generated from the registry. The
//! body shrinks first; the rail is never dropped. Untrusted text is sanitized on paint
//! (the effect and any copy keep the original bytes).

use std::sync::Arc;

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::adapters::tui::decision_queue::{
    self, ConsentPayload, DecisionItem, DecisionSubject, PermissionInput, PermissionPayload,
    Validity,
};
use crate::adapters::tui::keymap::{self, Context, table_card::CardCmd, table_overlay::OverlayCmd};
use crate::adapters::tui::notification_log::{LogFacts, footer_lines};
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::modal::wrapped;
use crate::adapters::tui::widgets::permission_prompt::{
    ExactInput, FrameRows, PermissionCard, card_rows, review_rows,
};
use crate::adapters::tui::widgets::{ModalPart, fit_modal};
use crate::domain::services::peer_text::sanitize_tool_text_block;

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
    /// The scrollable body.
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

/// The open Deny + feedback field as one row: `Feedback: <text>` with the caret.
fn field_row(payload: &PermissionPayload, width: usize, theme: &Theme) -> Option<Line<'static>> {
    const LABEL: &str = "Feedback: ";
    let field = payload.feedback.as_ref()?;
    let view = field.view(width.saturating_sub(LABEL.len()));
    let caret = if view.caret == " " { "▋" } else { view.caret };
    let normal = Style::default().fg(theme.colors.fg_primary);
    let mut spans = vec![
        Span::styled(LABEL, Style::default().fg(theme.colors.fg_secondary)),
        Span::styled(view.before.to_owned(), normal),
    ];
    spans.push(if caret == "▋" {
        Span::styled("▋", normal)
    } else {
        Span::styled(caret.to_owned(), normal.add_modifier(Modifier::REVERSED))
    });
    spans.push(Span::styled(view.after.to_owned(), normal));
    Some(Line::from(spans))
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
        for (key, label) in item.kind.rail() {
            rail.push(Span::styled(key, key_style));
            rail.push(Span::styled(format!(" {label}  "), normal));
        }
    }
    let text = queue
        .feedback()
        .map_or_else(|| body_text(item), str::to_owned);
    Parts {
        header: Vec::new(),
        body: wrapped(&text, normal, width),
        field: None,
        rail: footer_lines(rail, width),
    }
}

/// How many rows the body wraps to at the terminal's width (the scroll bound).
#[must_use]
pub(crate) fn body_rows(item: &DecisionItem, state: &TuiState) -> usize {
    let width = inner_width(state.terminal_width);
    if let (Validity::Live, DecisionSubject::Permission(payload)) = (&item.validity, &item.subject)
    {
        let prepared = prepared_permission(state, payload, width, &state.theme);
        return if payload.reviewing {
            prepared.review.body.len()
        } else {
            prepared.card.body.len()
        };
    }
    parts(item, state, &state.theme, width, usize::MAX)
        .body
        .len()
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
pub(crate) fn render(
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
    // Permission rows were sliced from the cached layout in `parts`.
    let body: Vec<Line<'static>> = if matches!(
        (&item.validity, &item.subject),
        (Validity::Live, DecisionSubject::Permission(_))
    ) {
        body
    } else {
        let first = queue.scroll().min(body.len().saturating_sub(1));
        body.into_iter().skip(first).collect()
    };

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
}
