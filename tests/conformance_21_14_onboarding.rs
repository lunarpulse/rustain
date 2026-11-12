//! Story 21.14 conformance: the onboarding tour and the voice pass. Slice 14-A — the tour card
//! (`Surface::Tour`), `/tour`, the five real-action steps, the first-launch offer and the
//! persistence of `tour_done` — carries the keystones K1 (the offer and hermeticity), K2 (the
//! cards and their keys), K3 (the steps), K4 (key truth) and K5 (persistence and privacy), and
//! the desk-check frame export of smoke leg A. Slice 14-B adds K6 (the team onboarding defaults
//! and persistent hints) and K7 (the coach line and the step highlight) and the leg-B frame export.
//! Slice 14-C (the voice pass) adds K8 (the voice gate over every production literal, reusing the
//! 21-3 K12 scanner), K9 (the rendered CLI help of every command at every depth) and K10 (failures
//! state what happened, the next step and optional detail) and the leg-C frame export.
//!
//! One CI-wired module in the `tui` area binary (`tests/tui/main.rs`). Every fixture goes
//! through a production door: keys travel as crossterm events through the local host's decoder,
//! `/tour` goes through the production command parser and the one call the event loop's arm
//! makes, a due card opens through the tick the event loop runs, and every assertion reads
//! painted cells, the frames on the stack, the notice store or the state file. No sleeps: time is
//! a `MockClock`, the state file lives in a tempdir.
#[path = "conformance_21_14_onboarding/support.rs"]
pub(crate) mod support;

#[path = "conformance_21_14_onboarding/k1_offer.rs"]
mod k1_offer;

#[path = "conformance_21_14_onboarding/k2_cards.rs"]
mod k2_cards;

#[path = "conformance_21_14_onboarding/k3_steps.rs"]
mod k3_steps;

#[path = "conformance_21_14_onboarding/k4_truth.rs"]
mod k4_truth;

#[path = "conformance_21_14_onboarding/k5_persist.rs"]
mod k5_persist;

#[path = "conformance_21_14_onboarding/k6_team.rs"]
mod k6_team;

#[path = "conformance_21_14_onboarding/k7_coach.rs"]
mod k7_coach;

#[path = "conformance_21_14_onboarding/k8_voice_gate.rs"]
pub(crate) mod k8_voice_gate;

#[path = "conformance_21_14_onboarding/k8_copy_pins.rs"]
mod k8_copy_pins;

#[path = "conformance_21_14_onboarding/k9_cli_help.rs"]
mod k9_cli_help;

#[path = "conformance_21_14_onboarding/k10_failures.rs"]
mod k10_failures;

#[path = "conformance_21_14_onboarding/smoke_frames.rs"]
mod smoke_frames;
