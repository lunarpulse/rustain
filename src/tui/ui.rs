use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Tabs, Wrap};

use crate::types::app_state::{AppMode, AppState, Focus};
use crate::types::conversation::MessageRole;

/// Root rendering function — draws the entire TUI layout.
pub fn render(frame: &mut Frame, state: &AppState) {
    let area = frame.area();

    // Main layout: tab bar | chat + sidebar | status bar | input
    let main_layout = Layout::vertical([
        Constraint::Length(1),  // Tab bar
        Constraint::Min(1),    // Chat area (+ optional sidebar)
        Constraint::Length(1), // Status bar
        Constraint::Length(3), // Input area
    ])
    .split(area);

    render_tab_bar(frame, main_layout[0], state);
    render_chat_area(frame, main_layout[1], state);
    render_status_bar(frame, main_layout[2], state);
    render_input(frame, main_layout[3], state);

    // Overlays (rendered last, on top of everything)
    render_overlays(frame, main_layout[3], state);
}

fn render_tab_bar(frame: &mut Frame, area: Rect, state: &AppState) {
    let tab_titles: Vec<Line> = state
        .tabs
        .iter()
        .map(|t| Line::from(format!(" {} ", t.title())))
        .collect();

    let tabs = Tabs::new(tab_titles)
        .select(state.active_tab)
        .highlight_style(Style::default().bold().fg(Color::Cyan));

    frame.render_widget(tabs, area);
}

fn render_chat_area(frame: &mut Frame, area: Rect, state: &AppState) {
    let chat_area = if state.show_sidebar {
        let layout = Layout::horizontal([
            Constraint::Percentage(70),
            Constraint::Percentage(30),
        ])
        .split(area);

        render_sidebar(frame, layout[1]);
        layout[0]
    } else {
        area
    };

    let tab = state.active_tab();

    match &tab.conversation {
        None => {
            let welcome = Paragraph::new("Start a conversation by typing a message below.")
                .style(Style::default().fg(Color::DarkGray))
                .block(Block::default().borders(Borders::NONE));
            frame.render_widget(welcome, chat_area);
        }
        Some(conv) => {
            if conv.messages.is_empty() {
                let welcome = Paragraph::new("Start a conversation by typing a message below.")
                    .style(Style::default().fg(Color::DarkGray));
                frame.render_widget(welcome, chat_area);
            } else {
                // TODO: Replace with MessageWidget that renders ContentBlocks
                // with virtual scrolling and proper height calculation
                let msg_text: String = conv
                    .messages
                    .iter()
                    .map(|m| {
                        let prefix = match m.role {
                            MessageRole::User => ">>> ",
                            MessageRole::Assistant => "<<< ",
                            MessageRole::System => "[sys] ",
                            MessageRole::Tool => "[tool] ",
                        };
                        format!("{}{}\n", prefix, m.content)
                    })
                    .collect();

                let chat = Paragraph::new(msg_text)
                    .block(Block::default().borders(Borders::NONE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(chat, chat_area);
            }
        }
    }
}

fn render_sidebar(frame: &mut Frame, area: Rect) {
    let sidebar = Block::default()
        .title(" History ")
        .borders(Borders::LEFT);
    frame.render_widget(sidebar, area);
}

fn render_status_bar(frame: &mut Frame, area: Rect, state: &AppState) {
    let model = &state.model;
    let mode = match state.permission_mode {
        crate::types::app_state::PermissionMode::Yolo => "yolo",
        crate::types::app_state::PermissionMode::Normal => "normal",
        crate::types::app_state::PermissionMode::Plan => "plan",
    };

    let usage_pct = state
        .active_tab()
        .conversation
        .as_ref()
        .and_then(|c| c.usage.as_ref())
        .map(|u| format!("ctx: {:.0}%", u.percentage))
        .unwrap_or_default();

    let status = format!(
        " {} | mode: {} | {} | {}",
        model,
        mode,
        usage_pct,
        state.status_message.as_deref().unwrap_or("Ready"),
    );

    let status_widget = Paragraph::new(status)
        .style(Style::default().fg(Color::DarkGray).bg(Color::Black));
    frame.render_widget(status_widget, area);
}

fn render_input(frame: &mut Frame, area: Rect, state: &AppState) {
    let border_style = if state.focus == Focus::Input {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let input = Paragraph::new(state.input_buffer.as_str()).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(" Message "),
    );
    frame.render_widget(input, area);

    // Show cursor in input
    if state.focus == Focus::Input {
        frame.set_cursor_position((
            area.x + state.cursor_position as u16 + 1,
            area.y + 1,
        ));
    }
}

/// Render overlay widgets (dropdowns, permission prompts) on top of main layout
fn render_overlays(frame: &mut Frame, input_area: Rect, state: &AppState) {
    match &state.mode {
        AppMode::PermissionPrompt {
            tool_name,
            tool_input,
            ..
        } => {
            let width = 50.min(frame.area().width.saturating_sub(4));
            let height = 7;
            let x = (frame.area().width.saturating_sub(width)) / 2;
            let y = (frame.area().height.saturating_sub(height)) / 2;
            let area = Rect::new(x, y, width, height);

            frame.render_widget(Clear, area);

            let text = format!(
                "Tool: {}\nInput: {}\n\n[y] Allow  [a] Always  [n] Deny  [Esc] Cancel",
                tool_name,
                truncate(tool_input, 60),
            );
            let prompt = Paragraph::new(text).block(
                Block::default()
                    .title(" Permission Required ")
                    .title_style(Style::default().bold().fg(Color::Yellow))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow)),
            );
            frame.render_widget(prompt, area);
        }

        AppMode::SlashCommandDropdown { filter, selected } => {
            // TODO: Render filtered command list as floating dropdown above input
            let _dropdown_area = Rect::new(
                input_area.x + 1,
                input_area.y.saturating_sub(8),
                40.min(input_area.width),
                6,
            );
            // frame.render_widget(Clear, dropdown_area);
            // frame.render_widget(dropdown_widget(commands, *selected), dropdown_area);
        }

        AppMode::MentionDropdown { filter, selected } => {
            // TODO: Render filtered mention list (files, agents, MCP servers)
        }

        AppMode::AskUserQuestion { question, .. } => {
            // TODO: Render inline question card
        }

        AppMode::PlanApproval { plan_content } => {
            // TODO: Render plan approval card (approve/feedback/new session)
        }

        AppMode::Normal => {}
    }
}

fn truncate(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        &s[..max_len]
    }
}
