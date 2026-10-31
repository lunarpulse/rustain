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

use super::hyperlink;
use super::modal::{ModalPart, fit_modal_indexed, modal_area, wrapped};
use crate::adapters::tui::login_ui::safe_url;
use crate::adapters::tui::state::{
    FieldKind, LOGIN_PICKER_ROWS, LoginOverlayState, LoginSection, LoginView, OverlayMode,
    Validation,
};
use crate::adapters::tui::theme::Theme;

/// Provider-supplied text, one sanitized line per source line.
fn clean(text: &str) -> String {
    text.split('\n')
        .map(crate::adapters::auth::bounded_provider_text)
        .collect::<Vec<_>>()
        .join("\n")
}

struct Styles {
    muted: Style,
    accent: Style,
    text: Style,
}

/// One visible URL row of a link, tied to its source part/row.
struct LinkRow {
    source: (usize, usize),
    prefix: usize,
    width: usize,
    visible: Option<Rect>,
}

struct Link {
    url: String,
    rows: Vec<LinkRow>,
}

/// The rows of one `<label><url>` block and whether the URL had to wrap.
struct LinkBlock {
    lines: Vec<Line<'static>>,
    wrapped: bool,
}

/// The displayed value without its leading `scheme://`.
fn without_scheme(url: &str) -> &str {
    url.split_once("://").map_or(url, |(_, rest)| rest)
}

/// A plain-mode value some terminal detector could still turn into a (partial)
/// link: it embeds an `http://` or `https://` (A7), or it starts with a
/// scheme-less `www.` or `ftp.`, which GNOME Terminal's detector links by default
/// (O3). Such a wrapped value shows its label only.
fn detector_bait(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("http://")
        || lower.contains("https://")
        || lower.starts_with("www.")
        || lower.starts_with("ftp.")
}

/// A `<label><url>` block. A safe URL that fits one row (with a blank cell left
/// before the border) reads `label value`; otherwise it is the label row and the
/// full value hard-wrapped into unindented rows, every row linked to the full
/// URL. Without OSC 8 a wrapped value drops its scheme, so no terminal can turn
/// a partial row into a broken link; a value that still embeds a scheme or starts
/// with `www.`/`ftp.` then shows its label only. `source` is the part and row of the block's first line.
fn link_block(
    label: &str,
    raw: &str,
    inner_width: usize,
    osc8: bool,
    styles: &Styles,
    links: &mut Vec<Link>,
    source: (usize, usize),
) -> LinkBlock {
    let Some(url) = safe_url(raw) else {
        return LinkBlock {
            lines: vec![Line::from(vec![
                Span::styled(label.to_owned(), styles.muted),
                Span::styled(clean(raw), styles.text),
            ])],
            wrapped: false,
        };
    };
    let (part, first_row) = source;
    let room = inner_width.saturating_sub(1).max(1);
    let prefix = label.width();
    let count = url.chars().count();
    if prefix + count <= room {
        let line = Line::from(vec![
            Span::styled(label.to_owned(), styles.muted),
            Span::styled(url.clone(), styles.accent),
        ]);
        links.push(Link {
            url,
            rows: vec![LinkRow {
                source: (part, first_row),
                prefix,
                width: count,
                visible: None,
            }],
        });
        return LinkBlock {
            lines: vec![line],
            wrapped: false,
        };
    }
    let mut lines = vec![Line::from(Span::styled(
        label.trim_end().to_owned(),
        styles.muted,
    ))];
    let shown = if osc8 { &url } else { without_scheme(&url) };
    let mut rows = Vec::new();
    if osc8 || !detector_bait(shown) {
        let chars: Vec<char> = shown.chars().collect();
        for (index, chunk) in chars.chunks(room).enumerate() {
            rows.push(LinkRow {
                source: (part, first_row + 1 + index),
                prefix: 0,
                width: chunk.len(),
                visible: None,
            });
            lines.push(Line::from(Span::styled(
                chunk.iter().collect::<String>(),
                styles.accent,
            )));
        }
    }
    if !rows.is_empty() {
        links.push(Link { url, rows });
    }
    LinkBlock {
        lines,
        wrapped: true,
    }
}

/// Draw the modal and decorate only retained URL cells inside this frame.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    state: &LoginOverlayState,
    theme: &Theme,
    supports_osc8: bool,
) {
    if !state.active {
        return;
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
        LoginView::ProviderPicker => (
            match state.mode {
                OverlayMode::Login => " Sign in ",
                OverlayMode::Logout => " Log out ",
                OverlayMode::Accounts => " Accounts ",
            }
            .to_string(),
            picker(state, inner_width, &styles),
        ),
        LoginView::AccountPicker => account_step(state, inner_width, &styles),
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
                running(state, inner_width, &styles, supports_osc8, &mut links),
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
    // `wanted` is unbounded (a provider URL wraps over as many rows as it needs):
    // clamp to the viewport while still a usize, then convert.
    let height = (wanted + 2)
        .min(area.height.saturating_sub(2) as usize)
        .max(8.min(area.height) as usize) as u16;
    let modal = modal_area(area, width, height);
    frame.render_widget(Clear, modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.accent))
        .title(Span::styled(title, theme.typography.heading));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    let fitted = fit_modal_indexed(parts, inner.height as usize, inner.width as usize);
    let lines: Vec<_> = fitted
        .into_iter()
        .enumerate()
        .map(|(row, (part, source_row, line))| {
            let row_link = links
                .iter_mut()
                .flat_map(|link| link.rows.iter_mut())
                .find(|link| link.source == (part, source_row));
            if supports_osc8
                && let Some(link) = row_link
                && link.prefix < inner.width as usize
            {
                link.visible = Some(Rect::new(
                    inner.x + link.prefix as u16,
                    inner.y + row as u16,
                    link.width.min(inner.width as usize - link.prefix) as u16,
                    1,
                ));
            }
            line
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
    for link in links {
        for row in link.rows {
            if let Some(visible) = row.visible {
                hyperlink::encode(frame.buffer_mut(), visible, &link.url);
            }
        }
    }
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
            // Logout and accounts rows carry no validation marker (no check runs).
            let status = match (&row.row.source, state.mode) {
                (Some(_), OverlayMode::Login) => {
                    let validation = match row.validation {
                        Validation::Checking => "checking…",
                        Validation::Valid => "● logged in",
                        Validation::Invalid => "✘ invalid",
                    };
                    format!("{validation} · {}", row.row.status)
                }
                _ => row.row.status.clone(),
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
    let action = match state.mode {
        OverlayMode::Login => "sign in",
        OverlayMode::Logout | OverlayMode::Accounts => "choose",
    };
    let escape = if state.query.is_empty() {
        "close"
    } else {
        "clear"
    };
    let footer = format!("↑↓ navigate · Enter {action} · Esc {escape}");
    vec![
        ModalPart::Header(header),
        ModalPart::Rows {
            lines: rows,
            selected,
        },
        ModalPart::Footer(wrapped(&footer, styles.muted, width)),
    ]
}

/// One unwrapped line that keeps its column spacing, cut to `width` cells
/// with `…`.
fn single_line(text: &str, style: Style, width: usize) -> Line<'static> {
    if text.width() <= width {
        return Line::from(Span::styled(text.to_owned(), style));
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cells = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cells + 1 > width {
            break;
        }
        out.push(ch);
        used += cells;
    }
    out.push('…');
    Line::from(Span::styled(out, style))
}

/// The `/logout` / `/accounts` account step: the `auth accounts` header, at
/// most ten rows around the cursor, an optional notice and the footer.
fn account_step(
    state: &LoginOverlayState,
    width: usize,
    styles: &Styles,
) -> (String, Vec<ModalPart>) {
    let Some(step) = &state.account else {
        return (String::new(), Vec::new());
    };
    let logout = state.mode == OverlayMode::Logout;
    let Some(listing) = &step.listing else {
        let title = if logout { " Log out " } else { " Accounts " };
        // Esc goes back to the provider step when the step came from it.
        let footer = if step.from_picker {
            "Esc back"
        } else {
            "Esc close"
        };
        return (
            title.to_string(),
            vec![
                ModalPart::Rows {
                    lines: wrapped("Loading accounts…", styles.muted, width),
                    selected: (0, 0),
                },
                ModalPart::Footer(wrapped(footer, styles.muted, width)),
            ],
        );
    };
    let display = clean(&listing.display);
    let title = if logout {
        format!(" Log out of {display} ")
    } else {
        format!(" {display} accounts ")
    };
    let mut rows = Vec::new();
    let mut selected = (0, 0);
    if listing.rows.is_empty() {
        rows.extend(wrapped("No stored accounts", styles.muted, width));
    }
    let len = listing.rows.len();
    let start = step
        .selected
        .saturating_sub(LOGIN_PICKER_ROWS / 2)
        .min(len.saturating_sub(LOGIN_PICKER_ROWS));
    for (position, row) in listing
        .rows
        .iter()
        .enumerate()
        .skip(start)
        .take(LOGIN_PICKER_ROWS)
    {
        let chosen = position == step.selected;
        if chosen {
            selected.0 = rows.len();
        }
        rows.push(single_line(
            &format!("{}{}", if chosen { "▸ " } else { "  " }, row.text),
            if chosen { styles.accent } else { styles.text },
            width,
        ));
        if chosen {
            selected.1 = rows.len();
        }
    }
    let footer = if step.busy {
        "Working…".to_string()
    } else {
        format!(
            "↑↓ navigate · Enter {} · Esc {}",
            if logout { "log out" } else { "make active" },
            if step.from_picker { "back" } else { "close" }
        )
    };
    let notice = step
        .notice
        .as_deref()
        .map(|notice| wrapped(notice, styles.accent, width))
        .unwrap_or_default();
    (
        title,
        vec![
            ModalPart::Header(wrapped(&listing.header, styles.text, width)),
            ModalPart::Notice(notice),
            ModalPart::Rows {
                lines: rows,
                selected,
            },
            ModalPart::Footer(wrapped(&footer, styles.muted, width)),
        ],
    )
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
    osc8: bool,
    links: &mut Vec<Link>,
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
                // The lead is the next part; both link headers follow it.
                let sign_in_part = parts.len() + 1;
                let sign_in = link_block(
                    "Sign-in link: ",
                    url,
                    width,
                    osc8,
                    styles,
                    links,
                    (sign_in_part, 0),
                );
                let local = link_block(
                    "Local shortcut (this machine only): ",
                    launch_url,
                    width,
                    osc8,
                    styles,
                    links,
                    (sign_in_part + 1, 0),
                );
                let lead = match (osc8 || !sign_in.wrapped, running.field.is_some()) {
                    (true, _) => {
                        "Continue in your browser. If it didn't open, open or copy the link below."
                    }
                    (false, true) => {
                        "Continue in your browser. If it didn't open, press Alt+C to copy the full sign-in link."
                    }
                    (false, false) => {
                        "Continue in your browser. If it didn't open, press c to copy the full sign-in link."
                    }
                };
                parts.push(ModalPart::Notice(wrapped(lead, styles.text, width)));
                parts.push(ModalPart::Header(sign_in.lines));
                parts.push(ModalPart::Header(local.lines));
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
                let source = (parts.len(), code.len());
                code.extend(
                    link_block(
                        "Sign-in page: ",
                        verification_url,
                        width,
                        osc8,
                        styles,
                        links,
                        source,
                    )
                    .lines,
                );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::state::{LoginOrigin, PendingField, RunningLogin};
    use crate::domain::events::LoginReply;
    use tokio_util::sync::CancellationToken;

    const LONG: &str = "https://claude.ai/oauth/authorize?client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e&response_type=code&redirect_uri=http%3A%2F%2Flocalhost%3A43123%2Fcallback&scope=org%3Acreate_api_key+user%3Aprofile&code_challenge=abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG&code_challenge_method=S256&state=0123456789abcdef&code=true";

    fn styles() -> Styles {
        let style = Style::default();
        Styles {
            muted: style,
            accent: style,
            text: style,
        }
    }
    fn text(lines: &[Line<'static>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }
    fn running_state(sections: Vec<LoginSection>, field: bool) -> LoginOverlayState {
        let mut state = LoginOverlayState::default();
        state.active = true;
        state.view = LoginView::Running;
        let (tx, _rx) = tokio::sync::oneshot::channel();
        state.running = Some(RunningLogin {
            login_id: 1,
            provider_id: "example".into(),
            display: "Example".into(),
            origin: LoginOrigin::Slash,
            cancel: CancellationToken::new(),
            sections,
            progress: vec!["Waiting".into()],
            field: field
                .then(|| PendingField::text(FieldKind::Manual, "Paste:".into(), LoginReply(tx))),
        });
        state
    }
    fn browser(url: &str, launch: &str, field: bool) -> LoginOverlayState {
        running_state(
            vec![LoginSection::AuthUrl {
                url: url.into(),
                launch_url: launch.into(),
                instructions: String::new(),
            }],
            field,
        )
    }

    #[test]
    fn one_row_link_keeps_label_and_scheme() {
        let mut links = Vec::new();
        let url = "https://a.test/x";
        let block = link_block(
            "Sign-in link: ",
            url,
            40,
            false,
            &styles(),
            &mut links,
            (3, 0),
        );
        assert!(!block.wrapped);
        assert_eq!(text(&block.lines), [format!("Sign-in link: {url}")]);
        assert_eq!(links[0].rows.len(), 1);
        assert_eq!(links[0].rows[0].source, (3, 0));
        assert_eq!(links[0].rows[0].prefix, "Sign-in link: ".len());
        assert_eq!(links[0].rows[0].width, url.len());
    }

    #[test]
    fn value_filling_exactly_the_room_stays_one_row_and_one_more_cell_wraps() {
        let label = "Sign-in link: ";
        let inner = 40;
        let fill = inner - 1 - label.len();
        let url = format!(
            "https://a.test/{}",
            "x".repeat(fill - "https://a.test/".len())
        );
        assert_eq!(url.len(), fill);
        let mut links = Vec::new();
        let block = link_block(label, &url, inner, false, &styles(), &mut links, (0, 0));
        assert!(!block.wrapped);
        assert_eq!(block.lines.len(), 1);
        let longer = format!("{url}x");
        let block = link_block(
            label,
            &longer,
            inner,
            false,
            &styles(),
            &mut Vec::new(),
            (0, 0),
        );
        assert!(block.wrapped);
        assert_eq!(block.lines[0].to_string(), "Sign-in link:");
    }

    #[test]
    fn wrapped_rows_rejoin_to_the_full_value_with_osc8_and_drop_the_scheme_without() {
        for (osc8, expected) in [
            (true, LONG.to_owned()),
            (false, LONG.strip_prefix("https://").unwrap().to_owned()),
        ] {
            let mut links = Vec::new();
            let block = link_block(
                "Sign-in link: ",
                LONG,
                74,
                osc8,
                &styles(),
                &mut links,
                (1, 0),
            );
            assert!(block.wrapped);
            let rows = text(&block.lines);
            assert_eq!(rows[0], "Sign-in link:");
            assert_eq!(rows[1..].concat(), expected);
            assert!(rows[1..].iter().all(|row| row.chars().count() <= 73));
            assert!(rows.iter().all(|row| !row.contains('…')));
            assert_eq!(links.len(), 1);
            assert_eq!(links[0].url, LONG);
            let sources: Vec<_> = links[0].rows.iter().map(|row| row.source).collect();
            let wanted: Vec<_> = (1..rows.len()).map(|row| (1, row)).collect();
            assert_eq!(sources, wanted);
        }
    }

    #[test]
    fn embedded_scheme_shows_the_label_only_without_osc8() {
        let url = format!("https://a.test/?next=https://b.test/{}", "y".repeat(80));
        let mut links = Vec::new();
        let plain = link_block(
            "Sign-in page: ",
            &url,
            60,
            false,
            &styles(),
            &mut links,
            (0, 0),
        );
        assert!(plain.wrapped);
        assert_eq!(text(&plain.lines), ["Sign-in page:"]);
        assert!(links.is_empty());
        let linked = link_block(
            "Sign-in page: ",
            &url,
            60,
            true,
            &styles(),
            &mut links,
            (0, 0),
        );
        assert_eq!(text(&linked.lines[1..]).concat(), url);
    }

    #[test]
    fn unsafe_value_stays_one_unlinked_line() {
        let mut links = Vec::new();
        let block = link_block(
            "Sign-in link: ",
            "javascript:alert(1)",
            40,
            true,
            &styles(),
            &mut links,
            (0, 0),
        );
        assert!(!block.wrapped && links.is_empty() && block.lines.len() == 1);
    }

    /// AC4 depends on this layout: at most one `Rows` part (the field) and every
    /// link row inside a `Header`, so `fit_modal_indexed` keeps a link's rows all
    /// or none.
    #[test]
    fn running_layout_keeps_every_link_row_in_a_header_with_at_most_one_rows_part() {
        let browser_and_device = vec![
            LoginSection::AuthUrl {
                url: LONG.into(),
                launch_url: "http://localhost:43123/launch".into(),
                instructions: "Sign in.".into(),
            },
            LoginSection::Device {
                verification_url: LONG.into(),
                user_code: "ABCD-EFGH".into(),
                instructions: "Then wait.".into(),
            },
        ];
        for field in [false, true] {
            for osc8 in [false, true] {
                let state = running_state(browser_and_device.clone(), field);
                let mut links = Vec::new();
                let parts = running(&state, 74, &styles(), osc8, &mut links);
                let rows = parts
                    .iter()
                    .filter(|part| matches!(part, ModalPart::Rows { .. }))
                    .count();
                assert_eq!(rows, usize::from(field));
                let link_rows: Vec<_> = links.iter().flat_map(|link| &link.rows).collect();
                assert!(!link_rows.is_empty());
                for row in link_rows {
                    assert!(
                        matches!(parts[row.source.0], ModalPart::Header(_)),
                        "{:?} is not in a Header",
                        row.source
                    );
                }
            }
        }
    }

    #[test]
    fn lead_names_the_copy_key_only_for_a_wrapped_link_without_osc8() {
        let lead = |state: &LoginOverlayState, osc8: bool| {
            let parts = running(state, 74, &styles(), osc8, &mut Vec::new());
            let ModalPart::Notice(lines) = &parts[0] else {
                panic!("the lead is a notice");
            };
            text(lines).join(" ")
        };
        let long = browser(LONG, "http://localhost:43123/launch", true);
        assert_eq!(
            lead(&long, false),
            "Continue in your browser. If it didn't open, press Alt+C to copy the full sign-in link."
        );
        let unfocused = browser(LONG, "http://localhost:43123/launch", false);
        assert_eq!(
            lead(&unfocused, false),
            "Continue in your browser. If it didn't open, press c to copy the full sign-in link."
        );
        let original = "Continue in your browser. If it didn't open, open or copy the link below.";
        assert_eq!(lead(&long, true), original);
        let short = browser("https://a.test/x", "http://localhost:43123/launch", true);
        assert_eq!(lead(&short, false), original);
    }

    #[test]
    fn www_and_ftp_remainders_show_the_label_only_without_osc8() {
        for host in [
            "www.example.com",
            "WWW.Example.com",
            "ftp.example.com",
            "FTP.example.com",
        ] {
            let url = format!("https://{host}/{}", "p".repeat(120));
            let mut links = Vec::new();
            let plain = link_block(
                "Sign-in link: ",
                &url,
                60,
                false,
                &styles(),
                &mut links,
                (0, 0),
            );
            assert!(plain.wrapped, "{host}");
            assert_eq!(text(&plain.lines), ["Sign-in link:"], "{host}");
            assert!(links.is_empty(), "{host}");
            let linked = link_block(
                "Sign-in link: ",
                &url,
                60,
                true,
                &styles(),
                &mut links,
                (0, 0),
            );
            assert_eq!(
                text(&linked.lines[1..]).concat(),
                safe_url(&url).unwrap(),
                "{host}"
            );
            // A value that fits one row keeps its scheme and a non-www host still wraps.
            let short = format!("https://{host}/x");
            let one = link_block(
                "Sign-in link: ",
                &short,
                60,
                false,
                &styles(),
                &mut Vec::new(),
                (0, 0),
            );
            assert_eq!(
                text(&one.lines),
                [format!("Sign-in link: {}", safe_url(&short).unwrap())]
            );
        }
        let other = format!("https://claude.ai/{}", "p".repeat(120));
        let block = link_block(
            "Sign-in link: ",
            &other,
            60,
            false,
            &styles(),
            &mut Vec::new(),
            (0, 0),
        );
        assert!(text(&block.lines).len() > 2);
    }

    #[test]
    fn the_detector_predicate_is_case_insensitive() {
        for text in [
            "WWW.x.test/a",
            "Ftp.x.test",
            "www.x",
            "a/https://b",
            "A/HTTP://b",
        ] {
            assert!(detector_bait(text), "{text}");
        }
        for text in ["claude.ai/oauth", "x.test/www.a", "wwwx.test", "ftpx.test"] {
            assert!(!detector_bait(text), "{text}");
        }
    }

    #[test]
    fn an_oversized_value_clamps_the_modal_to_the_viewport() {
        let url = format!("https://claude.ai/{}", "p".repeat(37 * 70_000));
        let state = browser(&url, "http://localhost:43123/launch", true);
        for osc8 in [false, true] {
            let backend = ratatui::backend::TestBackend::new(40, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            let theme = Theme::dark();
            terminal
                .draw(|f| render(f, f.area(), &state, &theme, osc8))
                .unwrap();
        }
    }
}
