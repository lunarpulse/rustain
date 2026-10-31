//! Story 21.11 conformance: display width, grapheme and cell correctness.
//!
//! One CI-wired module in the `tui` area binary (`tests/tui/main.rs`). Each
//! keystone drives the real public front door of its surface, paints into a real
//! `ratatui` buffer and reads the buffer (and backend cursor) back; none builds
//! an expected row by hand from the formula under test. Fixtures are
//! `G` (ZWJ family, 2 cells), `C` (CJK, 2 cells) and `A` (combining accent,
//! 1 cell) plus ASCII positive controls. See `support.rs`.
//!
//! K1 (shared units) lives as unit tests beside `text_geometry`, `modal`,
//! `sidebar` and `chat_pane::word_wrap`; K8 is the one native 80x24 smoke in
//! `tests_tui/test_story_21_11_display_width.py`.

#[path = "conformance_21_11_display_width/support.rs"]
mod support;

#[path = "conformance_21_11_display_width/k2_composer.rs"]
mod k2_composer;

#[path = "conformance_21_11_display_width/k3_live_text.rs"]
mod k3_live_text;

#[path = "conformance_21_11_display_width/k4_shell.rs"]
mod k4_shell;

#[path = "conformance_21_11_display_width/k5a_cards.rs"]
mod k5a_cards;

#[path = "conformance_21_11_display_width/k5b_cards.rs"]
mod k5b_cards;

#[path = "conformance_21_11_display_width/k6_modal_login.rs"]
mod k6_modal_login;

#[path = "conformance_21_11_display_width/k7_gate.rs"]
mod k7_gate;
