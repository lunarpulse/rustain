//! Conformance for the complete OAuth sign-in link (20-9j): the full URL wrapped
//! over every row it needs, linked on every row with OSC 8 by default, the
//! scheme omitted from a wrapped value without OSC 8, whole-link-or-label fit,
//! the capability policy and the plain-mode lead. Every keystone reads the real
//! `login_dialog::render` buffer back; nothing is asserted from internals.
use std::{
    io::{self, Write},
    num::NonZeroU16,
    sync::{Arc, Mutex},
};

use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::{CrosstermBackend, TestBackend},
    buffer::{Buffer, CellDiffOption},
    layout::Rect,
    style::Modifier,
};
use rustain::adapters::tui::{
    app::InputAction,
    handlers::login::{self, LoginAction},
    state::{
        FieldKind, LoginOrigin, LoginOverlayState, LoginSection, LoginView, PendingField,
        RunningLogin, TuiState,
    },
    theme::Theme,
    widgets::{
        hyperlink::{supports_osc8, terminal_supports_osc8},
        login_dialog,
    },
};
use rustain::domain::events::{DomainInputEvent, LoginReply};
use tokio_util::sync::CancellationToken;

const LOCAL: &str = "http://localhost:43123/launch";
const SIGN_IN: &str = "Sign-in link:";
const SHORTCUT: &str = "Local shortcut (this machine only):";
const ORIGINAL_LEAD: &str =
    "Continue in your browser. If it didn't open, open or copy the link below.";
const ALT_LEAD: &str =
    "Continue in your browser. If it didn't open, press Alt+C to copy the full sign-in link.";
const PLAIN_LEAD: &str =
    "Continue in your browser. If it didn't open, press c to copy the full sign-in link.";

/// An authorize URL in the production `oauth_code` query order.
fn authorize(base: &str, scope: &str, extras: &[(&str, &str)]) -> String {
    let mut url = url::Url::parse(base).unwrap();
    {
        let mut query = url.query_pairs_mut();
        query.extend_pairs([
            ("client_id", "9d1c250a-e61b-44d9-88ed-5944d1962f5e"),
            ("response_type", "code"),
            ("redirect_uri", "http://localhost:43123/callback"),
            ("scope", scope),
            (
                "code_challenge",
                "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
            ),
            ("code_challenge_method", "S256"),
            ("state", "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        ]);
        for (key, value) in extras {
            query.append_pair(key, value);
        }
    }
    url.to_string()
}
fn anthropic() -> String {
    authorize(
        "https://claude.ai/oauth/authorize",
        "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers",
        &[("code", "true")],
    )
}
fn codex() -> String {
    authorize(
        "https://auth.openai.com/oauth/authorize",
        "openid profile email offline_access api.connectors.read api.connectors.invoke",
        &[
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", "omp"),
        ],
    )
}
/// A one-row URL that fills exactly `room` cells.
fn exact(room: usize) -> String {
    let head = "https://a.test/";
    format!("{head}{}", "x".repeat(room - head.len()))
}

fn browser_section(url: &str, launch: &str) -> LoginSection {
    LoginSection::AuthUrl {
        url: url.into(),
        launch_url: launch.into(),
        instructions: String::new(),
    }
}
fn running(sections: Vec<LoginSection>, field: bool, progress: bool) -> LoginOverlayState {
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
        progress: if progress {
            vec!["Listening on the callback port".into()]
        } else {
            Vec::new()
        },
        field: field.then(|| {
            PendingField::text(FieldKind::Manual, "Paste the code:".into(), LoginReply(tx))
        }),
    });
    state
}
fn browser(url: &str, field: bool) -> LoginOverlayState {
    running(vec![browser_section(url, LOCAL)], field, field)
}

fn buffer(state: &LoginOverlayState, osc8: bool, width: u16, height: u16) -> Buffer {
    let theme = Theme::dark();
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|f| login_dialog::render(f, f.area(), state, &theme, osc8))
        .unwrap();
    terminal.backend().buffer().clone()
}

/// A drawn buffer with every OSC 8 envelope decoded; any other control
/// character in a cell fails the test.
struct Frame {
    glyphs: Vec<Vec<String>>,
    targets: Vec<Vec<Option<String>>>,
}
impl Frame {
    fn new(b: &Buffer) -> Self {
        let theme = Theme::dark();
        let (mut glyphs, mut targets) = (Vec::new(), Vec::new());
        for y in 0..b.area.height {
            let (mut row, mut row_targets) = (Vec::new(), Vec::new());
            for x in 0..b.area.width {
                let cell = &b[(x, y)];
                if let Some(rest) = cell.symbol().strip_prefix("\x1b]8;;") {
                    let (target, rest) = rest.split_once('\x07').expect("OSC target terminator");
                    let glyph = rest
                        .strip_suffix("\x1b]8;;\x07")
                        .expect("balanced OSC close");
                    assert!(!target.is_empty());
                    assert_eq!(glyph.chars().count(), 1, "one glyph per envelope");
                    assert!(!glyph.chars().any(char::is_control));
                    assert_eq!(
                        cell.diff_option,
                        CellDiffOption::ForcedWidth(NonZeroU16::new(1).unwrap())
                    );
                    assert_eq!(cell.fg, theme.colors.accent, "link fg at {x},{y}");
                    assert_eq!(cell.bg, theme.colors.bg_surface, "link bg at {x},{y}");
                    assert!(
                        cell.modifier.contains(Modifier::BOLD),
                        "link bold at {x},{y}"
                    );
                    row.push(glyph.to_owned());
                    row_targets.push(Some(target.to_owned()));
                } else {
                    assert!(
                        !cell.symbol().chars().any(char::is_control),
                        "control in {:?} at {x},{y}",
                        cell.symbol()
                    );
                    row.push(cell.symbol().to_owned());
                    row_targets.push(None);
                }
            }
            glyphs.push(row);
            targets.push(row_targets);
        }
        Self { glyphs, targets }
    }
    fn rows(&self) -> Vec<String> {
        self.glyphs.iter().map(|row| row.concat()).collect()
    }
    fn envelopes(&self) -> usize {
        self.targets
            .iter()
            .flatten()
            .filter(|t| t.is_some())
            .count()
    }
    /// The rows between the modal's side borders, in order.
    fn inner(&self) -> Vec<Inner> {
        let mut found = Vec::new();
        for (y, row) in self.glyphs.iter().enumerate() {
            let first = row.iter().position(|cell| cell == "│");
            let last = row.iter().rposition(|cell| cell == "│");
            if let (Some(first), Some(last)) = (first, last)
                && first < last
            {
                found.push(Inner {
                    y,
                    x0: first + 1,
                    width: last - first - 1,
                    text: row[first + 1..last].concat(),
                });
            }
        }
        found
    }
}
struct Inner {
    y: usize,
    x0: usize,
    width: usize,
    text: String,
}

#[derive(Debug, PartialEq)]
enum Shown {
    /// Every URL row, in order (one entry for a one-row link).
    Complete(Vec<usize>),
    LabelOnly,
    Absent,
}

/// Why a link block breaks the all-or-label rule.
#[derive(Debug, PartialEq)]
enum OracleError {
    /// A strict prefix of the value was retained.
    Partial { shown: usize, of: usize },
    /// Rows of the value were retained without its first row (or out of order):
    /// a continuation fragment under a label-only link.
    ContinuationFragment(String),
}

/// Reads one link back: a one-row `label value`, or the label row alone
/// followed by rows that rejoin to `display`. Never a strict subset, and never a
/// retained continuation fragment under a label-only link.
fn read_link(inner: &[Inner], label: &str, display: &str, one_row: bool) -> Shown {
    try_read_link(inner, label, display, one_row)
        .unwrap_or_else(|error| panic!("{label} breaks the all-or-label rule: {error:?}"))
}

fn try_read_link(
    inner: &[Inner],
    label: &str,
    display: &str,
    one_row: bool,
) -> Result<Shown, OracleError> {
    let heads: Vec<&Inner> = inner
        .iter()
        .filter(|row| row.text.starts_with(label))
        .collect();
    assert!(heads.len() <= 1, "one {label:?} row, found {}", heads.len());
    let Some(head) = heads.first() else {
        return Ok(Shown::Absent);
    };
    assert!(!head.text.contains('…'), "no ellipsis in {:?}", head.text);
    assert_eq!(
        head.text.chars().count(),
        head.width,
        "inner rows are padded to the modal width"
    );
    if one_row {
        assert_eq!(head.text.trim_end(), format!("{label} {display}"));
        assert_eq!(
            head.text.chars().last(),
            Some(' '),
            "a blank cell stays before the border"
        );
        return Ok(Shown::Complete(vec![head.y]));
    }
    assert_eq!(
        head.text.trim_end(),
        label,
        "a wrapped link's label row stands alone"
    );
    // The block: the consecutive rows under the label that are fragments of the
    // value, up to the point where the value is complete.
    let (mut joined, mut rows) = (String::new(), Vec::new());
    for row in inner.iter().filter(|row| row.y > head.y) {
        let text = row.text.trim_end();
        if row.y != head.y + 1 + rows.len()
            || text.is_empty()
            || joined == display
            || !display.contains(text)
        {
            break;
        }
        assert!(!text.contains('…'));
        assert_eq!(
            row.text.chars().last(),
            Some(' '),
            "no URL glyph in the cell before the border: {:?}",
            row.text
        );
        joined.push_str(text);
        rows.push(row);
    }
    if joined == display {
        for (index, row) in rows.iter().enumerate() {
            assert!(
                row.text.trim_end().len() == row.width - 1 || index + 1 == rows.len(),
                "every row but the last is hard-wrapped at width - 1"
            );
        }
        return Ok(Shown::Complete(rows.iter().map(|row| row.y).collect()));
    }
    match rows.first() {
        None => Ok(Shown::LabelOnly),
        Some(first) if display.starts_with(first.text.trim_end()) => Err(OracleError::Partial {
            shown: joined.len(),
            of: display.len(),
        }),
        Some(first) => Err(OracleError::ContinuationFragment(
            first.text.trim_end().to_owned(),
        )),
    }
}

/// The modal's inner width for a terminal `width` (login_dialog geometry).
fn inner_width(width: u16) -> usize {
    width.saturating_sub(4).clamp(40, 100).min(width) as usize - 2
}
/// AC1: the link is one row iff `label value` fits before the last cell.
fn one_row(label: &str, url: &str, inner_width: usize) -> bool {
    label.len() + 1 + url.len() < inner_width
}
/// AC2: the displayed value of a link.
fn displayed(url: &str, osc8: bool, one_row: bool) -> String {
    if osc8 || one_row {
        url.to_owned()
    } else {
        url.split_once("://").unwrap().1.to_owned()
    }
}
fn rows_needed(display: &str, inner_width: usize) -> usize {
    display.len().div_ceil(inner_width - 1)
}

// ---------------------------------------------------------------- K1

#[test]
fn k1_full_value_rows_or_label_only_at_every_width() {
    let long = [anthropic(), codex()];
    assert!(long.iter().all(|url| url.len() >= 400), "real-length URLs");
    let mut complete_seen = 0;
    for width in 40u16..=200 {
        let inner = inner_width(width);
        let mut fixtures: Vec<String> = long.to_vec();
        fixtures.push(exact(inner - 1 - SIGN_IN.len() - 1));
        for height in [24u16, 40] {
            for osc8 in [false, true] {
                for url in &fixtures {
                    let frame = Frame::new(&buffer(&browser(url, false), osc8, width, height));
                    let rows = inner_rows(&frame);
                    assert_eq!(frame.envelopes() > 0, osc8, "{width}x{height} osc8={osc8}");
                    let links = [(SIGN_IN, url.as_str()), (SHORTCUT, LOCAL)];
                    let mut proven = 0;
                    for (label, target) in links {
                        let single = one_row(label, target, inner);
                        proven += if single {
                            1
                        } else {
                            1 + rows_needed(&displayed(target, osc8, false), inner)
                        };
                    }
                    for (label, target) in links {
                        let single = one_row(label, target, inner);
                        let display = displayed(target, osc8, single);
                        let shown = read_link(&rows, label, &display, single);
                        let context = format!("{label} {width}x{height} osc8={osc8} {display:.40}");
                        assert_ne!(shown, Shown::Absent, "{context}");
                        // Lead, instructions, progress and footer: 12 lines of slack.
                        if proven + 12 <= height as usize - 4 {
                            assert!(matches!(shown, Shown::Complete(_)), "fits, {context}");
                        }
                        if single {
                            assert!(matches!(shown, Shown::Complete(_)), "one row, {context}");
                        }
                        if let Shown::Complete(ys) = shown {
                            complete_seen += 1;
                            if !single {
                                assert_eq!(ys.len(), rows_needed(&display, inner), "{context}");
                            }
                            // Every URL glyph cell is linked (osc8) or plain.
                            let start = if single { label.len() + 1 } else { 0 };
                            for y in ys {
                                let row = rows.iter().find(|row| row.y == y).unwrap();
                                let end = if single {
                                    start + display.len()
                                } else {
                                    row.text.trim_end().len()
                                };
                                for x in start..end {
                                    let target_at = &frame.targets[y][row.x0 + x];
                                    assert_eq!(
                                        target_at.as_deref(),
                                        osc8.then_some(target),
                                        "{context} at {x},{y}"
                                    );
                                }
                                for x in 0..start {
                                    assert!(frame.targets[y][row.x0 + x].is_none(), "label cell");
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(complete_seen > 1000, "positive control: {complete_seen}");
}

fn inner_rows(frame: &Frame) -> Vec<Inner> {
    frame.inner()
}

// ---------------------------------------------------------------- K2

/// herdr's plain-URL span rule (`src/app/actions.rs:2125-2138,2305-2340`).
fn herdr_urls(row: &str) -> Vec<String> {
    fn starts_with(cells: &[char], needle: &str) -> bool {
        let needle: Vec<char> = needle.chars().collect();
        cells.len() >= needle.len() && cells[..needle.len()] == needle[..]
    }
    fn balanced(cells: &[char], start: usize, end: usize, open: char, close: char) -> bool {
        let mut balance = 0i32;
        for ch in &cells[start..end] {
            if *ch == open {
                balance += 1;
            } else if *ch == close {
                balance -= 1;
            }
        }
        balance > 0
    }
    fn trim(cells: &[char], start: usize, mut end: usize) -> Option<(usize, usize)> {
        while start <= end {
            let trim = match cells[end] {
                '"' | '\'' | '`' | '.' | ',' | ';' | ':' | '!' | '?' => true,
                ')' => !balanced(cells, start, end, '(', ')'),
                ']' => !balanced(cells, start, end, '[', ']'),
                '}' => !balanced(cells, start, end, '{', '}'),
                _ => false,
            };
            if !trim {
                break;
            }
            if end == 0 {
                return None;
            }
            end -= 1;
        }
        (start <= end).then_some((start, end))
    }
    let cells: Vec<char> = row.chars().collect();
    let (mut urls, mut start) = (Vec::new(), 0);
    while start < cells.len() {
        if starts_with(&cells[start..], "http://") || starts_with(&cells[start..], "https://") {
            let mut end = start;
            while end + 1 < cells.len() && !cells[end + 1].is_whitespace() {
                end += 1;
            }
            if let Some((from, to)) = trim(&cells, start, end) {
                urls.push(cells[from..=to].iter().collect());
            }
            start = end + 1;
        } else {
            start += 1;
        }
    }
    urls
}

#[test]
fn k2_no_terminal_detector_extracts_a_partial_url() {
    // Positive controls: the helper reproduces the defect and the exact fit.
    let broken = "│Sign-in link: https://claude.ai/oauth/authorize?client_i…x&code=true│";
    assert_eq!(
        herdr_urls(broken),
        ["https://claude.ai/oauth/authorize?client_i…x&code=true│"]
    );
    let a7 = format!("https://a.test/?next=https://b.test/{}", "y".repeat(200));
    for width in [60u16, 80, 100, 140, 200] {
        let inner = inner_width(width);
        let exact_url = exact(inner - 1 - SIGN_IN.len() - 1);
        let exact_rows = Frame::new(&buffer(&browser(&exact_url, false), false, width, 24)).rows();
        let extracted: Vec<String> = exact_rows.iter().flat_map(|row| herdr_urls(row)).collect();
        assert!(
            extracted.contains(&exact_url),
            "an exact fit extracts whole: {width}"
        );
        assert!(!exact_url.contains('│'));
        for height in [24u16, 40] {
            for url in [anthropic(), codex(), a7.clone()] {
                let frame = Frame::new(&buffer(&browser(&url, false), false, width, height));
                assert_eq!(frame.envelopes(), 0);
                let rows = frame.rows();
                for row in &rows {
                    for found in herdr_urls(row) {
                        assert!(
                            found == LOCAL || found == exact_url,
                            "{width}x{height}: {found:?} is a partial URL\n{}",
                            rows.join("\n")
                        );
                    }
                }
                let inner = frame.inner();
                let single = one_row(SIGN_IN, &url, inner_width(width));
                assert!(!single, "the fixture wraps at {width}");
                let display = displayed(&url, false, false);
                let shown = read_link(&inner, SIGN_IN, &display, false);
                if url == a7 {
                    // A7: the remainder still embeds a scheme, so no URL rows.
                    assert_eq!(shown, Shown::LabelOnly, "A7 at {width}x{height}");
                } else if height == 40 && width >= 60 {
                    assert!(matches!(shown, Shown::Complete(_)), "{width}x{height}");
                }
                if shown != Shown::Absent {
                    let joined: String = rows.join("\n");
                    assert!(
                        !joined.contains("https://"),
                        "{width}x{height}: a plain row keeps a scheme\n{joined}"
                    );
                }
            }
        }
    }
    k2_gnome_scheme_less_www_and_ftp_links_never_expose_a_partial_url();
}

/// GNOME Terminal's scheme-less link rule, ported for the one case that matters
/// here (`src/terminal-regex.hh` `REGEX_URL_HTTP`, linked with
/// `FLAVOR_DEFAULT_TO_HTTP` in `src/terminal-screen.cc`): at a position not
/// preceded by a hostname character or `.`, `www` or `ftp` (any case) starts a
/// link made of a hostname (`[-[:alnum:]]` or a non-ASCII graphic character,
/// dot-separated), an optional `:port` (1..=65535) and an optional path that
/// starts with `/`, `?` or `#`, uses the path character class and ends in a
/// path-terminator character. Not ported: `file:`, e-mail, VoIP, `news`, balanced
/// bracket paths and the apostrophe rule. The model sees one row's text.
fn gnome_www_links(row: &str) -> Vec<String> {
    let cells: Vec<char> = row.chars().collect();
    let host = |c: char| {
        c == '-'
            || c.is_ascii_alphanumeric()
            || (!c.is_ascii() && !c.is_control() && !c.is_whitespace())
    };
    let path = |c: char| c.is_ascii_alphanumeric() || "-_$.+!*,:;@&=?/~#|%'".contains(c);
    let term = |c: char| c.is_ascii_alphanumeric() || "-_$+*@&=/~#|%'".contains(c);
    let (mut found, mut at) = (Vec::new(), 0);
    while at < cells.len() {
        let lead: String = cells[at..cells.len().min(at + 3)]
            .iter()
            .collect::<String>()
            .to_ascii_lowercase();
        let blocked = at > 0 && (host(cells[at - 1]) || cells[at - 1] == '.');
        if blocked || !(lead == "www" || lead == "ftp") {
            at += 1;
            continue;
        }
        let mut end = at;
        while end < cells.len() && (host(cells[end]) || cells[end] == '.') {
            end += 1;
        }
        while end > at && cells[end - 1] == '.' {
            end -= 1;
        }
        if end < cells.len() && cells[end] == ':' {
            let digits: String = cells[end + 1..]
                .iter()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if digits
                .parse::<u32>()
                .is_ok_and(|port| (1..=65535).contains(&port))
                && digits.len() <= 5
            {
                end += 1 + digits.len();
            }
        }
        if end < cells.len() && matches!(cells[end], '/' | '?' | '#') {
            let mut stop = end + 1;
            while stop < cells.len() && path(cells[stop]) {
                stop += 1;
            }
            while stop > end + 1 && !term(cells[stop - 1]) {
                stop -= 1;
            }
            end = stop;
        }
        found.push(cells[at..end].iter().collect());
        at = end.max(at + 1);
    }
    found
}

/// Part of K2: a GNOME Terminal scheme-less detector must never find a partial
/// URL in a plain-mode view (O3).
fn k2_gnome_scheme_less_www_and_ftp_links_never_expose_a_partial_url() {
    // Positive controls: the model finds a bare www/ftp link, and is blocked
    // inside a longer hostname, after a dot and in `abc.www.foo.bar`.
    assert_eq!(
        gnome_www_links("open www.example.com/a?b=c now"),
        ["www.example.com/a?b=c"]
    );
    assert_eq!(gnome_www_links("FTP.Example.com/x"), ["FTP.Example.com/x"]);
    assert!(gnome_www_links("abc.www.foo.bar xwww.foo.bar").is_empty());
    let path = format!("docs/{}?q={}", "segment/".repeat(20), "v".repeat(40));
    let fixtures = [
        ("www", format!("https://www.example.com/{path}")),
        ("ftp", format!("https://ftp.example.com/{path}")),
        ("upper", format!("HTTPS://WWW.EXAMPLE.COM/{path}")),
    ];
    for width in [60u16, 80, 100, 140] {
        let inner = inner_width(width);
        for (name, url) in &fixtures {
            assert!(
                !one_row(SIGN_IN, &url.to_ascii_lowercase(), inner),
                "{name} wraps at {width}"
            );
            // Plain mode: no row of the view (inner text, so no border glyph
            // class is relied on) yields a partial link; the label stays.
            let frame = Frame::new(&buffer(&browser(url, false), false, width, 40));
            let rows = frame.inner();
            for row in &rows {
                let found = gnome_www_links(&row.text);
                assert!(
                    found.is_empty(),
                    "{name} at {width}: GNOME links {found:?} in {:?}",
                    row.text
                );
            }
            let canonical = url::Url::parse(url).unwrap().to_string();
            let shown = read_link(&rows, SIGN_IN, &displayed(&canonical, false, false), false);
            assert_eq!(shown, Shown::LabelOnly, "{name} at {width}");
            // The copy path is unchanged: footer key, full value.
            assert!(frame.rows().join("\n").contains("c copy link"));
            // OSC 8 on: the full URL, every row linked.
            let linked = Frame::new(&buffer(&browser(url, false), true, width, 40));
            let shown = read_link(&linked.inner(), SIGN_IN, &canonical, false);
            assert!(matches!(shown, Shown::Complete(_)), "{name} at {width}");
            assert!(linked.envelopes() >= canonical.len());
        }
        // A one-row www value keeps its scheme: its www part sits inside a
        // complete scheme URL.
        let short = "https://www.example.com/x".to_owned();
        let frame = Frame::new(&buffer(&browser(&short, false), false, width, 24));
        let rows = frame.inner();
        assert_eq!(
            read_link(&rows, SIGN_IN, &short, true),
            Shown::Complete(vec![
                rows.iter().find(|r| r.text.starts_with(SIGN_IN)).unwrap().y
            ])
        );
        for row in &rows {
            for found in gnome_www_links(&row.text) {
                assert!(row.text.contains(&format!("https://{found}")), "{found:?}");
            }
        }
        // Control: a non-www host still wraps, every row retained.
        let control = Frame::new(&buffer(&browser(&anthropic(), false), false, width, 40));
        let shown = read_link(
            &control.inner(),
            SIGN_IN,
            &displayed(&anthropic(), false, false),
            false,
        );
        assert!(
            matches!(shown, Shown::Complete(_)),
            "claude.ai wraps at {width}"
        );
    }
}

// ---------------------------------------------------------------- K3

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);
impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn paint_bytes(state: &LoginOverlayState, width: u16, height: u16) -> String {
    let sink = Sink::default();
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(sink.clone()),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, width, height)),
        },
    )
    .unwrap();
    let theme = Theme::dark();
    terminal
        .draw(|f| login_dialog::render(f, f.area(), state, &theme, true))
        .unwrap();
    let bytes = std::mem::take(&mut *sink.0.lock().unwrap());
    String::from_utf8(bytes).unwrap()
}

#[test]
fn k3_every_url_glyph_on_every_row_is_linked_to_the_full_url() {
    let url = anthropic();
    for (width, height) in [(80u16, 24u16), (140, 30)] {
        let inner = inner_width(width);
        let state = browser(&url, true);
        let frame = Frame::new(&buffer(&state, true, width, height));
        let rows = frame.inner();
        let signed = read_link(&rows, SIGN_IN, &url, false);
        let Shown::Complete(ys) = &signed else {
            panic!("{width}x{height} shows the whole sign-in link: {signed:?}");
        };
        assert_eq!(ys.len(), rows_needed(&url, inner));
        assert!(ys.len() > 1);
        let shortcut = read_link(&rows, SHORTCUT, LOCAL, true);
        assert!(matches!(shortcut, Shown::Complete(_)), "{shortcut:?}");
        // The enveloped cells are exactly the URL glyphs of both links.
        assert_eq!(frame.envelopes(), url.len() + LOCAL.len());
        for y in ys {
            let row = rows.iter().find(|row| row.y == *y).unwrap();
            for x in 0..row.text.trim_end().len() {
                assert_eq!(frame.targets[*y][row.x0 + x].as_deref(), Some(url.as_str()));
            }
        }
        // A real crossterm paint: one balanced envelope per URL glyph.
        let bytes = paint_bytes(&state, width, height);
        assert_eq!(
            bytes.matches(&format!("\x1b]8;;{url}\x07")).count(),
            url.len(),
            "sign-in envelopes"
        );
        assert_eq!(
            bytes.matches(&format!("\x1b]8;;{LOCAL}\x07")).count(),
            LOCAL.len()
        );
        assert_eq!(
            bytes.matches("\x1b]8;;\x07").count(),
            url.len() + LOCAL.len(),
            "balanced closes"
        );
        // Positive control: the plain buffer has none.
        assert_eq!(
            Frame::new(&buffer(&state, false, width, height)).envelopes(),
            0
        );
    }
}

#[test]
fn k3_identical_rows_with_different_full_targets_each_keep_their_own() {
    let make = |mark: &str| {
        format!(
            "https://auth.example/{}{mark}{}",
            "p".repeat(100),
            "q".repeat(100)
        )
    };
    let (first, second) = (make("A"), make("B"));
    let state = running(
        vec![
            browser_section(&first, LOCAL),
            browser_section(&second, LOCAL),
        ],
        false,
        false,
    );
    let frame = Frame::new(&buffer(&state, true, 140, 40));
    let rows = frame.inner();
    let heads: Vec<&Inner> = rows
        .iter()
        .filter(|row| row.text.starts_with(SIGN_IN))
        .collect();
    assert_eq!(heads.len(), 2);
    let mut visible = Vec::new();
    for (head, target) in heads.iter().zip([&first, &second]) {
        let mut text = Vec::new();
        for offset in 1..=3 {
            let row = rows.iter().find(|row| row.y == head.y + offset).unwrap();
            let shown = row.text.trim_end();
            for x in 0..shown.len() {
                assert_eq!(
                    frame.targets[row.y][row.x0 + x].as_deref(),
                    Some(target.as_str())
                );
            }
            text.push(shown.to_owned());
        }
        assert_eq!(text.concat(), *target);
        visible.push(text);
    }
    // Rows 1 and 3 read the same; only their targets differ.
    assert_eq!(visible[0][0], visible[1][0]);
    assert_eq!(visible[0][2], visible[1][2]);
    assert_ne!(visible[0][1], visible[1][1]);
    assert_eq!(Frame::new(&buffer(&state, false, 140, 40)).envelopes(), 0);
}

// ---------------------------------------------------------------- K4

#[test]
fn k4_the_fitter_keeps_a_whole_link_or_its_label() {
    let url = anthropic();
    let mut independent = false;
    let mut whole_seen = false;
    for width in [40u16, 60, 80, 100] {
        let inner = inner_width(width);
        for height in 8u16..=40 {
            for osc8 in [false, true] {
                let state = running(vec![browser_section(&url, LOCAL)], true, true);
                let frame = Frame::new(&buffer(&state, osc8, width, height));
                let rows = frame.inner();
                let sign_single = one_row(SIGN_IN, &url, inner);
                let local_single = one_row(SHORTCUT, LOCAL, inner);
                let sign = read_link(
                    &rows,
                    SIGN_IN,
                    &displayed(&url, osc8, sign_single),
                    sign_single,
                );
                let local = read_link(
                    &rows,
                    SHORTCUT,
                    &displayed(LOCAL, osc8, local_single),
                    local_single,
                );
                independent |= sign == Shown::LabelOnly && matches!(local, Shown::Complete(_));
                whole_seen |= matches!(sign, Shown::Complete(_));
                if width == 60 && height == 16 {
                    let text = frame.rows().join("\n");
                    for corner in ['┌', '┐', '└', '┘'] {
                        assert!(text.contains(corner), "{corner}\n{text}");
                    }
                    assert_ne!(sign, Shown::Absent, "a row starting Sign-in link\n{text}");
                    assert!(rows.iter().any(|row| row.text.starts_with("> ")), "{text}");
                    assert!(text.contains("Alt+C copy link"), "footer copy key\n{text}");
                }
            }
        }
    }
    assert!(independent, "the two headers fit independently");
    assert!(whole_seen, "positive control: a height fits the whole link");
    oracle_rejects_a_dropped_first_row();
}

/// The all-or-label oracle itself: a good block passes; a block whose first URL
/// row was dropped (later rows retained under the label) and a block cut after
/// its first row are both rejected, each with its own error variant.
fn oracle_rejects_a_dropped_first_row() {
    let url = anthropic();
    let display = displayed(&url, false, false);
    let frame = Frame::new(&buffer(&browser(&url, false), false, 80, 40));
    let good = frame.inner();
    let head = good
        .iter()
        .position(|row| row.text.starts_with(SIGN_IN))
        .unwrap();
    let rebuild = |keep: &dyn Fn(usize) -> bool| -> Vec<Inner> {
        // Rows of the link block (label + URL rows), renumbered to be contiguous.
        let rows = rows_needed(&display, inner_width(80));
        let mut out = Vec::new();
        for (offset, row) in good[head..=head + rows].iter().enumerate() {
            if offset == 0 || keep(offset) {
                out.push(Inner {
                    y: good[head].y + out.len(),
                    x0: row.x0,
                    width: row.width,
                    text: row.text.clone(),
                });
            }
        }
        out
    };
    let block = rebuild(&|_| true);
    assert!(block.len() > 3);
    assert!(matches!(
        try_read_link(&block, SIGN_IN, &display, false),
        Ok(Shown::Complete(rows)) if rows.len() == block.len() - 1
    ));
    let dropped_first = rebuild(&|offset| offset != 1);
    assert_eq!(dropped_first.len(), block.len() - 1);
    assert_eq!(
        try_read_link(&dropped_first, SIGN_IN, &display, false),
        Err(OracleError::ContinuationFragment(
            good[head + 2].text.trim_end().to_owned()
        ))
    );
    let cut = rebuild(&|offset| offset == 1);
    assert_eq!(
        try_read_link(&cut, SIGN_IN, &display, false),
        Err(OracleError::Partial {
            shown: good[head + 1].text.trim_end().len(),
            of: display.len()
        })
    );
    let label_only = rebuild(&|_| false);
    assert_eq!(
        try_read_link(&label_only, SIGN_IN, &display, false),
        Ok(Shown::LabelOnly)
    );
}

/// A provider URL long enough to wrap over more than `u16::MAX` rows (the device
/// URL is unbounded and `safe_url` caps no length) must neither panic nor leak a
/// partial link: the fitter keeps the code line, or the label, and nothing else.
#[test]
fn k1b_oversized_device_and_browser_urls_clamp_and_keep_none_of_the_link() {
    let width = 40u16;
    let inner = inner_width(width);
    let device_url = format!(
        "https://accounts.x.ai/device?next={}",
        "n".repeat((inner - 1) * (usize::from(u16::MAX) + 100))
    );
    assert!(rows_needed(&device_url, inner) > usize::from(u16::MAX));
    for osc8 in [false, true] {
        for height in [8u16, 24] {
            let device = running(
                vec![LoginSection::Device {
                    verification_url: device_url.clone(),
                    user_code: "WDJB-MJHT".into(),
                    instructions: String::new(),
                }],
                false,
                false,
            );
            let frame = Frame::new(&buffer(&device, osc8, width, height));
            let rows = frame.inner();
            assert!(
                rows.iter().any(|row| row.text.contains("WDJB-MJHT")),
                "the code line stays"
            );
            let shown = read_link(
                &rows,
                "Sign-in page:",
                &displayed(&device_url, osc8, false),
                false,
            );
            assert!(!matches!(shown, Shown::Complete(_)), "{shown:?}");
            assert_eq!(frame.envelopes(), 0, "no partial link cells");
            let browser_state = browser(&device_url, true);
            let frame = Frame::new(&buffer(&browser_state, osc8, width, height));
            assert!(
                frame
                    .targets
                    .iter()
                    .flatten()
                    .all(|target| target.as_deref() != Some(device_url.as_str())),
                "no cell of the oversized link is linked"
            );
            let shown = read_link(
                &frame.inner(),
                SIGN_IN,
                &displayed(&device_url, osc8, false),
                false,
            );
            assert!(!matches!(shown, Shown::Complete(_)), "{shown:?}");
        }
    }
}

// ---------------------------------------------------------------- K5

const TMUX_ENV: Option<&str> = Some("/tmp/tmux-1000/default,1,0");

#[test]
fn k5_capability_table_and_dialog() {
    let cases = [
        (
            "xterm-256color",
            true,
            Some("xterm-256color"),
            None,
            None,
            true,
        ),
        (
            "herdr",
            true,
            Some("xterm-256color"),
            Some(""),
            Some(""),
            true,
        ),
        ("linux", true, Some("linux"), None, None, true),
        ("tmux", true, Some("tmux-256color"), TMUX_ENV, None, true),
        (
            "screen in tmux",
            true,
            Some("screen-256color"),
            TMUX_ENV,
            None,
            true,
        ),
        ("ghostty", true, Some("xterm-ghostty"), None, None, true),
        ("kitty", true, Some("xterm-kitty"), None, None, true),
        (
            "trimmed",
            true,
            Some(" xterm-256color "),
            Some(" \t"),
            Some(" \n"),
            true,
        ),
        (
            "trimmed tmux",
            true,
            Some(" screen-256color "),
            Some(" 1 "),
            None,
            true,
        ),
        ("vt100", true, Some("vt100"), None, None, true),
        (
            "not a tty",
            false,
            Some("xterm-256color"),
            None,
            None,
            false,
        ),
        ("no TERM", true, None, None, None, false),
        ("empty TERM", true, Some(""), None, None, false),
        ("blank TERM", true, Some("  "), None, None, false),
        ("dumb", true, Some("DUMB"), None, None, false),
        (
            "STY",
            true,
            Some("xterm-256color"),
            None,
            Some("1234.pts"),
            false,
        ),
        ("screen", true, Some("screen"), None, None, false),
        (
            "screen-256color",
            true,
            Some("screen-256color"),
            None,
            None,
            false,
        ),
        (
            "screen blank tmux",
            true,
            Some("screen-256color"),
            Some(" "),
            None,
            false,
        ),
        (
            "STY beats tmux",
            true,
            Some("screen-256color"),
            TMUX_ENV,
            Some("1.pts"),
            false,
        ),
    ];
    let state = browser(&anthropic(), true);
    for (name, tty, term, tmux, sty, wanted) in cases {
        let support = supports_osc8(tty, term, tmux, sty);
        assert_eq!(support, wanted, "{name}");
        let frame = Frame::new(&buffer(&state, support, 80, 24));
        assert_eq!(
            frame.envelopes() > 0,
            wanted,
            "{name}: the dialog follows the policy"
        );
    }
}

/// Runs `test` as a child whose stdout is a PTY slave, with `vars` applied to its
/// environment. The master is read on a thread and awaited with a deadline, so a
/// child that never exits fails with the captured bytes instead of hanging.
#[cfg(unix)]
fn pty_child(test: &str, vars: &[(&str, Option<&str>)]) -> String {
    use std::io::Read;
    use std::os::fd::FromRawFd;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    let (mut master, mut slave) = (-1, -1);
    let size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty fills two newly owned descriptors.
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        )
    };
    assert_eq!(result, 0, "openpty: {}", io::Error::last_os_error());
    // SAFETY: both descriptors are fresh from openpty.
    let mut master = unsafe { std::fs::File::from_raw_fd(master) };
    let slave = unsafe { std::fs::File::from_raw_fd(slave) };
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test, "--nocapture"])
        .env("RUSTAIN_K5_CHILD", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(slave))
        .stderr(Stdio::piped());
    for (key, value) in vars {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
    let mut child = command.spawn().unwrap();
    // The command owns the parent's copy of the slave: drop it, or the master
    // never sees the child's exit.
    drop(command);
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let sink = captured.clone();
    std::thread::spawn(move || {
        let mut block = [0u8; 4096];
        loop {
            match master.read(&mut block) {
                Ok(0) => break,
                Ok(n) => sink.lock().unwrap().extend_from_slice(&block[..n]),
                // Linux reports EIO rather than EOF once the slave closes.
                Err(_) => break,
            }
        }
        let _ = done_tx.send(());
    });
    if done_rx.recv_timeout(Duration::from_secs(120)).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        let seen = String::from_utf8_lossy(&captured.lock().unwrap()).into_owned();
        panic!("PTY child did not finish within 120 s; captured: {seen:?}");
    }
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&captured.lock().unwrap()).into_owned();
    assert!(
        output.status.success() && stdout.contains("1 passed; 0 failed"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

/// Runs only as the PTY child of `k5_wrapper_reads_the_environment_once`.
#[test]
fn k5_wrapper_child() {
    if std::env::var_os("RUSTAIN_K5_CHILD").is_some() {
        println!("K5_OSC8={}", terminal_supports_osc8());
    }
}

#[cfg(unix)]
#[test]
fn k5_wrapper_reads_the_environment_once() {
    let child = "conformance_20_9j_sign_in_link::k5_wrapper_child";
    let base = [
        ("TERM", Some("xterm-256color")),
        ("TERM_PROGRAM", None),
        ("TMUX", Some("")),
        ("STY", Some("")),
    ];
    let on = pty_child(child, &base);
    assert!(on.contains("K5_OSC8=true"), "{on}");
    let mut denied = base.to_vec();
    denied[3] = ("STY", Some("x"));
    let off = pty_child(child, &denied);
    assert!(off.contains("K5_OSC8=false"), "{off}");
}

// ---------------------------------------------------------------- K6b

/// The lead notice: every inner row above the first link label, joined.
fn lead(frame: &Frame) -> String {
    let mut words = Vec::new();
    for row in frame.inner() {
        if row.text.starts_with(SIGN_IN) {
            break;
        }
        words.push(row.text.trim().to_owned());
    }
    words
        .into_iter()
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn copy_text(state: &mut TuiState, key: DomainInputEvent) -> String {
    match login::on_key(state, &key) {
        InputAction::Login(LoginAction::Copy(text)) => text,
        other => panic!("a copy key yields a copy action, got {other:?}"),
    }
}

#[test]
fn k6b_unfocused_lead_says_press_c_and_copies_the_full_urls() {
    let long = anthropic();
    let state = browser(&long, false);
    assert!(state.running.as_ref().unwrap().field.is_none());
    let plain = Frame::new(&buffer(&state, false, 80, 24));
    assert!(lead(&plain).contains(PLAIN_LEAD), "{}", lead(&plain));
    // OSC 8 on keeps the original lead.
    let linked = Frame::new(&buffer(&state, true, 80, 24));
    assert!(lead(&linked).contains(ORIGINAL_LEAD), "{}", lead(&linked));
    // The copy keys give the full URLs, never the scheme-less display.
    let mut tui = TuiState::new(80, 24);
    tui.login = state;
    assert_eq!(copy_text(&mut tui, DomainInputEvent::KeyPress('c')), long);
    assert_eq!(copy_text(&mut tui, DomainInputEvent::KeyPress('l')), LOCAL);
    // Focused: the Alt+C lead, and the unchanged lead when the link fits.
    let focused = browser(&long, true);
    assert!(lead(&Frame::new(&buffer(&focused, false, 80, 24))).contains(ALT_LEAD));
    assert!(lead(&Frame::new(&buffer(&focused, true, 80, 24))).contains(ORIGINAL_LEAD));
    // Positive control: a one-row link keeps the original lead in plain mode.
    let short = browser("https://auth.example/authorize?client=example", false);
    let frame = Frame::new(&buffer(&short, false, 80, 24));
    assert!(lead(&frame).contains(ORIGINAL_LEAD), "{}", lead(&frame));
    assert!(!lead(&frame).contains("press"));
}
