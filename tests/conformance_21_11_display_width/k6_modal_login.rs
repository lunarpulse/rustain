//! K6 (Story 21.11, AC6): modal/login/permission display-width preservation.
//!
//! Everything enters through the public login renderer and permission row builder,
//! and reads the painted `Buffer` back. The indexed fitter (`modal::fit_modal_indexed`, budget
//! 3 and 0) is `pub(crate)` and is forced from the lib unit tests
//! `adapters::tui::widgets::modal::tests::display_width_*`.
//!
//! The 20-9j positive controls here (full URL rows, a link on every row,
//! targets, scheme-less plain rows) are a small re-assertion through the same
//! public render; the complete 20-9j suite stays in
//! `conformance_20_9j_sign_in_link`.
use super::support::*;

use ratatui::buffer::Buffer;
use rustain::adapters::tui::state::{
    AccountStep, FieldKind, LoginOrigin, LoginOverlayState, LoginSection, LoginView, OverlayMode,
    PendingField, RunningLogin,
};
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::login_dialog;
use rustain::adapters::tui::widgets::permission_prompt::{ExactInput, PermissionCard, card_rows};
use rustain::domain::events::{AccountListRow, AccountListing, LoginReply};
use rustain::domain::models::ToolRisk;
use rustain::domain::models::tool_call::ApprovalSource;
use tokio_util::sync::CancellationToken;
use unicode_width::UnicodeWidthStr;

const LOCAL: &str = "http://localhost:43123/launch";
/// A production-shaped OAuth authorize URL; ASCII, already in `url::Url` form.
const LONG: &str = "https://claude.ai/oauth/authorize?client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e&response_type=code&redirect_uri=http%3A%2F%2Flocalhost%3A43123%2Fcallback&scope=org%3Acreate_api_key+user%3Aprofile&code_challenge=abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG&code_challenge_method=S256&state=0123456789abcdef&code=true";

fn x(n: usize) -> String {
    "x".repeat(n)
}

/// The text between the modal's side borders of one row, if it is a modal row.
fn inner(row: &str) -> Option<&str> {
    row.trim().strip_prefix('│')?.strip_suffix('│')
}

/// Every modal inner row of a painted buffer, in order.
fn inner_rows(buffer: &Buffer) -> Vec<String> {
    rows(buffer)
        .iter()
        .filter_map(|row| inner(row).map(str::to_owned))
        .collect()
}

// ── account picker ────────────────────────────────────────────────────────

fn account_state(texts: &[&str], selected: usize, notice: Option<&str>) -> LoginOverlayState {
    let mut state = LoginOverlayState::default();
    state.open_accounts(OverlayMode::Accounts, "anthropic".into(), false);
    state.account = Some(AccountStep {
        provider: "anthropic".into(),
        listing: Some(AccountListing {
            provider: "anthropic".into(),
            display: "Anthropic".into(),
            header: "Anthropic (anthropic): signed in".into(),
            rows: texts
                .iter()
                .enumerate()
                .map(|(index, text)| AccountListRow {
                    id: index as u64 + 1,
                    text: (*text).to_owned(),
                    served: index == 0,
                })
                .collect(),
        }),
        selected,
        from_picker: false,
        busy: false,
        notice: notice.map(str::to_owned),
    });
    state
}

fn draw_login(state: &LoginOverlayState, osc8: bool, width: u16, height: u16) -> Buffer {
    let theme = Theme::dark();
    paint(width, height, |f| {
        // Content-width controls supply their own sheet, independent of shell geometry.
        let sheet = ratatui::layout::Rect::new(
            0,
            0,
            width.saturating_sub(4).clamp(40, 100).min(width),
            height,
        );
        login_dialog::render(f, sheet, state, &theme, osc8);
    })
    .buffer
}

const FOOTER: &str = "↑↓ navigate · Enter make active · Esc close";

/// A 60-column terminal gives the account rows a 54-cell content budget after
/// the six-cell `▸ #1  ` prefix: exact fit and fit-minus-one for G, C and A.
#[test]
fn account_picker_clips_long_rows_on_whole_graphemes() {
    // (text after `#1  `, painted row after its two-cell marker)
    let cases: Vec<(String, String)> = vec![
        // ASCII control.
        (x(48), format!("#1  {}", x(48))),
        (x(49), format!("#1  {}…", x(47))),
        // G: exact fit, one over, ends on cell 53, straddles cell 53.
        (format!("{}{G}", x(46)), format!("#1  {}{G}", x(46))),
        (format!("{}{G}", x(47)), format!("#1  {}…", x(47))),
        (format!("{}{G}yy", x(45)), format!("#1  {}{G}…", x(45))),
        (format!("{}{G}yy", x(46)), format!("#1  {}…", x(46))),
        // C.
        (format!("{}{C}", x(46)), format!("#1  {}{C}", x(46))),
        (format!("{}{C}yy", x(45)), format!("#1  {}{C}…", x(45))),
        (format!("{}{C}yy", x(46)), format!("#1  {}…", x(46))),
        // A.
        (format!("{}{A}", x(47)), format!("#1  {}{A}", x(47))),
        (format!("{}{A}yy", x(46)), format!("#1  {}{A}…", x(46))),
        (format!("{}{A}yy", x(47)), format!("#1  {}…", x(47))),
    ];
    for (tail, painted_row) in cases {
        let text = format!("#1  {tail}");
        let state = account_state(&[&text, "#2  other"], 0, None);
        let buffer = draw_login(&state, false, 60, 16);
        let inner = inner_rows(&buffer);
        assert!(
            inner.iter().all(|row| row.width() == 54),
            "every modal row fills the 54-cell content width: {inner:?}"
        );
        assert_eq!(
            inner
                .iter()
                .find(|row| row.starts_with("▸ #1"))
                .map(|row| row.trim_end()),
            Some(format!("▸ {painted_row}").as_str()),
            "{tail:?}"
        );
        // The other source row and the footer survive untouched.
        assert!(inner.iter().any(|row| row.trim_end() == "  #2  other"));
        assert!(inner.iter().any(|row| row.trim_end() == FOOTER));
        assert_whole_clusters(&buffer);
        assert_no_partial_family(&screen(&buffer));
    }
}

/// A multi-line notice shrinks to one clipped line at the real content budget
/// while the header, the selected row and the footer keep their place.
#[test]
fn account_notice_shrinks_without_splitting_a_grapheme() {
    for (first, expected) in [
        // G ends on cell 53: kept, then `…` makes 54.
        (format!("{}{G}yy", x(51)), format!("{}{G}…", x(51))),
        // G would straddle cell 53: dropped whole.
        (format!("{}{G}yy", x(52)), format!("{}…", x(52))),
        (format!("{}{C}yy", x(51)), format!("{}{C}…", x(51))),
        (format!("{}{C}yy", x(52)), format!("{}…", x(52))),
        (format!("{}{A}yy", x(52)), format!("{}{A}…", x(52))),
        (format!("{}{A}yy", x(53)), format!("{}…", x(53))),
        // ASCII control.
        (format!("{}zz", x(60)), format!("{}…", x(53))),
    ] {
        let notice = format!("{first}\nsecond notice line");
        let state = account_state(&["#1  a", "#2  b", "#3  c"], 1, Some(&notice));
        let buffer = draw_login(&state, false, 60, 8);
        let texts: Vec<String> = inner_rows(&buffer)
            .iter()
            .map(|row| row.trim_end().to_owned())
            .collect();
        let shrunk = texts.get(1).expect("notice row");
        assert_eq!(shrunk, &expected, "{first:?}");
        assert!(shrunk.width() <= 54);
        assert_eq!(texts[0], "Anthropic (anthropic): signed in");
        assert_eq!(texts[2], "  #1  a");
        assert_eq!(texts[3], "▸ #2  b");
        assert_eq!(texts[4], "  #3  c");
        assert_eq!(texts[5], FOOTER);
        assert!(!texts.concat().contains("second notice line"));
        assert_whole_clusters(&buffer);
        assert_no_partial_family(&screen(&buffer));
    }
}

// ── 20-9j URL rows ────────────────────────────────────────────────────────

fn browser(url: &str) -> LoginOverlayState {
    let mut state = LoginOverlayState::default();
    state.view = LoginView::Running;
    state.running = Some(RunningLogin {
        login_id: 1,
        generation: state.generation,
        provider_id: "example".into(),
        display: "Example".into(),
        origin: LoginOrigin::Slash,
        cancel: CancellationToken::new(),
        sections: vec![LoginSection::AuthUrl {
            url: url.into(),
            launch_url: LOCAL.into(),
            instructions: String::new(),
        }],
        progress: vec!["Waiting".into()],
        field: None,
    });
    state
}

/// One painted row with its OSC 8 envelopes decoded: `(glyph, target)` per cell.
fn decode_row(buffer: &Buffer, y: u16) -> Vec<(String, Option<String>)> {
    (0..buffer.area.width)
        .map(|col| {
            let symbol = buffer[(col, y)].symbol();
            if let Some(rest) = symbol.strip_prefix("\x1b]8;;") {
                let (target, rest) = rest.split_once('\x07').expect("OSC target terminator");
                let glyph = rest.strip_suffix("\x1b]8;;\x07").expect("balanced close");
                assert_eq!(glyph.chars().count(), 1, "one glyph per envelope");
                (glyph.to_owned(), Some(target.to_owned()))
            } else {
                assert!(
                    !symbol.chars().any(char::is_control),
                    "control in {symbol:?}"
                );
                (symbol.to_owned(), None)
            }
        })
        .collect()
}

/// `(glyph text between the borders, targets of those cells)` for every modal row.
fn decoded_inner(buffer: &Buffer) -> Vec<(String, Vec<Option<String>>)> {
    (0..buffer.area.height)
        .filter_map(|y| {
            let cells = decode_row(buffer, y);
            let first = cells.iter().position(|(glyph, _)| glyph == "│")?;
            let last = cells.iter().rposition(|(glyph, _)| glyph == "│")?;
            (first < last).then(|| {
                let body = &cells[first + 1..last];
                (
                    body.iter().map(|(glyph, _)| glyph.as_str()).collect(),
                    body.iter().map(|(_, target)| target.clone()).collect(),
                )
            })
        })
        .collect()
}

/// A normalized, ASCII URL still wraps over every row it needs, each row's
/// glyphs carry the full target, and nothing else on the row does (20-9j).
#[test]
fn ascii_url_keeps_every_row_linked_to_the_full_target() {
    assert_eq!(
        rustain::adapters::tui::login_ui::safe_url(LONG).as_deref(),
        Some(LONG),
        "the control URL is already in serialized form"
    );
    // 80 columns: modal 76, inner 74, rows hard-wrapped at inner - 1 = 73.
    let buffer = draw_login(&browser(LONG), true, 80, 30);
    let inner = decoded_inner(&buffer);
    let label = inner
        .iter()
        .position(|(text, _)| text.trim_end() == "Sign-in link:")
        .expect("a wrapped link's label row stands alone");
    assert!(inner[label].1.iter().all(Option::is_none), "label unlinked");

    let url_rows = LONG.len().div_ceil(73);
    assert!(url_rows >= 3, "a genuinely wrapped control");
    let mut joined = String::new();
    for (text, targets) in &inner[label + 1..label + 1 + url_rows] {
        let chunk = text.trim_end();
        joined.push_str(chunk);
        assert!(
            text.ends_with(' '),
            "the cell before the border stays blank: {text:?}"
        );
        let linked: Vec<&Option<String>> = targets.iter().filter(|t| t.is_some()).collect();
        assert_eq!(linked.len(), chunk.chars().count(), "linked on every row");
        assert!(
            linked.iter().all(|t| t.as_deref() == Some(LONG)),
            "full target on {chunk:?}"
        );
    }
    assert_eq!(joined, LONG, "rows rejoin to the full URL");

    // The local shortcut is a one-row link: label unlinked, URL cells linked.
    let (text, targets) = inner
        .iter()
        .find(|(text, _)| text.starts_with("Local shortcut"))
        .expect("local shortcut row");
    assert!(text.trim_end().ends_with(LOCAL), "{text:?}");
    let linked: Vec<&Option<String>> = targets.iter().filter(|t| t.is_some()).collect();
    assert_eq!(linked.len(), LOCAL.chars().count());
    assert!(linked.iter().all(|t| t.as_deref() == Some(LOCAL)));
}

/// Without OSC 8 the wrapped value drops its scheme and carries no envelopes.
#[test]
fn ascii_url_without_osc8_wraps_scheme_less_and_unlinked() {
    let buffer = draw_login(&browser(LONG), false, 80, 30);
    let inner = decoded_inner(&buffer);
    assert!(
        inner
            .iter()
            .all(|(_, targets)| targets.iter().all(Option::is_none))
    );
    let label = inner
        .iter()
        .position(|(text, _)| text.trim_end() == "Sign-in link:")
        .expect("label row");
    let shown = LONG.strip_prefix("https://").expect("https control");
    let mut joined = String::new();
    for (text, _) in &inner[label + 1..] {
        if joined.len() >= shown.len() {
            break;
        }
        joined.push_str(text.trim_end());
    }
    assert_eq!(joined, shown);
}

/// A raw Unicode URL is serialized to an ASCII safe URL before it reaches the
/// URL count/chunk path, so no wide or combining glyph can enter a URL row.
#[test]
fn unicode_url_is_serialized_to_ascii_before_the_link_rows() {
    let raw = "https://例え.test/界?q=é";
    let safe = rustain::adapters::tui::login_ui::safe_url(raw).expect("safe URL");
    assert!(safe.is_ascii(), "{safe}");
    assert!(safe.starts_with("https://xn--"), "{safe}");
    assert!(safe.contains("%E7%95%8C"), "{safe}");
    assert!(safe.ends_with("?q=%C3%A9"), "{safe}");

    let buffer = draw_login(&browser(raw), true, 80, 30);
    let inner = decoded_inner(&buffer);
    let (text, targets) = inner
        .iter()
        .find(|(text, _)| text.starts_with("Sign-in link: "))
        .expect("one-row link");
    assert_eq!(text.trim_end(), format!("Sign-in link: {safe}"));
    assert!(text.is_ascii());
    let linked: Vec<&Option<String>> = targets.iter().filter(|t| t.is_some()).collect();
    assert_eq!(linked.len(), safe.chars().count());
    assert!(linked.iter().all(|t| t.as_deref() == Some(safe.as_str())));
    for ch in ['例', 'え', '界', 'é'] {
        assert!(
            !inner.iter().any(|(text, _)| text.contains(ch)),
            "{ch} reached the painted modal"
        );
    }
}

/// A masked field paints one bullet per typed scalar, never the glyphs or
/// their display width.
#[test]
fn masked_field_paints_bullets_only() {
    let mut state = browser(LONG);
    let (tx, _rx) = tokio::sync::oneshot::channel();
    let secret = format!("{C}{G}{A}");
    let mut field = PendingField::text(FieldKind::Manual, "Paste:".into(), LoginReply(tx));
    field.buffer.push_str(&secret);
    state.running.as_mut().expect("running").field = Some(field);
    let buffer = draw_login(&state, false, 80, 40);
    let inner = inner_rows(&buffer);
    let row = inner
        .iter()
        .find(|row| row.starts_with("> "))
        .expect("field row");
    assert_eq!(
        row.trim_end(),
        format!("> {}", "•".repeat(secret.chars().count()))
    );
    assert!(!screen(&buffer).contains(C));
    assert_no_partial_family(&screen(&buffer));
}

#[test]
fn story21_4_running_secret_body_and_keys_fit_minimum_and_wide_sheets() {
    let mut state = browser(LONG);
    let (tx, _rx) = tokio::sync::oneshot::channel();
    let mut field = PendingField::text(FieldKind::Secret, "API key:".into(), LoginReply(tx));
    let canary = "private-canary";
    field.buffer.push_str(canary);
    state.running.as_mut().unwrap().field = Some(field);
    let theme = Theme::dark();
    for (width, height) in [(60, 16), (130, 30)] {
        for osc8 in [false, true] {
            let painted = paint(width, height, |frame| {
                let layout = rustain::adapters::tui::layout::compute_layout(
                    frame.area(),
                    &theme,
                    "1\n2\n3\n4\n5\n6\n7\n8",
                    1,
                    false,
                    rustain::domain::models::visual::DensityMode::Focus,
                )
                .unwrap();
                let region = rustain::adapters::tui::presentation::geometry::modal_region(
                    &layout,
                    frame.area(),
                );
                let sheet = rustain::adapters::tui::presentation::geometry::frame_rect(region);
                if width == 60 {
                    assert_eq!(sheet.height, 5);
                }
                login_dialog::render(frame, sheet, &state, &theme, osc8);
                assert_eq!(frame.buffer_mut()[(sheet.x, sheet.y)].symbol(), "┌");
                assert_eq!(
                    frame.buffer_mut()[(sheet.right() - 1, sheet.bottom() - 1)].symbol(),
                    "┘"
                );
            });
            let text = screen(&painted.buffer);
            assert!(
                text.contains(&format!("> {}", "•".repeat(canary.len()))),
                "{text}"
            );
            assert!(text.contains("Enter submit"), "{text}");
            assert!(text.contains("Esc cancel"), "{text}");
            assert!(!text.contains(canary));
            assert_whole_clusters(&painted.buffer);
        }
    }
}

// ── permission rail ───────────────────────────────────────────────────────

fn permission_rows(tool_name: &str, width: u16) -> (Buffer, Vec<String>) {
    let theme = Theme::dark();
    let source = ApprovalSource::ForegroundTurn {
        conversation_id: "c1".into(),
    };
    let input = serde_json::json!({"file_path": "/w/a.txt"});
    let card = PermissionCard {
        source: &source,
        tool_name,
        risk: ToolRisk::Standard,
        input: ExactInput::Ready(&input),
    };
    let painted = paint(width, 24, |f| {
        let rows = card_rows(&card, width, &theme, true);
        f.render_widget(ratatui::widgets::Paragraph::new(rows.rail), f.area());
    });
    let text = rows(&painted.buffer)
        .into_iter()
        .filter(|row| !row.is_empty())
        .collect();
    (painted.buffer, text)
}

/// `[a] Always for [server]` is packed by painted cells for MCP tool names
/// whose server is CJK, a ZWJ family or a combining sequence.
#[test]
fn always_label_wraps_by_cells_for_mcp_server_names() {
    // (server, narrowest width at which `[y] [s] [a]` share one rail row).
    let cases = [
        ("srv".to_owned(), 44),
        (format!("{C}{C}"), 45),
        (G.to_owned(), 43),
        (A.to_owned(), 42),
    ];
    for (server, exact) in cases {
        let tool = format!("mcp__{server}__Read");
        let item = format!("[a] Always for [{server}]");

        let (buffer, painted) = permission_rows(&tool, exact);
        assert!(
            painted.contains(&format!("[y] Allow  [s] Session  {item}")),
            "{server:?} at {exact}: {painted:?}"
        );
        assert_whole_clusters(&buffer);

        let (buffer, painted) = permission_rows(&tool, exact - 1);
        assert!(
            painted.contains(&"[y] Allow  [s] Session".to_owned()),
            "{server:?} at {}: {painted:?}",
            exact - 1
        );
        assert!(
            painted.contains(&item),
            "the item wraps whole, not clipped: {server:?} at {}: {painted:?}",
            exact - 1
        );
        assert_whole_clusters(&buffer);
        assert_no_partial_family(&painted.join("\n"));
    }
}

/// A built-in tool keeps the bare `Always` label and the second rail row.
#[test]
fn always_label_for_builtin_tools_is_unchanged() {
    let (_, painted) = permission_rows("Write", 80);
    assert!(painted.contains(&"[y] Allow  [s] Session  [a] Always".to_owned()));
    assert!(painted.contains(&"[n] Deny  [f] Deny + feedback  [r] Review".to_owned()));
}
