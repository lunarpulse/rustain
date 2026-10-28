//! Several stored accounts per provider: equality upsert, the durable active
//! row, per-row logout and the accounts service (20-1d5a), plus the inherited
//! stored-row origin, login-only, identity-claim and per-account Copilot state
//! fixes, and the debug-binary two-account Codex flow. Every row is written by
//! `flows::login` against a loopback stub (`RUSTAIN_TEST_AUTH_ORIGIN`), except
//! the legacy layouts a current login can no longer produce, which are named
//! where they are written. The OpenRouter half (20-1d5b) adds its own modules.
#![cfg(all(feature = "test-instrumentation", feature = "openai"))]

#[path = "conformance_20_1d5_openrouter_accounts/accounts.rs"]
mod accounts;
#[path = "conformance_20_1d5_openrouter_accounts/cli.rs"]
mod cli;
#[path = "conformance_20_1d5_openrouter_accounts/inherited.rs"]
mod inherited;
