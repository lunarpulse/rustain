//! Story 19.29 conformance target — Bash redirect targets under command
//! patterns, and skill restrictions that cross delegation.
//!
//! One target, wired into CI through the `skills` area binary (`cargo test
//! --test skills conformance_19_29_redirect_and_delegation::`):
//!
//! - `redirect`   — AC2/AC3/AC4: the scheduler front door over the real
//!   `SecurityAdapter` and the real `bash -c` toolset, plus chain-level rows for
//!   the targets that must never be executed (`~`, `$HOME`).
//! - `foreground` — AC5: the production turn door (`run_turn`), both origins,
//!   Normal mode raising no approval card.
//! - `delegation` — AC8 freshness: a turn's `task` call carries the skill
//!   restriction through the composite adapter and provider to the child spec.
//! - `plan_delegation` — AC11 (owner ruling CR-7): the plan a call proposes
//!   carries that call's restrictions; YOLO and the cards narrow its children.
//! - `rail_harness` — the production `task` stack, shared with
//!   `conformance_19_28_parent_restriction_rail` (the `a2a` area binary).

mod delegation;
mod foreground;
mod plan_delegation;
mod rail_harness;
mod redirect;
