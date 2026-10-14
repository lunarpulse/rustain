#![cfg(feature = "a2a")]

#[path = "conformance_19_21/attached_harness.rs"]
mod attached_harness;
#[path = "conformance_19_21/harness.rs"]
mod harness;
#[path = "conformance_19_21/k01_send_action.rs"]
mod k01_send_action;
#[path = "conformance_19_21/k02_board_rules.rs"]
mod k02_board_rules;
#[path = "conformance_19_21/k03_restart.rs"]
mod k03_restart;
#[path = "conformance_19_21/k03b_liveness.rs"]
mod k03b_liveness;
#[path = "conformance_19_21/k04_board_surface.rs"]
mod k04_board_surface;
#[path = "conformance_19_21/k04b_journal_read.rs"]
mod k04b_journal_read;
#[path = "conformance_19_21/k05_attached_lifecycle.rs"]
mod k05_attached_lifecycle;
#[path = "conformance_19_21/k05_attached_send_board.rs"]
mod k05_attached_send_board;
#[path = "conformance_19_21/k05_sender_local_privacy.rs"]
mod k05_sender_local_privacy;
#[path = "conformance_19_21/k06_attached_status_consent.rs"]
mod k06_attached_status_consent;
#[path = "conformance_19_21/k07_k08_recipient.rs"]
mod k07_k08_recipient;
