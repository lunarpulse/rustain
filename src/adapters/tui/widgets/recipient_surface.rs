use crate::adapters::tui::state::{RecipientSurfaceAccess, RecipientSurfaceTarget};
use crate::domain::models::{ChatMessage, Conversation};

pub(crate) fn pinned_response<'a>(
    conversation: &'a Conversation,
    node: &str,
) -> Option<(&'a ChatMessage, &'a crate::domain::models::PeerRowControl)> {
    conversation.messages.iter().find_map(|message| {
        let control = conversation.peer_row_controls.get(&message.id)?;
        (control.node_id == node).then_some((message, control))
    })
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecipientSurfaceAction {
    Respond,
    Discuss,
    Busy,
    Acknowledge,
    Approve,
    Edit,
    Reject,
    Decline,
    Context,
}
impl RecipientSurfaceAction {
    pub(crate) const fn key(self) -> char {
        match self {
            Self::Respond => 'r',
            Self::Discuss => 'd',
            Self::Busy => 'b',
            Self::Acknowledge => 'a',
            Self::Approve => 'y',
            Self::Edit => 'e',
            Self::Reject | Self::Decline => 'n',
            Self::Context => 'c',
        }
    }

    const fn hint(self) -> &'static str {
        match self {
            Self::Respond => "[r] Respond",
            Self::Discuss => "[d] Discuss",
            Self::Busy => "[b] Busy — defer 30m",
            Self::Acknowledge => "[a] Acknowledge",
            Self::Approve => "[y] Approve",
            Self::Edit => "[e] Edit",
            Self::Reject => "[n] Reject",
            Self::Decline => "[n] Decline",
            Self::Context => "[c] Use as context",
        }
    }
}
pub(crate) fn recipient_surface_actions(
    state: &crate::adapters::tui::state::TuiState,
    conversation: &Conversation,
    read_only: bool,
) -> [Option<RecipientSurfaceAction>; 6] {
    use crate::adapters::tui::state::RecipientSurfaceTarget;
    use crate::domain::models::{PeerRowStage, SemanticMessageType};
    let mut actions = [None; 6];
    let Some(surface) = state.recipient_surface.as_ref() else {
        return actions;
    };
    if read_only || surface.access != RecipientSurfaceAccess::Writer {
        return actions;
    }
    let control = surface
        .node_id
        .as_deref()
        .and_then(|node| pinned_response(conversation, node))
        .map(|(_, control)| control);
    let live_drafting =
        control.is_some_and(|c| c.node_resolvable && c.stage == PeerRowStage::Drafting);
    if matches!(surface.target, RecipientSurfaceTarget::Item(_))
        && surface.item_state != Some("removed")
    {
        if !live_drafting {
            actions[0] = match surface.message_type {
                SemanticMessageType::ArchitectureUpdate => Some(RecipientSurfaceAction::Discuss),
                SemanticMessageType::Consultation => Some(RecipientSurfaceAction::Respond),
                _ if control.is_some_and(|c| c.stage != PeerRowStage::Settled) => {
                    Some(RecipientSurfaceAction::Respond)
                }
                _ => None,
            };
        }
        actions[1] = match surface.message_type {
            SemanticMessageType::Consultation => Some(RecipientSurfaceAction::Busy),
            // Story 19.21 `C5` — `UX-DR-TM-01:111` ratified `[✓] Confirm →
            // [Acknowledge]` for a story assignment; the 2026-10-07 matrix's
            // Act-2 scope was staging, not repeal. The shipped arm, extended:
            // the same `/team ack` frame, the same correlated Applied/
            // Idempotent result, and the daemon's transition gate rechecks it.
            // A receipt, ⛔ not an acceptance and not a person acting.
            SemanticMessageType::ArchitectureUpdate | SemanticMessageType::StoryAssignment
                if surface.item_state == Some("received") =>
            {
                Some(RecipientSurfaceAction::Acknowledge)
            }
            _ => None,
        };
    } else if matches!(surface.target, RecipientSurfaceTarget::Node(_))
        && control.is_some_and(|c| {
            c.node_resolvable
                && matches!(
                    c.stage,
                    PeerRowStage::AwaitingOperator | PeerRowStage::Ready
                )
        })
    {
        actions[0] = Some(RecipientSurfaceAction::Respond);
    }
    if let Some(control) =
        control.filter(|c| c.node_resolvable && surface.item_state != Some("removed"))
    {
        match control.stage {
            PeerRowStage::Ready => {
                actions[2] = Some(RecipientSurfaceAction::Approve);
                actions[3] = Some(RecipientSurfaceAction::Edit);
                actions[4] = Some(RecipientSurfaceAction::Reject);
            }
            PeerRowStage::AwaitingOperator => actions[4] = Some(RecipientSurfaceAction::Decline),
            _ => {}
        }
    }
    if matches!(surface.target, RecipientSurfaceTarget::Item(_))
        && (surface.context_available || surface.context_selected)
    {
        actions[5] = Some(RecipientSurfaceAction::Context);
    }
    use crate::adapters::daemon::protocol::RecipientAction;
    use crate::adapters::tui::handlers::recipient_actions::{
        action_is_pending, pending_composition,
    };
    for slot in &mut actions {
        let Some(action) = *slot else {
            continue;
        };
        let wire_action = match action {
            RecipientSurfaceAction::Busy => RecipientAction::Defer,
            RecipientSurfaceAction::Acknowledge => RecipientAction::Acknowledge,
            RecipientSurfaceAction::Context if surface.context_selected => {
                RecipientAction::ContextRemove
            }
            RecipientSurfaceAction::Context => RecipientAction::ContextSelect,
            RecipientSurfaceAction::Respond | RecipientSurfaceAction::Discuss
                if matches!(surface.target, RecipientSurfaceTarget::Item(_)) =>
            {
                RecipientAction::Reply
            }
            _ => RecipientAction::Response,
        };
        if (matches!(
            action,
            RecipientSurfaceAction::Respond
                | RecipientSurfaceAction::Discuss
                | RecipientSurfaceAction::Edit
        ) && pending_composition(state).is_some())
            || action_is_pending(
                state,
                wire_action,
                &surface.target,
                surface.node_id.as_deref(),
            )
            || (action == RecipientSurfaceAction::Context
                && action_is_pending(
                    state,
                    if surface.context_selected {
                        RecipientAction::ContextSelect
                    } else {
                        RecipientAction::ContextRemove
                    },
                    &surface.target,
                    surface.node_id.as_deref(),
                ))
        {
            *slot = None;
        }
    }
    actions
}
pub(crate) fn render_recipient_surface(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    surface: &mut crate::adapters::tui::state::RecipientSurface,
    notices: &std::collections::BTreeMap<
        (
            crate::adapters::daemon::protocol::RecipientAction,
            crate::adapters::daemon::protocol::RecipientActionTarget,
        ),
        crate::adapters::tui::state::RecipientActionState,
    >,
    actions: &[Option<RecipientSurfaceAction>; 6],
    read_only: bool,
    theme: &crate::adapters::tui::theme::Theme,
) {
    use crate::adapters::tui::state::RecipientSurfaceTarget;
    use ratatui::widgets::{Block, Borders, Clear};
    if area.width < 3 || area.height < 4 {
        return;
    }
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(ratatui::style::Style::default().fg(theme.colors.info_border));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width_changed = surface.cached_width != inner.width;
    if width_changed {
        surface.cached_lines = surface
            .content
            .split('\n')
            .chain(surface.results.iter().flat_map(|result| result.split('\n')))
            .flat_map(|line| {
                wrap_recipient_line(
                    line,
                    usize::from(inner.width),
                    0,
                    if line.starts_with("      ") {
                        theme.colors.fg_primary
                    } else {
                        theme.colors.fg_secondary
                    },
                )
            })
            .collect();
        for notice in crate::adapters::tui::handlers::recipient_actions::surface_action_notices(
            notices,
            &surface.target,
            surface.node_id.as_deref(),
        ) {
            surface.cached_lines.extend(wrap_recipient_line(
                &notice,
                usize::from(inner.width),
                2,
                theme.colors.fg_primary,
            ));
        }
        let mut heading = match &surface.target {
            RecipientSurfaceTarget::Item(address) => format!(
                "item actions · {}\n{}",
                address.item().as_str(),
                if surface.message_type == crate::domain::models::SemanticMessageType::Unknown {
                    "unknown type"
                } else {
                    crate::domain::models::semantic_message_type_metadata(surface.message_type)
                        .token
                },
            ),
            RecipientSurfaceTarget::Node(node) => format!("response actions · {node}"),
        };
        if surface.context_selected {
            heading.push_str(" · peer-tainted");
        }
        if let (Some(root), RecipientSurfaceTarget::Item(address)) =
            (&surface.root_id, &surface.target)
            && root != address.item().as_str()
        {
            heading.push_str("\nthread root · ");
            heading.push_str(root);
        }
        surface.cached_header = heading
            .split('\n')
            .flat_map(|line| {
                wrap_recipient_line(line, usize::from(inner.width), 2, theme.colors.fg_secondary)
            })
            .collect();
        surface.cached_width = inner.width;
    }
    let action_mask = actions.iter().flatten().fold(
        (u16::from(read_only) << 15)
            | (u16::from(surface.access == RecipientSurfaceAccess::LocalOnly) << 14)
            | (u16::from(surface.omitted_before > 0) << 13)
            | (u16::from(surface.omitted_after > 0) << 12)
            | (u16::from(surface.context_selected) << 11),
        |mask, action| mask | (1 << (*action as u8)),
    );
    if width_changed || surface.cached_action_mask != action_mask {
        let mut footer = String::new();
        if surface.access == RecipientSurfaceAccess::LocalOnly {
            footer.push_str("local view only — replying, acknowledgement and item context require a daemon-attached writer\n");
        } else if read_only {
            footer.push_str("read-only — view only; item actions require the attached writer\n");
        } else {
            for action in actions.iter().flatten() {
                if !footer.is_empty() {
                    footer.push_str("  ");
                }
                footer.push_str(
                    if *action == RecipientSurfaceAction::Context && surface.context_selected {
                        "[c] Stop using as context"
                    } else {
                        action.hint()
                    },
                );
            }
            footer.push('\n');
        }
        if surface.can_toggle_target {
            footer.push_str(if read_only {
                "[Tab] View item/response\n"
            } else {
                "[Tab] Item/Response actions\n"
            });
        }
        if surface.omitted_before > 0 {
            footer.push_str("[p] Earlier  ");
        }
        if surface.omitted_after > 0 {
            footer.push_str("[l] Newer  ");
        }
        if surface.access != RecipientSurfaceAccess::LocalOnly {
            footer.push_str("  Ctrl+D/Ctrl+C: detach");
        }
        footer.push_str("[PgUp/PgDn] Scroll  [Esc] Close");
        surface.cached_footer = footer
            .split('\n')
            .flat_map(|line| {
                wrap_recipient_line(line, usize::from(inner.width), 2, theme.colors.fg_secondary)
            })
            .collect();
        surface.cached_action_mask = action_mask;
    }
    let header_height = u16::try_from(surface.cached_header.len())
        .unwrap_or(u16::MAX)
        .min(inner.height.saturating_sub(1));
    let footer_height = u16::try_from(surface.cached_footer.len())
        .unwrap_or(u16::MAX)
        .min(inner.height.saturating_sub(header_height + 1));
    let content_height = inner.height.saturating_sub(header_height + footer_height);
    surface.viewport_rows = usize::from(content_height);
    let top = ratatui::layout::Rect::new(inner.x, inner.y, inner.width, header_height);
    let body = ratatui::layout::Rect::new(
        inner.x,
        inner.y + header_height,
        inner.width,
        content_height,
    );
    let bottom = ratatui::layout::Rect::new(
        inner.x,
        inner.y + inner.height - footer_height,
        inner.width,
        footer_height,
    );
    paint_recipient_lines(frame, top, &surface.cached_header);
    let start = surface.scroll_offset.min(
        surface
            .cached_lines
            .len()
            .saturating_sub(usize::from(content_height)),
    );
    surface.scroll_offset = start;
    paint_recipient_lines(frame, body, &surface.cached_lines[start..]);
    let footer_start = surface
        .cached_footer
        .len()
        .saturating_sub(usize::from(footer_height));
    paint_recipient_lines(frame, bottom, &surface.cached_footer[footer_start..]);
}

fn wrap_recipient_line(
    text: &str,
    width: usize,
    indent: usize,
    color: ratatui::style::Color,
) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::{
        style::Style,
        text::{Line, Span},
    };
    use unicode_width::UnicodeWidthStr as _;
    let indent = indent.min(width.saturating_sub(2));
    let prefix = &"      "[..indent];
    let mut result = Vec::new();
    for paragraph in text.split('\n') {
        let hanging = (indent + paragraph.bytes().take_while(|byte| *byte == b' ').count())
            .min(6)
            .min(width.saturating_sub(2));
        let span = Span::raw(paragraph);
        let mut line = String::with_capacity(paragraph.len().min(width.saturating_mul(4)) + indent);
        line.push_str(prefix);
        let mut used = indent;
        for grapheme in span.styled_graphemes(Style::default()) {
            let cells = grapheme.symbol.width();
            if used + cells > width && used > indent {
                result.push(Line::styled(
                    std::mem::take(&mut line),
                    Style::default().fg(color),
                ));
                line.push_str(&"      "[..hanging]);
                used = hanging;
            }
            line.push_str(grapheme.symbol);
            used += cells;
        }
        result.push(Line::styled(line, Style::default().fg(color)));
    }
    result
}
fn paint_recipient_lines(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    lines: &[ratatui::text::Line<'static>],
) {
    for (index, line) in lines.iter().take(usize::from(area.height)).enumerate() {
        frame.render_widget(
            line,
            ratatui::layout::Rect::new(area.x, area.y + index as u16, area.width, 1),
        );
    }
}
