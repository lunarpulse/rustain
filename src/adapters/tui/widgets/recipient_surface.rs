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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecipientSurfaceAction {
    Respond,
    Discuss,
    Busy,
    Acknowledge,
    Approve,
    Edit,
    Reject,
    Decline,
    ShareStatus,
    DeclineStatus,
    Context,
    /// Story 19.22 `AC4(c)` — `[d] Discuss concerns` on an item that carries a
    /// story list; prefills a typed `--discuss` verdict reply.
    DiscussConcerns,
    /// Story 19.22 `AC4(c)` — `[y] Accept` when the list assigns a story to
    /// you; queues the Accept verdict frame directly.
    Accept,
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
            Self::Reject | Self::Decline | Self::DeclineStatus => 'n',
            Self::ShareStatus => 's',
            Self::Context => 'c',
            Self::DiscussConcerns => 'd',
            Self::Accept => 'y',
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
            Self::ShareStatus => "[s] Share status",
            Self::DeclineStatus => "[n] Decline",
            Self::Context => "[c] Use as context",
            Self::DiscussConcerns => "[d] Discuss concerns",
            Self::Accept => "[y] Accept",
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
            // Story 19.22 `AC4(c)` — on a list item a verdict is offered in
            // place of Respond; every other type and stage is unchanged.
            actions[0] = if surface.verdict_offer != crate::domain::models::VerdictOffer::None {
                Some(RecipientSurfaceAction::DiscussConcerns)
            } else {
                match surface.message_type {
                    SemanticMessageType::ArchitectureUpdate | SemanticMessageType::BugReport => {
                        Some(RecipientSurfaceAction::Discuss)
                    }
                    SemanticMessageType::Consultation => Some(RecipientSurfaceAction::Respond),
                    _ if control.is_some_and(|c| c.stage != PeerRowStage::Settled) => {
                        Some(RecipientSurfaceAction::Respond)
                    }
                    _ => None,
                }
            };
            // Ready keeps its draft-review actions, including on an orphaned
            // control that is no longer resolvable.
            if surface.verdict_offer == crate::domain::models::VerdictOffer::AcceptOrDiscuss
                && !control.is_some_and(|c| c.stage == PeerRowStage::Ready)
            {
                actions[2] = Some(RecipientSurfaceAction::Accept);
            }
        }
        actions[1] = match surface.message_type {
            SemanticMessageType::Consultation => Some(RecipientSurfaceAction::Busy),
            // Story 19.21 `C5` — `UX-DR-TM-01:111` ratified `[✓] Confirm →
            // [Acknowledge]` for a story assignment; the 2026-10-07 matrix's
            // Act-2 scope was staging, not repeal. The shipped arm, extended:
            // the same `/team ack` frame, the same correlated Applied/
            // Idempotent result, and the daemon's transition gate rechecks it.
            // A receipt, ⛔ not an acceptance and not a person acting.
            SemanticMessageType::ArchitectureUpdate
            // Story 19.22 `AC4(c)` — an Act 6 scope change is acknowledged
            // like any received notice; a receipt, never an acceptance.
            | SemanticMessageType::ScopeChange
            | SemanticMessageType::StoryAssignment
            | SemanticMessageType::BugReport
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
    // Like `[r] Respond`, the status acts survive a restart: without a live
    // card the daemon appends the stored preview directly (orphan arm).
    if matches!(surface.target, RecipientSurfaceTarget::Item(_))
        && surface.item_state != Some("removed")
        && surface.message_type == SemanticMessageType::StatusRequest
        && control.is_some_and(|c| c.stage == PeerRowStage::AwaitingOperator && c.status.is_some())
    {
        actions[1] = Some(RecipientSurfaceAction::Busy);
        actions[2] = Some(RecipientSurfaceAction::ShareStatus);
        actions[4] = Some(RecipientSurfaceAction::DeclineStatus);
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
            RecipientSurfaceAction::ShareStatus | RecipientSurfaceAction::DeclineStatus => {
                RecipientAction::ShareStatus
            }
            RecipientSurfaceAction::Context if surface.context_selected => {
                RecipientAction::ContextRemove
            }
            RecipientSurfaceAction::Context => RecipientAction::ContextSelect,
            RecipientSurfaceAction::Respond
            | RecipientSurfaceAction::Discuss
            // Story 19.22 — verdict acts are replies too: same suppression key.
            | RecipientSurfaceAction::DiscussConcerns
            | RecipientSurfaceAction::Accept
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
                | RecipientSurfaceAction::DiscussConcerns
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
        // Story 19.22 — `u32`: 13 action discriminants (bits 0–12) plus the
        // five flags now packed at bits 27–31.
        (u32::from(read_only) << 31)
            | (u32::from(surface.access == RecipientSurfaceAccess::LocalOnly) << 30)
            | (u32::from(surface.omitted_before > 0) << 29)
            | (u32::from(surface.omitted_after > 0) << 28)
            | (u32::from(surface.context_selected) << 27),
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

#[cfg(test)]
mod tests {
    use super::RecipientSurfaceAction;

    #[test]
    fn status_action_keys_and_labels_are_exact() {
        assert_eq!(RecipientSurfaceAction::ShareStatus.key(), 's');
        assert_eq!(
            RecipientSurfaceAction::ShareStatus.hint(),
            "[s] Share status"
        );
        assert_eq!(RecipientSurfaceAction::DeclineStatus.key(), 'n');
        assert_eq!(RecipientSurfaceAction::DeclineStatus.hint(), "[n] Decline");
    }

    // Story 19.22 `AC4(c)` — 13 action discriminants plus the five footer
    // flag bits must stay disjoint inside a `u32` (the old `u16` collided).
    #[test]
    fn story19_22_action_bits_leave_room_for_the_five_flag_bits() {
        let actions = [
            RecipientSurfaceAction::Respond,
            RecipientSurfaceAction::Discuss,
            RecipientSurfaceAction::Busy,
            RecipientSurfaceAction::Acknowledge,
            RecipientSurfaceAction::Approve,
            RecipientSurfaceAction::Edit,
            RecipientSurfaceAction::Reject,
            RecipientSurfaceAction::Decline,
            RecipientSurfaceAction::ShareStatus,
            RecipientSurfaceAction::DeclineStatus,
            RecipientSurfaceAction::Context,
            RecipientSurfaceAction::DiscussConcerns,
            RecipientSurfaceAction::Accept,
        ];
        let mut bits = std::collections::BTreeSet::new();
        for action in actions {
            assert!(
                (action as u8) < 27,
                "{} crosses the flag band",
                action as u8
            );
            assert!(bits.insert(1u32 << (action as u8)), "duplicate bit");
        }
        for flag in [27u8, 28, 29, 30, 31] {
            assert!(bits.insert(1u32 << flag), "flag bit {flag} collides");
        }
        assert_eq!(RecipientSurfaceAction::DiscussConcerns.key(), 'd');
        assert_eq!(
            RecipientSurfaceAction::DiscussConcerns.hint(),
            "[d] Discuss concerns"
        );
        assert_eq!(RecipientSurfaceAction::Accept.key(), 'y');
        assert_eq!(RecipientSurfaceAction::Accept.hint(), "[y] Accept");
    }
}
