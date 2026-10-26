//! Story 19.18d — per-peer credential binding, A2A front-door keystones.
//!
//! Every keystone enters through the production front door (`A21`): a written
//! `.rustain/a2a.json` (roster under `agents` + a `server` block) is parsed by
//! `parse_workspace_a2a_config` and `parse_workspace_a2a_server_config` and
//! resolved by the one production resolver, `resolve_server_auth`. ⛔ No
//! hand-built `A2aServerAuth` with a binding (unreachable by construction),
//! no direct `items_*` call, no constructed `KeyBinding`.
//!
//! Keystones live in submodules under `tests/conformance_19_18d/`.

#![cfg(feature = "a2a")]

#[path = "conformance_19_18d/k07_resolver.rs"]
mod k07_resolver;

#[allow(clippy::duplicate_mod)] // private copy per root, as when each root was its own crate
#[path = "conformance_19_18d/k09_journal.rs"]
mod k09_journal;

#[path = "conformance_19_18d/process_runs.rs"]
mod process_runs;

#[allow(clippy::duplicate_mod)] // private copy per root, as when each root was its own crate
#[path = "conformance_19_18d/harness.rs"]
mod harness;

#[path = "conformance_19_18d/k02_bound_principals.rs"]
mod k02_bound_principals;

#[path = "conformance_19_18d/k03_k04_unbound_and_gate.rs"]
mod k03_k04_unbound_and_gate;

#[path = "conformance_19_18d/k08_sender_surfaces.rs"]
mod k08_sender_surfaces;

#[path = "conformance_19_18d/k06_restart.rs"]
mod k06_restart;

#[path = "conformance_19_18d/k05_adoption_orphans.rs"]
mod k05_adoption_orphans;

#[path = "conformance_19_18d/k11_k14_consent_policy.rs"]
mod k11_k14_consent_policy;

#[path = "conformance_19_18d/k19_20b_rap_one_way.rs"]
mod k19_20b_rap_one_way;

#[path = "conformance_19_18d/k19_18f_cross_rail.rs"]
mod k19_18f_cross_rail;
