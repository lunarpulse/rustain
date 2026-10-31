//! S-QUIT: the guarded quit decision (Story 21.2 AC7, A3).
//!
//! One bounded presentation state with a saved caller focus, a shared widget
//! and a shared request/key transition used by BOTH hosts (the local TUI and the
//! attached client). It is deliberately *not* a modal stack: 21-4 migrates and
//! deletes it. Requesting or cancelling quit never interrupts work, drains
//! queues, resolves cards or touches the draft, focus or scroll; only the
//! explicit `y` makes the host perform its existing exit — exactly once.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::{ModalPart, fit_modal, modal_area};
use crate::domain::models::FocusState;

/// Which client raised the decision: it decides the consequences painted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitHost {
    /// The local TUI: quitting interrupts all owned work and exits.
    Local,
    /// The attached client: quitting closes THIS client only; the daemon and
    /// its work continue.
    Attached,
}

/// Real execution/draft facts at the instant quit was requested. Counts are
/// gathered from live sources by the host (every owned tab, attachments and
/// peer composition included) — never from status text or a completed wave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuitFacts {
    pub host: QuitHost,
    /// Conversations with a live stream/turn (all owned tabs).
    pub streams: usize,
    /// Tool calls currently running.
    pub live_tools: usize,
    /// Agents / wave spokes still running.
    pub agents: usize,
    /// Turns queued but not yet sent (all tabs).
    pub queued_turns: usize,
    /// The composer holds unsent text.
    pub composer_draft: bool,
    /// Images attached to the unsent message.
    pub pending_images: usize,
    /// A peer/recipient composition is in progress.
    pub peer_compose: bool,
    /// Permission / plan / consent decisions waiting on the operator.
    pub pending_decisions: usize,
}

impl QuitFacts {
    /// Facts for a host with nothing running and nothing unsent.
    pub fn idle(host: QuitHost) -> Self {
        Self {
            host,
            streams: 0,
            live_tools: 0,
            agents: 0,
            queued_turns: 0,
            composer_draft: false,
            pending_images: 0,
            peer_compose: false,
            pending_decisions: 0,
        }
    }

    /// True when quitting would lose nothing: the host may exit directly.
    pub fn is_idle(&self) -> bool {
        self.streams == 0
            && self.live_tools == 0
            && self.agents == 0
            && self.queued_turns == 0
            && !self.composer_draft
            && self.pending_images == 0
            && !self.peer_compose
            && self.pending_decisions == 0
    }

    /// The actual consequences, one per line, for the decision frame.
    pub fn consequences(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let plural = |n: usize, one: &str, many: &str| {
            if n == 1 {
                format!("1 {one}")
            } else {
                format!("{n} {many}")
            }
        };
        match self.host {
            QuitHost::Local => {
                if self.streams > 0 {
                    lines.push(format!(
                        "{} will be interrupted.",
                        plural(self.streams, "running turn", "running turns")
                    ));
                }
                if self.live_tools > 0 {
                    lines.push(format!(
                        "{} will be stopped.",
                        plural(self.live_tools, "running tool", "running tools")
                    ));
                }
                if self.agents > 0 {
                    lines.push(format!(
                        "{} will be cancelled.",
                        plural(self.agents, "running agent", "running agents")
                    ));
                }
            }
            QuitHost::Attached => {
                if self.streams > 0 || self.live_tools > 0 || self.agents > 0 {
                    lines.push(
                        "Daemon work in progress continues without this client (Ctrl+C stops it)."
                            .to_string(),
                    );
                }
            }
        }
        if self.queued_turns > 0 {
            lines.push(format!(
                "{} will be discarded.",
                plural(self.queued_turns, "queued message", "queued messages")
            ));
        }
        if self.composer_draft {
            lines.push("Your unsent draft will be lost.".to_string());
        }
        if self.pending_images > 0 {
            lines.push(format!(
                "{} will be dropped.",
                plural(self.pending_images, "attached image", "attached images")
            ));
        }
        if self.peer_compose {
            lines.push("Your peer reply in progress will be lost.".to_string());
        }
        if self.pending_decisions > 0 {
            lines.push(format!(
                "{} will go unanswered.",
                plural(
                    self.pending_decisions,
                    "pending decision",
                    "pending decisions"
                )
            ));
        }
        lines
    }
}

/// The one open S-QUIT decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuitConfirmState {
    pub facts: QuitFacts,
    /// Caller focus saved at request time and restored on cancel.
    pub caller_focus: FocusState,
}

/// Outcome of asking to quit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitRequest {
    /// Nothing to lose and no frame open: the host exits directly.
    ExitNow,
    /// The decision frame is (now) open; nothing else happens.
    Confirming,
}

/// Request quit. Never stacks a second frame; never touches work, draft,
/// queues, cards, focus or scroll.
pub fn request(state: &mut TuiState, facts: QuitFacts) -> QuitRequest {
    if state.quit_confirm.is_some() {
        return QuitRequest::Confirming;
    }
    if facts.is_idle() {
        return QuitRequest::ExitNow;
    }
    state.quit_confirm = Some(QuitConfirmState {
        facts,
        caller_focus: state.focus.clone(),
    });
    state.needs_redraw = true;
    QuitRequest::Confirming
}

/// Host-neutral key classes the decision frame understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitKey {
    Char(char),
    Enter,
    Esc,
    CtrlC,
    /// Any other key: consumed, never leaked to the surface below.
    Other,
}

/// Result of a key while the frame is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitKeyOutcome {
    /// Explicit `y`: the host performs its existing exit exactly once.
    Confirmed,
    /// `n` / Esc / Ctrl+C / Enter: frame closed, caller restored.
    Cancelled,
    /// Everything else: swallowed.
    Consumed,
}

/// Apply one key to the open frame. Without an open frame it returns
/// `Consumed` and changes nothing (callers gate on `quit_confirm.is_some()`).
pub fn handle_key(state: &mut TuiState, key: QuitKey) -> QuitKeyOutcome {
    if state.quit_confirm.is_none() {
        return QuitKeyOutcome::Consumed;
    }
    state.needs_redraw = true;
    match key {
        QuitKey::Char('y' | 'Y') => {
            state.quit_confirm = None;
            QuitKeyOutcome::Confirmed
        }
        QuitKey::Char('n' | 'N') | QuitKey::Esc | QuitKey::CtrlC | QuitKey::Enter => {
            if let Some(frame) = state.quit_confirm.take() {
                state.focus = frame.caller_focus;
            }
            QuitKeyOutcome::Cancelled
        }
        QuitKey::Char(_) | QuitKey::Other => QuitKeyOutcome::Consumed,
    }
}

/// Apply a local domain event to the open frame. User keys, paste and mouse are
/// consumed (never edit the draft or scroll underneath); resize/focus events are
/// not user input and return `None` so the caller handles them normally.
pub fn handle_domain_event(
    state: &mut TuiState,
    event: &crate::domain::events::DomainInputEvent,
) -> Option<QuitKeyOutcome> {
    use crate::domain::events::{DomainInputEvent, DomainKey};
    let key = match event {
        DomainInputEvent::Resize(..)
        | DomainInputEvent::FocusGained
        | DomainInputEvent::FocusLost => return None,
        DomainInputEvent::KeyPress(c) => QuitKey::Char(*c),
        DomainInputEvent::SpecialKey(DomainKey::Enter) => QuitKey::Enter,
        DomainInputEvent::SpecialKey(DomainKey::Esc) => QuitKey::Esc,
        DomainInputEvent::SpecialKey(DomainKey::CtrlC) => QuitKey::CtrlC,
        _ => QuitKey::Other,
    };
    Some(handle_key(state, key))
}

/// Apply a raw crossterm key to the open frame (the attached client's ingress).
pub fn handle_raw_key(state: &mut TuiState, key: crossterm::event::KeyEvent) -> QuitKeyOutcome {
    use crossterm::event::{KeyCode, KeyModifiers};
    let plain = !key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT);
    let quit_key = match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => QuitKey::CtrlC,
        KeyCode::Char(c) if plain => QuitKey::Char(c),
        KeyCode::Enter if plain => QuitKey::Enter,
        KeyCode::Esc => QuitKey::Esc,
        _ => QuitKey::Other,
    };
    handle_key(state, quit_key)
}

/// Paint the decision frame on top of every other overlay: a System/Quit title,
/// the actual consequences and the safe rail (Stay is the default).
pub fn render(frame: &mut Frame, area: Rect, state: &QuitConfirmState, theme: &Theme) {
    if area.width < 12 || area.height < 5 {
        return;
    }
    let width = area.width.min(64);
    let inner_width = usize::from(width.saturating_sub(4));
    let muted = Style::default().fg(theme.colors.fg_muted);
    let normal = Style::default().fg(theme.colors.fg_primary);
    let warn = Style::default()
        .fg(theme.colors.warning)
        .add_modifier(Modifier::BOLD);

    let mut header = crate::adapters::tui::widgets::modal::wrapped(
        match state.facts.host {
            QuitHost::Local => "Quit rustain?",
            QuitHost::Attached => "Leave this attached client?",
        },
        warn,
        inner_width,
    );
    if header.is_empty() {
        header.push(Line::from(""));
    }
    let mut body = Vec::new();
    for line in state.facts.consequences() {
        body.extend(crate::adapters::tui::widgets::modal::wrapped(
            &format!("• {line}"),
            normal,
            inner_width,
        ));
    }
    if state.facts.host == QuitHost::Attached {
        body.extend(crate::adapters::tui::widgets::modal::wrapped(
            "This closes this client only; the daemon keeps running.",
            muted,
            inner_width,
        ));
    }
    let footer = vec![Line::from(vec![
        Span::styled("[y]", warn),
        Span::styled(" Quit   ", normal),
        Span::styled("[n/Esc/Enter]", Style::default().fg(theme.colors.accent)),
        Span::styled(" Stay", normal),
    ])];

    // Chrome: borders + title; the body shrinks, the rail never does.
    let wanted = u16::try_from(header.len() + body.len() + footer.len() + 2).unwrap_or(u16::MAX);
    let modal = modal_area(area, width, wanted);
    frame.render_widget(Clear, modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.warning))
        .title(" System · Quit ")
        .style(Style::default().bg(theme.colors.bg_surface));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    let parts = vec![
        ModalPart::Header(header),
        ModalPart::Notice(body),
        ModalPart::Footer(footer),
    ];
    let lines = fit_modal(parts, usize::from(inner.height), usize::from(inner.width));
    frame.render_widget(
        Paragraph::new(lines).style(
            Style::default()
                .fg(theme.colors.fg_primary)
                .bg(theme.colors.bg_surface),
        ),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn busy() -> QuitFacts {
        QuitFacts {
            streams: 1,
            composer_draft: true,
            ..QuitFacts::idle(QuitHost::Local)
        }
    }

    #[test]
    fn story21_2_idle_exits_directly_without_a_frame() {
        let mut state = TuiState::new(80, 24);
        assert_eq!(
            request(&mut state, QuitFacts::idle(QuitHost::Local)),
            QuitRequest::ExitNow
        );
        assert!(state.quit_confirm.is_none());
    }

    #[test]
    fn story21_2_request_cancel_and_confirm_restore_exactly() {
        let mut state = TuiState::new(80, 24);
        state.focus = FocusState::Chat;
        state.input_buffer = "unsent draft".into();
        state.cursor_position = 3;
        assert_eq!(request(&mut state, busy()), QuitRequest::Confirming);
        // Repeated requests never stack a second frame or change the saved caller.
        state.focus = FocusState::Input;
        assert_eq!(request(&mut state, busy()), QuitRequest::Confirming);
        assert_eq!(
            state.quit_confirm.as_ref().unwrap().caller_focus,
            FocusState::Chat
        );
        // Unrelated keys are consumed and leave the draft alone.
        assert_eq!(
            handle_key(&mut state, QuitKey::Char('x')),
            QuitKeyOutcome::Consumed
        );
        assert_eq!(state.input_buffer, "unsent draft");
        for cancel in [
            QuitKey::Char('n'),
            QuitKey::Esc,
            QuitKey::CtrlC,
            QuitKey::Enter,
        ] {
            let mut s = TuiState::new(80, 24);
            s.focus = FocusState::Chat;
            s.input_buffer = "keep".into();
            request(&mut s, busy());
            assert_eq!(handle_key(&mut s, cancel), QuitKeyOutcome::Cancelled);
            assert!(s.quit_confirm.is_none());
            assert_eq!(s.focus, FocusState::Chat);
            assert_eq!(s.input_buffer, "keep");
        }
        assert_eq!(
            handle_key(&mut state, QuitKey::Char('y')),
            QuitKeyOutcome::Confirmed
        );
        assert!(state.quit_confirm.is_none());
    }

    #[test]
    fn story21_2_consequences_name_the_real_loss_per_host() {
        let local = QuitFacts {
            streams: 2,
            live_tools: 1,
            agents: 1,
            queued_turns: 3,
            composer_draft: true,
            pending_images: 1,
            peer_compose: true,
            pending_decisions: 1,
            ..QuitFacts::idle(QuitHost::Local)
        }
        .consequences()
        .join("\n");
        for needle in [
            "2 running turns",
            "1 running tool",
            "1 running agent",
            "3 queued messages",
            "unsent draft",
            "1 attached image",
            "peer reply",
            "1 pending decision",
        ] {
            assert!(local.contains(needle), "missing `{needle}` in {local}");
        }
        let attached = QuitFacts {
            streams: 1,
            ..QuitFacts::idle(QuitHost::Attached)
        }
        .consequences()
        .join("\n");
        assert!(attached.contains("Daemon work in progress continues"));
        assert!(!attached.contains("interrupted"));
    }

    #[test]
    fn story21_2_quit_frame_fits_60x16_with_title_consequences_and_rail() {
        let mut state = TuiState::new(60, 16);
        request(
            &mut state,
            QuitFacts {
                streams: 1,
                live_tools: 2,
                agents: 3,
                queued_turns: 4,
                composer_draft: true,
                pending_images: 1,
                peer_compose: true,
                pending_decisions: 2,
                ..QuitFacts::idle(QuitHost::Local)
            },
        );
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    state.quit_confirm.as_ref().unwrap(),
                    &Theme::dark(),
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text: String = (0..16)
            .map(|y| {
                (0..60)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        assert!(text.contains("System · Quit"), "{text}");
        assert!(text.contains("[y] Quit"), "{text}");
        assert!(text.contains("[n/Esc/Enter] Stay"), "{text}");
        assert!(text.contains("running turn"), "{text}");
    }
}
