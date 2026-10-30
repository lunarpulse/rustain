//! Conformance for the login widget's in-buffer OSC 8 rendering.
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
    style::{Color, Modifier, Style},
    widgets::{Clear, Paragraph},
};
use rustain::adapters::tui::{
    color_detect::ColorCapability,
    state::{LoginOrigin, LoginOverlayState, LoginSection, LoginView, RunningLogin},
    theme::Theme,
    widgets::{hyperlink::supports_osc8, login_dialog},
};
use tokio_util::sync::CancellationToken;
use unicode_width::UnicodeWidthChar;

const AUTH: &str =
    "https://auth.example/authorize?client=example&state=abcdefghijklmnopqrstuvwxyz0123456789";
const LOCAL: &str = "http://localhost:43123/launch";
const DEVICE: &str = "https://device.example/verify?user_code=ABCD";

fn state(sections: Vec<LoginSection>) -> LoginOverlayState {
    let mut state = LoginOverlayState::default();
    state.active = true;
    state.view = LoginView::Running;
    state.running = Some(RunningLogin {
        login_id: 1,
        provider_id: "example".into(),
        display: "Example".into(),
        origin: LoginOrigin::Slash,
        cancel: CancellationToken::new(),
        sections,
        progress: Vec::new(),
        field: None,
    });
    state
}
fn browser(url: &str, local: &str) -> LoginOverlayState {
    state(vec![LoginSection::AuthUrl {
        url: url.into(),
        launch_url: local.into(),
        instructions: String::new(),
    }])
}
fn device(url: &str, code: &str) -> LoginOverlayState {
    state(vec![LoginSection::Device {
        verification_url: url.into(),
        user_code: code.into(),
        instructions: String::new(),
    }])
}
fn buffer(
    state: &LoginOverlayState,
    theme: &Theme,
    support: bool,
    width: u16,
    height: u16,
) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|f| login_dialog::render(f, f.area(), state, theme, support))
        .unwrap();
    terminal.backend().buffer().clone()
}

/// Only the exact per-cell OSC 8 envelope is accepted; no other control may be hidden.
fn glyph(symbol: &str) -> (&str, Option<&str>) {
    if let Some(rest) = symbol.strip_prefix("\x1b]8;;") {
        let (target, rest) = rest.split_once('\x07').expect("OSC target terminator");
        assert!(!target.is_empty());
        let visible = rest
            .strip_suffix("\x1b]8;;\x07")
            .expect("balanced OSC close");
        assert_eq!(visible.chars().count(), 1, "one visible glyph per cell");
        assert!(!visible.chars().any(char::is_control));
        (visible, Some(target))
    } else {
        assert!(
            !symbol.chars().any(char::is_control),
            "unexpected control in {symbol:?}"
        );
        (symbol, None)
    }
}
fn projection(b: &Buffer) -> String {
    let mut result = String::new();
    for y in 0..b.area.height {
        for x in 0..b.area.width {
            result.push_str(glyph(b[(x, y)].symbol()).0);
        }
        result.push('\n');
    }
    result
}
fn links(b: &Buffer, theme: &Theme) -> Vec<(u16, u16, String)> {
    let mut found = Vec::new();
    for y in 0..b.area.height {
        for x in 0..b.area.width {
            let cell = &b[(x, y)];
            if let (_, Some(target)) = glyph(cell.symbol()) {
                assert_eq!(
                    cell.diff_option,
                    CellDiffOption::ForcedWidth(NonZeroU16::new(1).unwrap())
                );
                assert_eq!(cell.fg, theme.colors.accent);
                assert_eq!(cell.bg, theme.colors.bg_surface);
                assert!(cell.modifier.contains(Modifier::BOLD));
                found.push((x, y, target.to_owned()));
            }
        }
    }
    found
}

/// Expected link rectangles come only from the readable fallback and labels.
fn assert_link_rectangles(
    plain: &Buffer,
    linked: &Buffer,
    theme: &Theme,
    expected: &[(&str, &str)],
) {
    let text = projection(plain);
    let rectangles: Vec<_> = expected
        .iter()
        .map(|(label, target)| {
            let rows: Vec<_> = text
                .lines()
                .enumerate()
                .filter(|(_, row)| row.contains(label))
                .collect();
            assert_eq!(rows.len(), 1, "one visible row for {label:?}");
            let (y, row) = rows[0];
            let start = row[..row.find(label).unwrap()].chars().count() + label.chars().count();
            let end = row[..row.rfind('│').expect("modal right border")]
                .trim_end()
                .chars()
                .count();
            assert!(end > start, "visible URL after {label:?}");
            (y as u16, start as u16, end as u16, *target)
        })
        .collect();
    for y in 0..plain.area.height {
        for x in 0..plain.area.width {
            let cell = &linked[(x, y)];
            let fallback = &plain[(x, y)];
            assert_eq!(cell.style(), fallback.style(), "style at {x},{y}");
            if let Some((_, _, _, target)) = rectangles
                .iter()
                .find(|(row, start, end, _)| y == *row && x >= *start && x < *end)
            {
                assert_eq!(
                    cell.symbol(),
                    format!("\x1b]8;;{target}\x07{}\x1b]8;;\x07", fallback.symbol()),
                    "complete URL rectangle at {x},{y}"
                );
                assert_eq!(
                    cell.diff_option,
                    CellDiffOption::ForcedWidth(NonZeroU16::new(1).unwrap())
                );
                assert_eq!(cell.fg, theme.colors.accent);
                assert_eq!(cell.bg, theme.colors.bg_surface);
                assert!(cell.modifier.contains(Modifier::BOLD));
            } else {
                assert_eq!(cell.symbol(), fallback.symbol(), "unlinked cell at {x},{y}");
                assert!(!cell.symbol().chars().any(char::is_control));
            }
        }
    }
}

#[test]
fn k1_k2_browser_cells_and_capability_fallback() {
    let cases = [
        (
            true,
            Some("xterm-256color"),
            Some("WezTerm"),
            None,
            None,
            true,
        ),
        (true, Some("xterm-kitty"), None, None, None, true),
        (true, Some("foot-extra"), None, None, None, true),
        (true, Some("foot"), None, None, None, true),
        (true, Some("xterm"), Some("KITTY"), None, None, true),
        (true, Some("xterm"), Some("iTerm2"), None, None, true),
        (true, Some("xterm"), Some("foot"), None, None, true),
        (
            true,
            Some("xterm-256color"),
            Some("iTerm.app"),
            None,
            None,
            true,
        ),
        (false, Some("xterm-kitty"), Some("kitty"), None, None, false),
        (true, None, Some("kitty"), None, None, false),
        (true, Some(""), Some("kitty"), None, None, false),
        (true, Some("DUMB"), Some("kitty"), None, None, false),
        (
            true,
            Some("screen-256color"),
            Some("kitty"),
            None,
            None,
            false,
        ),
        (
            true,
            Some("tmux-256color"),
            Some("kitty"),
            None,
            None,
            false,
        ),
        (
            true,
            Some("xterm-256color"),
            Some("kitty"),
            Some("session"),
            None,
            false,
        ),
        (
            true,
            Some("xterm-256color"),
            Some("kitty"),
            None,
            Some("session"),
            false,
        ),
        (true, Some("xterm-256color"), None, None, None, false),
        (true, Some("linux"), Some("unknown"), None, None, false),
    ];
    let theme = Theme::dark();
    let state = browser(AUTH, LOCAL);
    let plain = buffer(&state, &theme, false, 80, 24);
    let expected = projection(&plain);
    assert!(expected.contains("Sign-in link: https://auth.example/"));
    assert!(expected.contains('…'));
    assert!(expected.contains(LOCAL));
    assert!(expected.contains("Esc cancel"));
    let label = expected
        .lines()
        .position(|line| line.contains("Sign-in link:"))
        .unwrap() as u16;
    let label_line = expected.lines().nth(label as usize).unwrap();
    let label_x = label_line[..label_line.find("Sign-in link:").unwrap()]
        .chars()
        .count() as u16;
    assert_eq!(plain[(label_x, label)].fg, theme.colors.fg_muted);
    assert_eq!(plain[(label_x, label)].bg, theme.colors.bg_surface);
    assert!(!plain[(label_x, label)].modifier.contains(Modifier::BOLD));
    for (tty, term, program, tmux, sty, wanted) in cases {
        let support = supports_osc8(tty, term, program, tmux, sty);
        assert_eq!(support, wanted);
        let drawn = buffer(&state, &theme, support, 80, 24);
        assert_eq!(projection(&drawn), expected);
        let encoded = links(&drawn, &theme);
        if wanted {
            assert!(encoded.iter().any(|(_, _, target)| target == AUTH));
            assert!(encoded.iter().any(|(_, _, target)| target == LOCAL));
        } else {
            assert!(encoded.is_empty());
        }
        for y in 0..24 {
            for x in 0..80 {
                let a = &plain[(x, y)];
                let b = &drawn[(x, y)];
                assert_eq!((a.fg, a.bg, a.modifier), (b.fg, b.bg, b.modifier));
            }
        }
    }
}

#[test]
fn k4_device_safe_and_hostile_supported() {
    let theme = Theme::dark();
    let state = device(DEVICE, "ABCD-EFGH");
    let plain = buffer(&state, &theme, false, 80, 24);
    let safe = buffer(&state, &theme, true, 80, 24);
    assert!(projection(&safe).contains("ABCD-EFGH"));
    assert_link_rectangles(&plain, &safe, &theme, &[("Sign-in page: ", DEVICE)]);
    let hostile_state = device(
        "https://example.test/\x1b]52;c;INJECT\x07",
        "CODE\x1b]52;c;SECRET\x07",
    );
    let hostile = buffer(&hostile_state, &theme, true, 80, 24);
    let text = projection(&hostile);
    assert!(text.contains("Sign-in page:"));
    assert!(links(&hostile, &theme).is_empty());
    assert!(!text.contains('\x1b'));
    assert_link_rectangles(
        &buffer(&hostile_state, &theme, false, 80, 24),
        &hostile,
        &theme,
        &[],
    );
    let sink = Sink::default();
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(sink.clone()),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
        },
    )
    .unwrap();
    terminal
        .draw(|f| login_dialog::render(f, f.area(), &device(DEVICE, "ABCD-EFGH"), &theme, true))
        .unwrap();
    let positive = String::from_utf8(sink.take()).unwrap();
    assert!(positive.contains(&format!("\x1b]8;;{DEVICE}\x07")));
    terminal
        .draw(|f| login_dialog::render(f, f.area(), &hostile_state, &theme, true))
        .unwrap();
    let bytes = String::from_utf8(sink.take()).unwrap();
    assert!(
        !bytes.contains("\x1b]52;"),
        "hostile OSC52 must not reach the terminal"
    );
    assert!(
        !bytes.contains("\x1b]8;;https://example.test/"),
        "hostile URL must not become a link"
    );
}

#[test]
fn k5_browser_and_device_80x24_snapshots() {
    for (name, theme) in [
        ("dark", Theme::dark()),
        ("color16", Theme::for_capability(ColorCapability::Color16)),
    ] {
        let browser = projection(&buffer(&browser(AUTH, LOCAL), &theme, false, 80, 24));
        let device = projection(&buffer(&device(DEVICE, "ABCD-EFGH"), &theme, false, 80, 24));
        insta::assert_snapshot!(format!("login_browser_{name}_80x24"), browser);
        insta::assert_snapshot!(format!("login_device_{name}_80x24"), device);
    }
}

#[test]
fn k5_dark_and_color16_plain_login_views() {
    for theme in [
        Theme::dark(),
        Theme::for_capability(ColorCapability::Color16),
    ] {
        for state in [browser(AUTH, LOCAL), device(DEVICE, "ABCD-EFGH")] {
            let plain = buffer(&state, &theme, false, 80, 24);
            let supported = buffer(&state, &theme, true, 80, 24);
            let visible = projection(&plain);
            assert_eq!(visible, projection(&supported));
            assert!(visible.contains(" Sign in to Example "));
            assert!(visible.contains("Esc cancel"));
            assert!(!links(&supported, &theme).is_empty());
        }
    }
}

#[test]
fn k3_duplicate_rows_clipping_and_tiny_rectangles() {
    let theme = Theme::dark();
    let section = LoginSection::Device {
        verification_url: DEVICE.into(),
        user_code: "ABCD".into(),
        instructions: String::new(),
    };
    let state = state(vec![section.clone(), section]);
    let b = buffer(&state, &theme, true, 80, 24);
    let encoded = links(&b, &theme);
    let rows: std::collections::BTreeSet<_> = encoded.iter().map(|(_, y, _)| y).collect();
    assert_eq!(rows.len(), 2, "equal link rows must both be encoded");
    for (w, h) in [(60, 16), (10, 5), (1, 1), (0, 0)] {
        let b = buffer(&state, &theme, true, w, h);
        for (x, y, _) in links(&b, &theme) {
            assert!(x < w && y < h);
        }
        assert_eq!(
            projection(&b),
            projection(&buffer(&state, &theme, false, w, h))
        );
    }
}

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
impl Sink {
    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

#[derive(Clone, Debug, Default)]
struct Observed {
    glyph: String,
    target: Option<String>,
    fg: Color,
    bg: Color,
    bold: bool,
}
struct Observer {
    cells: Vec<Observed>,
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    fg: Color,
    bg: Color,
    bold: bool,
    target: Option<String>,
}
impl Observer {
    fn new(width: usize, height: usize) -> Self {
        Self {
            cells: vec![Observed::default(); width * height],
            width,
            height,
            x: 0,
            y: 0,
            fg: Color::Reset,
            bg: Color::Reset,
            bold: false,
            target: None,
        }
    }
    fn replay(&mut self, bytes: &[u8]) {
        let text = std::str::from_utf8(bytes).unwrap();
        let mut it = text.chars().peekable();
        while let Some(c) = it.next() {
            if c == '\x1b' {
                match it.next().expect("escape introducer") {
                    ']' => {
                        let osc: String = it.by_ref().take_while(|c| *c != '\x07').collect();
                        let target = osc.strip_prefix("8;;").expect("only OSC 8 is expected");
                        if target.is_empty() {
                            assert!(self.target.take().is_some(), "unmatched close");
                        } else {
                            assert!(self.target.replace(target.into()).is_none(), "nested OSC");
                        }
                    }
                    '[' => {
                        let mut args = String::new();
                        let end = loop {
                            let c = it.next().expect("complete CSI");
                            if ('@'..='~').contains(&c) {
                                break c;
                            }
                            args.push(c);
                        };
                        match end {
                            'H' | 'f' => {
                                let mut n =
                                    args.split(';').map(|s| s.parse::<usize>().unwrap_or(1));
                                self.y = n.next().unwrap_or(1).saturating_sub(1);
                                self.x = n.next().unwrap_or(1).saturating_sub(1);
                            }
                            'm' => self.sgr(&args),
                            'J' if args == "2" => self.cells.fill(Observed::default()),
                            'J' if args.is_empty() || args == "0" => {
                                let start = (self.y * self.width + self.x).min(self.cells.len());
                                self.cells[start..].fill(Observed::default());
                            }
                            'h' | 'l' if args.starts_with('?') => {}
                            _ => panic!("unexpected CSI {args}{end}"),
                        }
                    }
                    other => panic!("unexpected escape {other:?}"),
                }
            } else {
                assert!(!c.is_control(), "unexpected control {c:?}");
                let width = c.width().unwrap_or(0);
                if width > 0 && self.x < self.width && self.y < self.height {
                    self.cells[self.y * self.width + self.x] = Observed {
                        glyph: c.to_string(),
                        target: self.target.clone(),
                        fg: self.fg,
                        bg: self.bg,
                        bold: self.bold,
                    };
                }
                self.x += width;
            }
        }
        assert!(self.target.is_none(), "draw leaked OSC state");
    }
    fn sgr(&mut self, args: &str) {
        let n: Vec<u16> = args.split(';').map(|s| s.parse().unwrap_or(0)).collect();
        let mut i = 0;
        while i < n.len() {
            match n[i] {
                0 => {
                    self.fg = Color::Reset;
                    self.bg = Color::Reset;
                    self.bold = false;
                }
                1 => self.bold = true,
                22 => self.bold = false,
                39 => self.fg = Color::Reset,
                49 => self.bg = Color::Reset,
                38 | 48 => {
                    let fg = n[i] == 38;
                    let color = match n.get(i + 1) {
                        Some(2) => {
                            let c = Color::Rgb(n[i + 2] as u8, n[i + 3] as u8, n[i + 4] as u8);
                            i += 4;
                            c
                        }
                        Some(5) => {
                            let c = Color::Indexed(n[i + 2] as u8);
                            i += 2;
                            c
                        }
                        _ => panic!("unexpected SGR {args}"),
                    };
                    if fg {
                        self.fg = color;
                    } else {
                        self.bg = color;
                    }
                }
                30..=37 | 90..=97 | 40..=47 | 100..=107 => {
                    let fg = n[i] < 40 || (90..=97).contains(&n[i]);
                    let index = if n[i] >= 100 {
                        n[i] - 100 + 8
                    } else if n[i] >= 90 {
                        n[i] - 90 + 8
                    } else if n[i] >= 40 {
                        n[i] - 40
                    } else {
                        n[i] - 30
                    };
                    let colors = [
                        Color::Black,
                        Color::Red,
                        Color::Green,
                        Color::Yellow,
                        Color::Blue,
                        Color::Magenta,
                        Color::Cyan,
                        Color::Gray,
                        Color::DarkGray,
                        Color::LightRed,
                        Color::LightGreen,
                        Color::LightYellow,
                        Color::LightBlue,
                        Color::LightMagenta,
                        Color::LightCyan,
                        Color::White,
                    ];
                    if fg {
                        self.fg = colors[index as usize];
                    } else {
                        self.bg = colors[index as usize];
                    }
                }
                // Other modifiers are irrelevant to this widget's link style.
                2..=9 | 21 | 23..=29 | 59 => {}
                other => panic!("unexpected SGR {other} in {args}"),
            }
            i += 1;
        }
    }
    fn compare(&self, expected: &Buffer) {
        for y in 0..expected.area.height {
            for x in 0..expected.area.width {
                let cell = &expected[(x, y)];
                let (symbol, target) = glyph(cell.symbol());
                let seen = &self.cells[y as usize * self.width + x as usize];
                // Empty cells not sent by the initial diff are terminal-default blanks.
                let actual = if seen.glyph.is_empty() {
                    " "
                } else {
                    &seen.glyph
                };
                assert_eq!(actual, symbol, "glyph at {x},{y}");
                assert_eq!(seen.target.as_deref(), target, "target at {x},{y}");
                assert_eq!(seen.bg, cell.bg, "background at {x},{y}");
                assert_eq!(
                    seen.bold,
                    cell.modifier.contains(Modifier::BOLD),
                    "bold at {x},{y}"
                );
                if cell.fg == Color::Reset {
                    assert_eq!(seen.fg, Color::Reset, "reset foreground at {x},{y}");
                }
                if target.is_some() {
                    assert_eq!(
                        (seen.fg, seen.bg, seen.bold),
                        (cell.fg, cell.bg, true),
                        "link SGR at {x},{y}"
                    );
                }
            }
        }
    }
}
#[cfg(unix)]
fn run_k3_with_pty() {
    use std::io::Read;
    use std::os::fd::FromRawFd;
    use std::process::{Command, Stdio};

    let mut master = -1;
    let mut slave = -1;
    let size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // The child owns the PTY slave as stdout; crossterm can query its size even
    // when the test runner has no controlling terminal.
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
    // SAFETY: openpty returned two newly owned file descriptors.
    let mut master = unsafe { std::fs::File::from_raw_fd(master) };
    let slave = unsafe { std::fs::File::from_raw_fd(slave) };
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "conformance_20_9e_styled_osc8::k3_crossterm_incremental_frames_and_middle_overlay",
            "--nocapture",
        ])
        .env("RUSTAIN_K3_PTY_CHILD", "1")
        .env_remove("NO_COLOR")
        .stdin(Stdio::null())
        .stdout(Stdio::from(slave))
        .output()
        .unwrap();
    let mut stdout = Vec::new();
    // Linux reports EIO rather than EOF once the slave closes.
    if let Err(err) = master.read_to_end(&mut stdout) {
        assert_eq!(err.raw_os_error(), Some(libc::EIO));
    }
    let stdout = String::from_utf8_lossy(&stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed; 0 failed"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn k3_crossterm_incremental_frames_and_middle_overlay() {
    // Ratatui's fixed-viewport resize queries the real terminal size while
    // clearing. Isolate a PTY (and NO_COLOR) from the parallel test process.
    #[cfg(unix)]
    if std::env::var_os("RUSTAIN_K3_PTY_CHILD").is_none() {
        run_k3_with_pty();
        return;
    }
    #[cfg(not(unix))]
    if std::env::var("NO_COLOR").is_ok_and(|value| !value.is_empty()) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "conformance_20_9e_styled_osc8::k3_crossterm_incremental_frames_and_middle_overlay",
                "--nocapture",
            ])
            .env_remove("NO_COLOR")
            .output()
            .unwrap();
        assert!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let sink = Sink::default();
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(sink.clone()),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
        },
    )
    .unwrap();
    let theme = Theme::dark();
    let mut observer = Observer::new(80, 24);
    let original = format!("https://auth.example/{}A{}", "p".repeat(80), "q".repeat(80));
    let mut state = browser(&original, LOCAL);
    enum Edit {
        Cover(Rect),
        Glyph(u16, u16),
    }
    let mut draw = |state: &LoginOverlayState,
                    edit: Option<Edit>,
                    terminal: &mut Terminal<CrosstermBackend<Sink>>| {
        let mut expected = None;
        terminal
            .draw(|f| {
                login_dialog::render(f, f.area(), state, &theme, true);
                match edit {
                    Some(Edit::Cover(rect)) => {
                        f.render_widget(Clear, rect);
                        f.render_widget(
                            Paragraph::new("X").style(Style::default().fg(Color::Red)),
                            rect,
                        );
                    }
                    Some(Edit::Glyph(x, y)) => {
                        let cell = &mut f.buffer_mut()[(x, y)];
                        let (old, target) = glyph(cell.symbol());
                        let target = target.expect("interior cell is linked");
                        let replacement = if old == "X" { "Y" } else { "X" };
                        let symbol = format!("\x1b]8;;{target}\x07{replacement}\x1b]8;;\x07");
                        cell.set_symbol(&symbol);
                    }
                    None => {}
                }
                expected = Some(f.buffer_mut().clone());
            })
            .unwrap();
        observer.replay(&sink.take());
        let expected = expected.unwrap();
        observer.compare(&expected);
        expected
    };
    let first = draw(&state, None, &mut terminal);
    let positions = links(&first, &theme);
    let auth_cells: Vec<_> = positions
        .iter()
        .filter(|(_, _, t)| t == &original)
        .collect();
    assert!(auth_cells.len() > 8);
    draw(&state, None, &mut terminal);
    // Change only a truncated-away target segment.
    let changed_target = original.replace('A', "B");
    state = browser(&changed_target, LOCAL);
    let changed = draw(&state, None, &mut terminal);
    for cell in &auth_cells {
        assert_eq!(
            glyph(changed[(cell.0, cell.1)].symbol()).1,
            Some(changed_target.as_str())
        );
    }
    assert_eq!(
        projection(&first),
        projection(&changed),
        "only the hidden target changed"
    );
    // Change exactly one visible cell without changing its URL, style, or width.
    let middle = auth_cells[auth_cells.len() / 2];
    let altered = draw(&state, Some(Edit::Glyph(middle.0, middle.1)), &mut terminal);
    let differences = changed
        .content
        .iter()
        .zip(&altered.content)
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(differences, 1, "only the interior glyph changes");
    let restored = draw(&state, None, &mut terminal);
    assert_eq!(restored, changed);
    state = browser("https://auth.example/a", LOCAL);
    let short = draw(&state, None, &mut terminal);
    let pos = links(&short, &theme);
    let row: Vec<_> = pos
        .iter()
        .filter(|(_, _, t)| t == "https://auth.example/a")
        .collect();
    let middle = row[row.len() / 2];
    draw(
        &state,
        Some(Edit::Cover(Rect::new(middle.0, middle.1, 1, 1))),
        &mut terminal,
    );
    draw(&state, None, &mut terminal);
    terminal.resize(Rect::new(0, 0, 60, 16)).unwrap();
    draw(&state, None, &mut terminal);
    terminal.resize(Rect::new(0, 0, 80, 24)).unwrap();
    draw(&state, None, &mut terminal);
    state.active = false;
    draw(&state, None, &mut terminal);
}
