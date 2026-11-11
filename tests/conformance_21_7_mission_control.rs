//! Story 21.7 conformance: the Mission Control hub (S-MC). Slice 7-A — the frame, its keys,
//! the shared list builder and the Room tab — carries the keystones K1 (open and geometry),
//! K2 (keys and route), K3 (identity), K4 (the presented-only Room poll) and the desk-check
//! frame export of smoke leg A. Slice 7-B adds K5 (the Messages tab: 21-8's A2A log re-homed)
//! and K6 (the Artifacts tab and the artifact window). Slice 7-C adds K7 (the Agents tab and the
//! agent window) and K8 (the Adapters window). K12 clause (a) is `mission-21-7-a`, clause (b)
//! `mission-21-7-b` and clause (c) `mission-21-7-c` in the one cutover scanner
//! (`conformance_21_4_modal_stack/k6_gate.rs`). Later slices add their keystones here.
//!
//! One CI-wired module in the `tui` area binary (`tests/tui/main.rs`). Every fixture goes
//! through a production door: the room journal is written by the production `NodeJournal`
//! writer and read back through the production reader and fold, keys travel as crossterm
//! events through the local host's decoder, and every assertion reads painted cells, the one
//! frame on the stack, the composer or an effect the event loop would run. No sleeps.
#[path = "conformance_21_7_mission_control/support.rs"]
pub(crate) mod support;

#[path = "conformance_21_7_mission_control/k1_open.rs"]
mod k1_open;

#[path = "conformance_21_7_mission_control/k2_keys.rs"]
mod k2_keys;

#[path = "conformance_21_7_mission_control/k3_identity.rs"]
mod k3_identity;

#[path = "conformance_21_7_mission_control/k4_room.rs"]
mod k4_room;

#[path = "conformance_21_7_mission_control/smoke_frames.rs"]
mod smoke_frames;

#[path = "conformance_21_7_mission_control/k5_messages.rs"]
mod k5_messages;

#[path = "conformance_21_7_mission_control/k6_artifacts.rs"]
mod k6_artifacts;

#[path = "conformance_21_7_mission_control/k7_agents.rs"]
mod k7_agents;

#[path = "conformance_21_7_mission_control/k8_adapters.rs"]
mod k8_adapters;
