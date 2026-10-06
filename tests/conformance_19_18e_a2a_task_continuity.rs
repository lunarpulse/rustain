//! Story 19.18e — pre-binding task continuity, A2A front-door keystones.
//!
//! Every keystone enters through the production front door (`A11`): a written
//! `.rustain/a2a.json` (roster under `agents` + a `server` block) is parsed by
//! both production parsers and resolved by `resolve_server_auth`, served by
//! `serve` on `0.0.0.0:0` with rcgen TLS, and read by an anchored raw JSON-RPC
//! client presenting a key. A binding change is a crash restart
//! (`BoundHost::restart_after_crash`), so the new `serve` reconciles exactly
//! what a killed process left on disk. ⛔ No direct `scoped_task` /
//! `get_in_scope` / `tasks_get` call, no hand-inserted task, no hand-built key
//! set, no loopback bind.
//!
//! The 19.18d harness is included by path (it carries `#![allow(dead_code)]`);
//! the keystones live under `tests/conformance_19_18e/`. Every assertion
//! message starts with its AC tag, so a mutant's RED log names the property.

#![cfg(feature = "a2a")]

#[path = "conformance_19_18d/harness.rs"]
mod harness;

#[path = "conformance_19_18e/k01_adopted_tasks.rs"]
mod k01_adopted_tasks;

#[path = "conformance_19_18e/k02_k03_opt_in_and_precedence.rs"]
mod k02_k03_opt_in_and_precedence;

#[path = "conformance_19_18e/k05_capability.rs"]
mod k05_capability;

use harness::{RawClient, Reply, detail, wire_state};
use rustain::adapters::a2a::exec::RESTART_DETAIL;

/// A task id no test ever submits: the not-found control.
const NEVER: &str = "e-never-submitted";

/// `reply` is the recovered restart projection: `failed` with exactly
/// [`RESTART_DETAIL`].
fn assert_restart_projection(reply: &Reply, tag: &str) {
    assert_eq!(wire_state(reply), "failed", "{tag}: {}", reply.body);
    assert_eq!(detail(reply), Some(RESTART_DETAIL), "{tag}: {}", reply.body);
}

/// `tasks/get` and `tasks/cancel` of `task` answer bodies byte-identical to a
/// never-submitted id, with the same JSON-RPC id and capability argument.
async fn assert_never_bytes(
    client: &RawClient,
    rpc: u64,
    task: &str,
    capability: Option<&str>,
    tag: &str,
) {
    let never = client.tasks_get(rpc, NEVER, capability).await.body;
    assert!(never.contains("-32001"), "{tag} control: {never}");
    assert_eq!(
        client.tasks_get(rpc, task, capability).await.body,
        never,
        "{tag} tasks/get {task}: not-found bytes"
    );
    assert_eq!(
        client.tasks_cancel(rpc, task, capability).await.body,
        client.tasks_cancel(rpc, NEVER, capability).await.body,
        "{tag} tasks/cancel {task}: not-found bytes"
    );
}
