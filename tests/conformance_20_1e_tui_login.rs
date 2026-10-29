//! Sign-in from the TUI: `/login`, Ctrl+X I and Enter on a locked model-picker
//! column drive the production login engines through `TuiLoginUi`, the sign-in
//! overlay and the live provider layer (unlock, bloom, periodic refresh,
//! models.dev remerge). Every stored row is written by `flows::login` against a
//! loopback stub; waits are on events or stub requests, never on time.
#![cfg(all(feature = "test-instrumentation", feature = "openai"))]

#[path = "conformance_20_1e_tui_login/bloom.rs"]
mod bloom;
#[path = "conformance_20_1e_tui_login/browser.rs"]
mod browser;
#[path = "conformance_20_1e_tui_login/commands.rs"]
mod commands;
#[path = "conformance_20_1e_tui_login/common.rs"]
pub(crate) mod common;
#[path = "conformance_20_1e_tui_login/device.rs"]
mod device;
#[path = "conformance_20_1e_tui_login/inherited.rs"]
mod inherited;
#[path = "conformance_20_1e_tui_login/picker.rs"]
mod picker;
#[path = "conformance_20_1e_tui_login/small_terminal.rs"]
mod small_terminal;
