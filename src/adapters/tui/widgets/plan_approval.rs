use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, BorderType, Clear, Paragraph, Wrap},
    Frame,
};

use crate::adapters::tui::state::PendingPlanApproval;
use crate::adapters::tui::theme::Theme;

/// Render the plan approval card as a centered overlay.
pub fn render_plan_approval_card(
    frame: &mut Frame,
    area: Rect,
    pending: &PendingPlanApproval,
    theme: &Theme,
) {
    let block = Block::default()
        .title(" Plan Approval ")
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(theme.colors.decision_border);

    // Compute inner area
    let inner = block.inner(area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Length(1), // summary
            Constraint::Min(3),    // contents
            Constraint::Length(1), // footer
        ])
        .split(inner);

    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    // Header
    let slug = pending
        .plan_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("plan");
    let header = Paragraph::new(Line::from(vec![
        Span::styled("Plan Approval — ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!("{}.md", slug)),
    ]));
    frame.render_widget(header, chunks[0]);

    // Summary
    let summary = Paragraph::new(Line::from(vec![
        Span::styled("Summary: ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(pending.summary.clone()),
    ]));
    frame.render_widget(summary, chunks[1]);

    // Contents (markdown-aware rendering would go here; for now plain text)
    let contents_text = Text::from(pending.contents.clone());
    let contents_para = Paragraph::new(contents_text)
        .wrap(Wrap { trim: true })
        .scroll((0, 0));
    frame.render_widget(contents_para, chunks[2]);

    // Action footer
    let footer = Paragraph::new(Line::from(vec![
        Span::styled("[y]", Style::default().add_modifier(Modifier::BOLD).fg(Color::Green)),
        Span::raw(" Approve  "),
        Span::styled("[a]", Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan)),
        Span::raw(" Approve & AutoEdit  "),
        Span::styled("[n]", Style::default().add_modifier(Modifier::BOLD).fg(Color::Red)),
        Span::raw(" Reject  "),
        Span::styled("[e]", Style::default().add_modifier(Modifier::BOLD).fg(Color::Yellow)),
        Span::raw(" Revise"),
    ]))
    .alignment(Alignment::Center);
    frame.render_widget(footer, chunks[3]);
}

/// Compute the centered rectangle for the plan approval card.
pub fn plan_approval_area(area: Rect) -> Rect {
    let width = (area.width as f32 * 0.8).min(120.0).max(40.0) as u16;
    let height = (area.height as f32 * 0.7).min(40.0).max(12.0) as u16;
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}
