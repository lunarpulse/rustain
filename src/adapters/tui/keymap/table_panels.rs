//! Rows of the wave Chat sub-state and the wave overlay (Story 21.3 AC21, KG-2).
//!
//! They describe TODAY's hand-written arms of `app::handle_char` /
//! `handle_special_key` exactly (3-B2 moves the dispatch under the resolver and
//! must keep every by-value witness green). All are local-only surfaces. Arms
//! that no real terminal can reach (the synthetic `'\n'`, `'\r'`, `'\u{1b}'` and
//! arrow-glyph characters of `handle_char`) are not rows.
//!
//! The task drill-down, the Tasks sidebar panel and the task reorder are gone (Story 21.7,
//! 7-D): the Tasks tab, the task window and the reorder sub-context of Mission Control own
//! those keys (`table_mission.rs`), so a retained wave's `d s r Enter` in Chat never depends
//! on a reorder state.
//!
//! * One row per line on purpose (`rustfmt::skip`): the table is data.

use super::model::{Binding, CommandId, Context, Key, bind};

/// Commands of the wave Chat sub-state and the wave overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(clippy::enum_variant_names)]
#[rustfmt::skip]
pub enum PanelCmd {
    // Chat sub-state.
    WaveBack, WaveOpenOverlay, WaveOpenDiverge, WaveCloseDiverge, WaveOpenFromChat,
    // Wave overlay.
    WaveNext, WavePrev, WaveDrill, WaveClose, WaveRerun, WaveDiverge, WaveDivergeClose,
}

const fn cw(keys: &'static [Key], cmd: PanelCmd) -> Binding {
    bind(Context::ChatWave, keys, CommandId::Panel(cmd), "").local()
}
const fn w(keys: &'static [Key], cmd: PanelCmd) -> Binding {
    bind(Context::WaveOverlay, keys, CommandId::Panel(cmd), "").local()
}

#[rustfmt::skip]
pub const ROWS: &[Binding] = &[
    // ── Chat sub-state ──
    cw(&[Key::Esc], PanelCmd::WaveBack).does("Close the divergence view, else dismiss the retained wave"),
    cw(&[Key::Enter], PanelCmd::WaveOpenOverlay).does("Open the retained wave overlay"),
    cw(&[Key::Char('d')], PanelCmd::WaveOpenDiverge).does("Open the divergence view of the retained wave"),
    cw(&[Key::Char('s')], PanelCmd::WaveCloseDiverge).does("Close the divergence view of the retained wave"),
    cw(&[Key::Char('r')], PanelCmd::WaveOpenFromChat).does("Open the retained wave overlay"),
    // ── Wave overlay ──
    w(&[Key::Char('j')], PanelCmd::WaveNext).does("Select the next spoke"),
    w(&[Key::Down], PanelCmd::WaveNext).does("Select the next spoke"),
    w(&[Key::Char('k')], PanelCmd::WavePrev).does("Select the previous spoke"),
    w(&[Key::Up], PanelCmd::WavePrev).does("Select the previous spoke"),
    w(&[Key::Enter], PanelCmd::WaveDrill).does("Open the selected spoke"),
    w(&[Key::Esc], PanelCmd::WaveClose).does("Close the wave overlay"),
    w(&[Key::Ctrl('c')], PanelCmd::WaveClose).does("Close the wave overlay (back one frame)"),
    w(&[Key::Char('r')], PanelCmd::WaveRerun).does("Re-run the selected spoke"),
    w(&[Key::Char('d')], PanelCmd::WaveDiverge).does("Open the divergence view"),
    w(&[Key::Char('s')], PanelCmd::WaveDivergeClose).does("Close the divergence view"),
];
