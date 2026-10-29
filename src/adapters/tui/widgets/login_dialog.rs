//! The sign-in overlay: provider picker, method picker and the running login
//! (browser link or device code, progress, one pending field, copy footer).
//!
//! MIT License
//!
//! Copyright (c) 2025 Mario Zechner
//! Copyright (c) 2025-2026 Can Bölük
//! Copyright (c) 2026 Stencil Labs, Inc.
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::hyperlink::LinkSpan;
use super::modal::{ModalPart, fit_modal, modal_area, wrapped};
use crate::adapters::tui::login_ui::safe_url;
use crate::adapters::tui::state::{
    FieldKind, LOGIN_PICKER_ROWS, LoginOverlayState, LoginSection, LoginView, Validation,
};
use crate::adapters::tui::theme::Theme;

/// Provider-supplied text, one sanitized line per source line.
fn clean(text: &str) -> String {
    text.split('\n')
        .map(crate::adapters::auth::bounded_provider_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// `head…tail` in at most `width` cells (URLs are ASCII once re-serialized).
fn middle_truncate(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width || width < 3 {
        return text.chars().take(width.max(1)).collect();
    }
    let keep = width - 1;
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let mut out: String = text.chars().take(head).collect();
    out.push('…');
    out.extend(text.chars().skip(count - tail));
    out
}

struct Styles {
    muted: Style,
    accent: Style,
    text: Style,
}

/// A `<label><url>` line; a safe URL becomes a link over its visible part.
fn link_line(
    label: &str,
    raw: &str,
    width: usize,
    styles: &Styles,
    links: &mut Vec<(Line<'static>, usize, String, String)>,
) -> Line<'static> {
    let Some(url) = safe_url(raw) else {
        return Line::from(vec![
            Span::styled(label.to_owned(), styles.muted),
            Span::styled(clean(raw), styles.text),
        ]);
    };
    let prefix = label.width();
    let visible = middle_truncate(&url, width.saturating_sub(prefix));
    let line = Line::from(vec![
        Span::styled(label.to_owned(), styles.muted),
        Span::styled(visible.clone(), styles.accent),
    ]);
    links.push((line.clone(), prefix, visible, url));
    line
}

/// Draws the overlay; returns the links to write over the drawn frame.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    state: &LoginOverlayState,
    theme: &Theme,
) -> Vec<LinkSpan> {
    if !state.active {
        return Vec::new();
    }
    let styles = Styles {
        muted: Style::default().fg(theme.colors.fg_muted),
        accent: Style::default()
            .fg(theme.colors.accent)
            .add_modifier(Modifier::BOLD),
        text: Style::default().fg(theme.colors.fg_primary),
    };
    let width = area.width.saturating_sub(4).clamp(40, 100).min(area.width);
    let inner_width = width.saturating_sub(2) as usize;
    let mut links = Vec::new();
    let (title, parts) = match state.view {
        LoginView::ProviderPicker => (" Sign in ".to_string(), picker(state, inner_width, &styles)),
        LoginView::MethodPicker => {
            let display = state.target.as_ref().map_or("", |(_, display)| display);
            (
                format!(" Sign in to {} ", clean(display)),
                methods(state, inner_width, &styles),
            )
        }
        LoginView::Running => {
            let display = state
                .running
                .as_ref()
                .map_or("", |running| &running.display);
            (
                format!(" Sign in to {display} "),
                running(state, inner_width, &styles, &mut links),
            )
        }
    };
    let wanted: usize = parts
        .iter()
        .map(|part| match part {
            ModalPart::Header(lines) | ModalPart::Notice(lines) | ModalPart::Footer(lines) => {
                lines.len()
            }
            ModalPart::Rows { lines, .. } => lines.len(),
        })
        .sum();
    let height = (wanted as u16 + 2)
        .min(area.height.saturating_sub(2))
        .max(8.min(area.height));
    let modal = modal_area(area, width, height);
    frame.render_widget(Clear, modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.accent))
        .title(Span::styled(title, theme.typography.heading));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    let lines = fit_modal(parts, inner.height as usize, inner.width as usize);
    let spans = links
        .into_iter()
        .filter_map(|(line, prefix, text, url)| {
            let row = lines.iter().position(|drawn| *drawn == line)?;
            Some(LinkSpan {
                x: inner.x + prefix as u16,
                y: inner.y + row as u16,
                text,
                url,
            })
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).style(
            Style::default()
                .fg(theme.colors.fg_primary)
                .bg(theme.colors.bg_surface),
        ),
        inner,
    );
    spans
}

fn picker(state: &LoginOverlayState, width: usize, styles: &Styles) -> Vec<ModalPart> {
    let header = if state.query.is_empty() {
        wrapped("Type to search", styles.muted, width)
    } else {
        wrapped(&format!("Search: {}▏", state.query), styles.accent, width)
    };
    let mut rows = Vec::new();
    let mut selected = (0, 0);
    if state.loading {
        rows.extend(wrapped("Loading providers…", styles.muted, width));
    } else if state.filtered.is_empty() {
        rows.extend(wrapped(
            &format!("No providers match \"{}\"", state.query),
            styles.muted,
            width,
        ));
    } else {
        // A window of at most ten rows, centered on the selection.
        let len = state.filtered.len();
        let start = state
            .selected
            .saturating_sub(LOGIN_PICKER_ROWS / 2)
            .min(len.saturating_sub(LOGIN_PICKER_ROWS));
        for (position, index) in state
            .filtered
            .iter()
            .enumerate()
            .skip(start)
            .take(LOGIN_PICKER_ROWS)
        {
            let Some(row) = state.rows.get(*index) else {
                continue;
            };
            let status = match &row.row.source {
                None => "not signed in".to_string(),
                Some(source) => {
                    let validation = match row.validation {
                        Validation::Checking => "checking…",
                        Validation::Valid => "● logged in",
                        Validation::Invalid => "✘ invalid",
                    };
                    let mut status = format!("{validation} · {source}");
                    if row.row.accounts > 1 {
                        status.push_str(&format!(" · {} accounts", row.row.accounts));
                    }
                    status
                }
            };
            let chosen = position == state.selected;
            if chosen {
                selected.0 = rows.len();
            }
            rows.extend(wrapped(
                &format!(
                    "{}{} · {status}",
                    if chosen { "▸ " } else { "  " },
                    row.row.display
                ),
                if chosen { styles.accent } else { styles.text },
                width,
            ));
            if chosen {
                selected.1 = rows.len();
            }
        }
    }
    let footer = if state.query.is_empty() {
        "↑↓ navigate · Enter sign in · Esc close"
    } else {
        "↑↓ navigate · Enter sign in · Esc clear"
    };
    vec![
        ModalPart::Header(header),
        ModalPart::Rows {
            lines: rows,
            selected,
        },
        ModalPart::Footer(wrapped(footer, styles.muted, width)),
    ]
}

fn methods(state: &LoginOverlayState, width: usize, styles: &Styles) -> Vec<ModalPart> {
    let mut rows = Vec::new();
    let mut selected = (0, 0);
    for (index, method) in state.methods.iter().enumerate() {
        let chosen = index == state.method_selected;
        if chosen {
            selected.0 = rows.len();
        }
        rows.extend(wrapped(
            &format!("{}{}", if chosen { "▸ " } else { "  " }, method.label()),
            if chosen { styles.accent } else { styles.text },
            width,
        ));
        if chosen {
            selected.1 = rows.len();
        }
    }
    let footer = if state.method_from_picker {
        "↑↓ navigate · Enter select · Esc back"
    } else {
        "↑↓ navigate · Enter select · Esc close"
    };
    vec![
        ModalPart::Header(wrapped("Choose a login method:", styles.muted, width)),
        ModalPart::Rows {
            lines: rows,
            selected,
        },
        ModalPart::Footer(wrapped(footer, styles.muted, width)),
    ]
}

fn running(
    state: &LoginOverlayState,
    width: usize,
    styles: &Styles,
    links: &mut Vec<(Line<'static>, usize, String, String)>,
) -> Vec<ModalPart> {
    let Some(running) = &state.running else {
        return Vec::new();
    };
    let mut parts = Vec::new();
    let mut browser = false;
    let mut device = false;
    for section in &running.sections {
        match section {
            LoginSection::AuthUrl {
                url,
                launch_url,
                instructions,
            } => {
                browser = true;
                parts.push(ModalPart::Notice(wrapped(
                    "Continue in your browser. If it didn't open, open or copy the link below.",
                    styles.text,
                    width,
                )));
                parts.push(ModalPart::Header(vec![
                    link_line("Sign-in link: ", url, width, styles, links),
                    link_line(
                        "Local shortcut (this machine only): ",
                        launch_url,
                        width,
                        styles,
                        links,
                    ),
                ]));
                parts.push(ModalPart::Notice(wrapped(
                    &clean(instructions),
                    styles.muted,
                    width,
                )));
            }
            LoginSection::Device {
                verification_url,
                user_code,
                instructions,
            } => {
                device = true;
                parts.push(ModalPart::Notice(wrapped(
                    "Enter this code on the sign-in page to connect your account.",
                    styles.text,
                    width,
                )));
                let mut code = wrapped(&clean(user_code), styles.accent, width);
                code.push(link_line(
                    "Sign-in page: ",
                    verification_url,
                    width,
                    styles,
                    links,
                ));
                parts.push(ModalPart::Header(code));
                parts.push(ModalPart::Notice(wrapped(
                    &clean(instructions),
                    styles.muted,
                    width,
                )));
            }
        }
    }
    let mut progress = Vec::new();
    for (index, message) in running.progress.iter().enumerate() {
        let glyph = if index + 1 == running.progress.len() {
            "› "
        } else {
            "✓ "
        };
        progress.extend(wrapped(
            &format!("{glyph}{}", clean(message)),
            styles.muted,
            width,
        ));
    }
    if progress.is_empty() && browser {
        progress.extend(wrapped(
            "Waiting for you to finish in the browser…",
            styles.muted,
            width,
        ));
    }
    parts.push(ModalPart::Notice(progress));
    let field = running.field.as_ref();
    if let Some(field) = field {
        let mut lines = wrapped(&clean(&field.prompt), styles.text, width);
        let room = width.saturating_sub(2).max(1);
        let shown = match field.kind {
            FieldKind::Line => format!("{}▏", field.buffer.visible()),
            FieldKind::Secret | FieldKind::Manual => {
                let count = field.buffer.char_count();
                if count > room {
                    format!("{}…", "•".repeat(room - 1))
                } else {
                    "•".repeat(count)
                }
            }
        };
        let field_line = lines.len();
        lines.push(Line::from(vec![
            Span::styled("> ", styles.accent),
            Span::styled(shown, styles.text),
        ]));
        parts.push(ModalPart::Rows {
            selected: (field_line, field_line + 1),
            lines,
        });
    }
    let footer = match (field.is_some(), device) {
        (false, false) => "c copy link · l copy local link · Esc cancel",
        (false, true) => "c copy code · u copy link · Esc cancel",
        (true, false) if browser => {
            "Enter submit · Alt+C copy link · Alt+L copy local link · Esc cancel"
        }
        (true, false) => "Enter submit · Esc cancel",
        (true, true) => "Enter submit · Alt+C copy code · Alt+U copy link · Esc cancel",
    };
    let footer = if !browser && !device && field.is_none() {
        "Esc cancel"
    } else {
        footer
    };
    parts.push(ModalPart::Footer(wrapped(footer, styles.muted, width)));
    parts
}
