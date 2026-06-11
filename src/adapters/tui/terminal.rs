use std::io::{self, Stdout, stdout};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use crossterm::{
    ExecutableCommand,
    cursor::Show,
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture,
    },
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::prelude::*;

/// Whether mouse capture was enabled at setup time.
/// Used by `restore_terminal_raw` (panic/signal path) to decide whether to
/// emit DisableMouseCapture. Set atomically at startup by `setup()`.
/// Story 16.8, AC5.
static MOUSE_ENABLED: AtomicBool = AtomicBool::new(false);

/// Track whether mouse was enabled so `restore_terminal_raw` can decide.
/// Called by `setup()` after a successful EnableMouseCapture; called by
/// the event loop if the runtime changes mouse state (toggle/opt-out).
pub fn set_mouse_enabled(enabled: bool) {
    MOUSE_ENABLED.store(enabled, Ordering::Release);
}

/// Query whether mouse capture is currently active.
/// Returns `false` until `setup()` runs.
pub fn is_mouse_enabled() -> bool {
    MOUSE_ENABLED.load(Ordering::Acquire)
}

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Set up the terminal for TUI rendering.
/// Enables raw mode, alternate screen, mouse capture, and focus change events.
/// `mouse_enabled` gates mouse capture — set to false for SSH/screen-reader/tmux
/// where terminal-native text selection is preferred. Story 16.8, AC5 + AC14.
pub fn setup(mouse_enabled: bool) -> Result<Tui> {
    enable_raw_mode()?;
    let mut out = stdout();
    if let Err(e) = out.execute(EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(e.into());
    }
    if mouse_enabled {
        // S16.8: EnableMouseCapture wraps SGR (1006) + button-event (1000)
        // extension sequences. Failure is non-fatal — TUI runs without mouse.
        // CI/non-TTY environments silently noop.
        // P9: Only set MOUSE_ENABLED when capture actually succeeded.
        if out.execute(EnableMouseCapture).is_ok() {
            MOUSE_ENABLED.store(true, Ordering::Release);
        }
    }
    // P9: If subsequent setup steps fail, roll back mouse capture so
    // teardown and panic-recovery paths stay consistent.
    if let Err(e) = out.execute(EnableFocusChange) {
        if MOUSE_ENABLED.load(Ordering::Acquire) {
            let _ = out.execute(DisableMouseCapture);
            MOUSE_ENABLED.store(false, Ordering::Release);
        }
        let _ = disable_raw_mode();
        return Err(e.into());
    }
    // Enable bracketed paste so text pastes arrive as Event::Paste(String)
    // instead of a stream of individual KeyEvents.
    if let Err(e) = out.execute(EnableBracketedPaste) {
        if MOUSE_ENABLED.load(Ordering::Acquire) {
            let _ = out.execute(DisableMouseCapture);
            MOUSE_ENABLED.store(false, Ordering::Release);
        }
        let _ = disable_raw_mode();
        return Err(e.into());
    }
    let backend = CrosstermBackend::new(out);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

/// Restore the terminal to its original state.
/// Must be called on exit, including during panic recovery.
/// `mouse_enabled` must match the value passed to `setup()` (or caller can pass `is_mouse_enabled()`).
/// Story 16.8, AC5: DisableMouseCapture before DisableFocusChange to avoid
/// escape-sequence interleave.
pub fn teardown(mouse_enabled: bool) -> Result<()> {
    disable_raw_mode()?;
    let mut out = stdout();
    if mouse_enabled {
        let _ = out.execute(DisableMouseCapture);
    }
    out.execute(DisableFocusChange)?;
    out.execute(DisableBracketedPaste)?;
    out.execute(LeaveAlternateScreen)?;
    out.execute(Show)?;
    io::Write::flush(&mut out)?;
    Ok(())
}

/// Restore terminal using raw crossterm calls only.
/// Safe to call from panic hooks (no ratatui Terminal reference needed).
///
/// This is THE panic-safe path. MUST stay in sync with `teardown()`.
/// Story 16.8, AC5: includes DisableMouseCapture when mouse was enabled
/// at setup time, preventing terminal lockup on crash.
pub fn restore_terminal_raw() {
    let _ = disable_raw_mode();
    let mut out = stdout().lock();
    let mouse_was_enabled = MOUSE_ENABLED.load(Ordering::Acquire);
    if mouse_was_enabled {
        let _ = crossterm::execute!(out, DisableMouseCapture);
    }
    let _ = crossterm::execute!(
        out,
        DisableFocusChange,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        Show
    );
    let _ = io::Write::flush(&mut out);
}
