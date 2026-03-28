use std::io::{self, Stdout, stdout};

use anyhow::Result;
use crossterm::{
    ExecutableCommand,
    cursor::Show,
    event::{DisableFocusChange, EnableFocusChange},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::prelude::*;

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Set up the terminal for TUI rendering.
/// Enables raw mode, alternate screen, and focus change events.
pub fn setup() -> Result<Tui> {
    enable_raw_mode()?;
    let mut out = stdout();
    if let Err(e) = out.execute(EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(e.into());
    }
    if let Err(e) = out.execute(EnableFocusChange) {
        let _ = disable_raw_mode();
        return Err(e.into());
    }
    let backend = CrosstermBackend::new(out);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

/// Restore the terminal to its original state.
/// Must be called on exit, including during panic recovery.
pub fn teardown() -> Result<()> {
    disable_raw_mode()?;
    let mut out = stdout();
    out.execute(DisableFocusChange)?;
    out.execute(LeaveAlternateScreen)?;
    out.execute(Show)?;
    io::Write::flush(&mut out)?;
    Ok(())
}

/// Restore terminal using raw crossterm calls only.
/// Safe to call from panic hooks (no ratatui Terminal reference needed).
pub fn restore_terminal_raw() {
    let _ = disable_raw_mode();
    let mut out = stdout().lock();
    let _ = crossterm::execute!(out, DisableFocusChange, LeaveAlternateScreen, Show);
    let _ = io::Write::flush(&mut out);
}
