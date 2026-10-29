//! `/logout` and `/accounts` in the TUI: the provider and account steps of the
//! sign-in overlay over the production `auth_accounts` service, the live
//! provider layer's relock and re-serve after each change, the disabled-entry
//! account rule, and the command routes. Mutation evidence writes rows by TUI
//! `/login` through `flows::login` against a loopback stub; legacy-row cases
//! and the read-only ordering fixture seed their rows directly. Waits are on
//! events or stub requests, never on time.
#![cfg(all(feature = "test-instrumentation", feature = "openai"))]

#[path = "conformance_20_1e2_tui_accounts/commands.rs"]
mod commands;
#[path = "conformance_20_1e2_tui_accounts/common.rs"]
pub(crate) mod common;
#[path = "conformance_20_1e2_tui_accounts/disabled.rs"]
mod disabled;
#[path = "conformance_20_1e2_tui_accounts/logout.rs"]
mod logout;
#[path = "conformance_20_1e2_tui_accounts/ordering.rs"]
mod ordering;
#[path = "conformance_20_1e2_tui_accounts/select.rs"]
mod select;
