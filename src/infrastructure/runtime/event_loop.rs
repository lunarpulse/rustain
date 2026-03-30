use anyhow::Result;
use crossterm::event::EventStream;
use futures::StreamExt;
use tokio::sync::mpsc;

use crate::adapters::tui::app::{convert_crossterm_event, handle_input};
use crate::adapters::tui::color_detect::detect_color_capability;
use crate::adapters::tui::layout;
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::terminal::Tui;
use crate::adapters::tui::widgets::{chat_pane, input_box, status_bar};
use crate::domain::events::AppEvent;
use crate::domain::models::AppConfig;

/// Run the 4-branch tokio::select! event loop.
pub async fn run(
    terminal: &mut Tui,
    domain_events_rx: &mut mpsc::UnboundedReceiver<AppEvent>,
    config: &AppConfig,
) -> Result<()> {
    let size = terminal.size()?;
    let capability = detect_color_capability();
    let mut state = TuiState::with_capability(size.width, size.height, capability);

    let mut terminal_events = EventStream::new();
    let mut tick_interval = tokio::time::interval(std::time::Duration::from_millis(
        state.theme.timing.tick_interval_ms,
    ));

    // Render first frame immediately
    render(terminal, &state, &config.model)?;
    state.needs_redraw = false;

    loop {
        tokio::select! {
            // Branch 1: Terminal input (crossterm event stream)
            Some(event_result) = terminal_events.next() => {
                match event_result {
                    Ok(event) => {
                        if is_ctrl_c(&event) {
                            break;
                        }
                        if let Some(domain_event) = convert_crossterm_event(&event) {
                            handle_input(&mut state, &domain_event);
                        }
                    }
                    Err(e) => {
                        tracing::error!("Terminal event stream error: {}", e);
                        break;
                    }
                }
            }

            // Branch 2: Domain events (mpsc unbounded channel)
            Some(event) = domain_events_rx.recv() => {
                match event {
                    AppEvent::Shutdown => break,
                    AppEvent::SystemNotice(_level, msg) => {
                        state.status_message = msg;
                        state.needs_redraw = true;
                    }
                    _ => {
                        state.needs_redraw = true;
                    }
                }
            }

            // Branch 3: Render tick (250ms interval with needs_redraw optimization)
            _ = tick_interval.tick() => {
                if state.needs_redraw {
                    render(terminal, &state, &config.model)?;
                    state.needs_redraw = false;
                }
            }

            // Branch 4: Active task monitoring (placeholder for future stories)
            // Currently a no-op future that never resolves
        }

        if state.should_quit {
            break;
        }
    }

    Ok(())
}

fn is_ctrl_c(event: &crossterm::event::Event) -> bool {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            ..
        })
    )
}

/// Render the full TUI frame.
fn render(terminal: &mut Tui, state: &TuiState, model: &str) -> Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();

        match layout::compute_layout(area, &state.theme) {
            Some(app_layout) => {
                let is_compact = area.width < 80 || area.height < 24;

                chat_pane::render(frame, app_layout.chat_pane, false, &state.theme);
                status_bar::render(
                    frame,
                    app_layout.status_bar,
                    model,
                    &state.status_message,
                    is_compact,
                    &state.theme,
                );
                input_box::render(
                    frame,
                    app_layout.input_area,
                    &state.input_buffer,
                    state.cursor_position,
                    state.focus,
                    &state.theme,
                );
            }
            None => {
                // Terminal too small
                let msg = ratatui::widgets::Paragraph::new("Terminal too small (min 60x16)")
                    .style(ratatui::prelude::Style::default().fg(ratatui::prelude::Color::Red))
                    .alignment(ratatui::prelude::Alignment::Center);
                frame.render_widget(msg, area);
            }
        }
    })?;

    Ok(())
}
