//! Shared pure construction validator for fork-join requests (Story 17.3d-RC-B,
//! acceptance-contract RC-6-validator).
//!
//! Before RC-B, construction validity was split across two divergent sites: the
//! adapter [`crate::adapters::tui::fanout_spec::to_request`] checked only the
//! checked total + nested breadth while *generating* a valid shape, and the
//! executor `WaveCtx::validate_request` validated an arbitrary request's role
//! shape + dependency DAG — and *neither* validated concurrency bounds. A
//! request accepted by one caller could be rejected by the other (a divergence).
//!
//! [`validate_nested_request`] is the single pure function both callers now run.
//! It performs NO I/O, holds NO ledger, mints NO token, and never launches — it
//! only inspects the request and returns a typed [`OrchestrationError`] before
//! any launch, gate mint, budget debit, journal `WaveStarted`, or scratch clone.
//! Runtime authority (available budget, delegation, depth) stays in the
//! executor's `validate_coordinator`; that is authority checking, not pure
//! construction validation.

use crate::domain::models::agent_id::AgentId;
use crate::domain::models::capability_token::CapabilityToken;
use crate::domain::models::orchestration::{
    FORK_JOIN_SPAWN_CAP, MAX_NESTED_BREADTH, OrchestrationError, SpokeRole,
};
use crate::domain::ports::AuthorityError;
use crate::domain::ports::ForkJoinRequest;
use crate::domain::services::dag::{DependencyGraph, GraphError};

/// Map an authority-construction failure onto its orchestration-domain
/// equivalent. Only `Overflow` is a construction concern; every other authority
/// error at construction time is an internal invariant violation.
pub(crate) fn map_authority_construction_error(error: AuthorityError) -> OrchestrationError {
    match error {
        AuthorityError::Overflow { dimension } => OrchestrationError::Overflow { dimension },
        other => OrchestrationError::Internal(other.to_string()),
    }
}

/// Validate the top-level [`ForkJoinRequest`] *construction* — the pure,
/// launch-free subset shared by the adapter translation and the executor
/// admission path (RC-6). Each declarative coordinator's would-be nested wave
/// is validated here too (concurrency, wait-policy, and grandchild dependency
/// graph), so a malformed nested coordinator is refused at construction time —
/// before the coordinator launch, gate mint, budget debit, journal
/// `WaveStarted`, or scratch clone — not only when the coordinator later
/// materializes its nested request at runtime.
///
/// Checks, in order:
/// 1. top-level wait-policy is R1-active (`Quorum` rejected);
/// 2. the wave is non-empty;
/// 3. the top-level fan-out is within [`FORK_JOIN_SPAWN_CAP`];
/// 4. the top-level concurrency bound is within `1..=FORK_JOIN_SPAWN_CAP`;
/// 5. the total declared node count (parents + grandchildren) is representable
///    (checked arithmetic) and within [`MAX_NESTED_BREADTH`] when nested;
/// 6. each declarative coordinator is depth-two only: non-empty, within the
///    nested breadth cap, leaf-only grandchildren, R1-active nested wait-policy,
///    an in-bounds nested concurrency, its `r1_coordinator` budget arithmetic
///    (RC-A checked add/mul) does not overflow, and its grandchild dependency
///    references resolve + are acyclic;
/// 7. the top-level local dependency references resolve and the wait-for graph
///    is acyclic.
pub fn validate_nested_request(request: &ForkJoinRequest) -> Result<(), OrchestrationError> {
    // 1. Wait-policy bound (Quorum reserved & inert in R1).
    if let Some(reason) = request.wait_policy.r1_unsupported_reason() {
        return Err(OrchestrationError::WaitPolicyUnsupported(reason));
    }

    // 2. Non-empty wave.
    let attempted = request.spokes.len();
    if attempted == 0 {
        return Err(OrchestrationError::Internal(
            "fork-join requires at least one spoke".into(),
        ));
    }

    // 3. Top-level spawn cap.
    if attempted > FORK_JOIN_SPAWN_CAP {
        return Err(OrchestrationError::SpawnCapExceeded {
            cap: FORK_JOIN_SPAWN_CAP,
            attempted,
        });
    }

    // 4. Top-level concurrency bound.
    validate_concurrency(request.concurrency)?;

    // 5. Checked total nested node count.
    let nested_nodes = request.spokes.iter().try_fold(0usize, |total, spoke| {
        let descendants = match &spoke.role {
            SpokeRole::Coordinator { grandchildren, .. } => grandchildren.len(),
            SpokeRole::Leaf => 0,
        };
        total
            .checked_add(descendants)
            .ok_or(OrchestrationError::Overflow {
                dimension: "nested node count",
            })
    })?;
    let total_nodes = attempted
        .checked_add(nested_nodes)
        .ok_or(OrchestrationError::Overflow {
            dimension: "nested node count",
        })?;
    if nested_nodes > 0 && total_nodes > MAX_NESTED_BREADTH {
        return Err(OrchestrationError::NestedBreadthExceeded {
            cap: MAX_NESTED_BREADTH,
            attempted: total_nodes,
        });
    }

    // 6. Depth-two-only role shape + the coordinator's OWN nested-wave
    //    construction validity (RC-6: front-loaded before any side effect).
    for spoke in &request.spokes {
        if let SpokeRole::Coordinator {
            grandchildren,
            concurrency,
            wait_policy,
        } = &spoke.role
        {
            if grandchildren.is_empty() {
                return Err(OrchestrationError::Internal(
                    "nested coordinator requires at least one grandchild".into(),
                ));
            }
            if grandchildren.len() > MAX_NESTED_BREADTH {
                return Err(OrchestrationError::NestedBreadthExceeded {
                    cap: MAX_NESTED_BREADTH,
                    attempted: grandchildren.len(),
                });
            }
            if grandchildren
                .iter()
                .any(|grandchild| !matches!(grandchild.role, SpokeRole::Leaf))
            {
                return Err(OrchestrationError::NestedDepthUnsupported);
            }
            // Nested wave scheduling bounds — the coordinator materializes a
            // `ForkJoinRequest` from exactly these fields at runtime
            // (orchestrator `run_wave_body`); front-load its wait-policy and
            // concurrency here so a malformed coordinator cannot launch first.
            if let Some(reason) = wait_policy.r1_unsupported_reason() {
                return Err(OrchestrationError::WaitPolicyUnsupported(reason));
            }
            validate_concurrency(*concurrency)?;
            // RC-A checked arithmetic: refuse an unrepresentable coordinator
            // budget at construction time, before any launch or authorization.
            CapabilityToken::r1_coordinator(AgentId::new(), grandchildren.len())
                .map_err(map_authority_construction_error)?;
            // Grandchild dependency references + acyclicity — the nested wave
            // the coordinator will schedule on.
            validate_dependency_graph(
                grandchildren
                    .iter()
                    .map(|grandchild| (grandchild.id.clone(), grandchild.waits_for.clone())),
            )?;
        }
    }

    // 7. Top-level dependency references + acyclicity — the SAME graph the
    //    executor schedules on, so a request the adapter accepts cannot fail the
    //    executor's dependency partition.
    validate_dependency_graph(
        request
            .spokes
            .iter()
            .map(|spoke| (spoke.id.clone(), spoke.waits_for.clone())),
    )?;

    Ok(())
}

/// Concurrency bound shared by the top-level wave and every nested coordinator
/// wave: a wave must run at least one spoke and never more than
/// [`FORK_JOIN_SPAWN_CAP`] concurrently.
fn validate_concurrency(concurrency: usize) -> Result<(), OrchestrationError> {
    if concurrency == 0 || concurrency > FORK_JOIN_SPAWN_CAP {
        return Err(OrchestrationError::InvalidConcurrency {
            bound: FORK_JOIN_SPAWN_CAP,
            requested: concurrency,
        });
    }
    Ok(())
}

/// Dependency-reference resolution + acyclicity for one wave's spokes, mapping
/// the pure [`DependencyGraph`] errors onto their orchestration-domain
/// equivalents. Shared by the top-level wave and each coordinator's nested wave.
fn validate_dependency_graph(
    edges: impl IntoIterator<Item = (AgentId, Vec<AgentId>)>,
) -> Result<(), OrchestrationError> {
    let graph = DependencyGraph::new(edges).map_err(|error| match error {
        GraphError::Duplicate(id) => OrchestrationError::DuplicateSpoke(id),
        GraphError::Missing { node, dependency } => OrchestrationError::MissingDependency {
            spoke: node,
            dependency,
        },
        GraphError::Cycle => OrchestrationError::DependencyCycle,
    })?;
    graph
        .topological_waves()
        .map_err(|_| OrchestrationError::DependencyCycle)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::orchestration::{SpokeSpec, WaitPolicy};
    use crate::domain::models::{ModelTier, ToolPolicy};

    fn leaf(label: &str) -> SpokeSpec {
        SpokeSpec {
            id: AgentId::new(),
            label: label.into(),
            prompt: format!("do {label}"),
            effective_model: "test-model".into(),
            tier: ModelTier::Flagship,
            tools_allow: ToolPolicy::InheritFromParent,
            waits_for: Vec::new(),
            role: SpokeRole::Leaf,
        }
    }

    fn coordinator(label: &str, grandchildren: Vec<SpokeSpec>) -> SpokeSpec {
        let n = grandchildren.len().max(1);
        let mut spoke = leaf(label);
        spoke.role = SpokeRole::Coordinator {
            grandchildren: grandchildren.into_boxed_slice(),
            concurrency: n,
            wait_policy: WaitPolicy::All,
        };
        spoke
    }

    fn req(spokes: Vec<SpokeSpec>, concurrency: usize) -> ForkJoinRequest {
        ForkJoinRequest {
            coordinator: AgentId::root(),
            spokes,
            wait_policy: WaitPolicy::All,
            concurrency,
        }
    }

    #[test]
    fn accepts_a_valid_flat_request() {
        assert!(validate_nested_request(&req(vec![leaf("a"), leaf("b")], 2)).is_ok());
    }

    #[test]
    fn accepts_a_valid_depth_two_nested_request() {
        let c = coordinator("c", vec![leaf("g0"), leaf("g1")]);
        assert!(validate_nested_request(&req(vec![c], 1)).is_ok());
    }

    #[test]
    fn rejects_empty_wave() {
        assert!(matches!(
            validate_nested_request(&req(vec![], 1)),
            Err(OrchestrationError::Internal(_))
        ));
    }

    #[test]
    fn rejects_fan_out_above_spawn_cap() {
        let spokes: Vec<_> = (0..=FORK_JOIN_SPAWN_CAP)
            .map(|i| leaf(&format!("s{i}")))
            .collect();
        assert!(matches!(
            validate_nested_request(&req(spokes, 1)),
            Err(OrchestrationError::SpawnCapExceeded { cap, attempted })
                if cap == FORK_JOIN_SPAWN_CAP && attempted == FORK_JOIN_SPAWN_CAP + 1
        ));
    }

    #[test]
    fn rejects_zero_concurrency() {
        assert!(matches!(
            validate_nested_request(&req(vec![leaf("a")], 0)),
            Err(OrchestrationError::InvalidConcurrency { bound, requested })
                if bound == FORK_JOIN_SPAWN_CAP && requested == 0
        ));
    }

    #[test]
    fn rejects_concurrency_above_cap() {
        assert!(matches!(
            validate_nested_request(&req(vec![leaf("a")], FORK_JOIN_SPAWN_CAP + 1)),
            Err(OrchestrationError::InvalidConcurrency { bound, requested })
                if bound == FORK_JOIN_SPAWN_CAP && requested == FORK_JOIN_SPAWN_CAP + 1
        ));
    }

    #[test]
    fn rejects_quorum_wait_policy() {
        let mut r = req(vec![leaf("a")], 1);
        r.wait_policy = WaitPolicy::Quorum(1);
        assert!(matches!(
            validate_nested_request(&r),
            Err(OrchestrationError::WaitPolicyUnsupported(_))
        ));
    }

    #[test]
    fn rejects_empty_grandchildren() {
        let c = coordinator("c", vec![]);
        assert!(matches!(
            validate_nested_request(&req(vec![c], 1)),
            Err(OrchestrationError::Internal(_))
        ));
    }

    #[test]
    fn rejects_nested_breadth_over_total_cap() {
        let gc: Vec<_> = (0..MAX_NESTED_BREADTH)
            .map(|i| leaf(&format!("g{i}")))
            .collect();
        // total = 1 coordinator + MAX_NESTED_BREADTH grandchildren > cap.
        let c = coordinator("c", gc);
        assert!(matches!(
            validate_nested_request(&req(vec![c], 1)),
            Err(OrchestrationError::NestedBreadthExceeded { .. })
        ));
    }

    #[test]
    fn rejects_depth_three_role() {
        let inner = coordinator("inner", vec![leaf("g")]);
        let outer = coordinator("outer", vec![inner]);
        assert!(matches!(
            validate_nested_request(&req(vec![outer], 1)),
            Err(OrchestrationError::NestedDepthUnsupported)
        ));
    }

    #[test]
    fn rejects_duplicate_spoke_ids() {
        let a = leaf("a");
        let dup = SpokeSpec {
            id: a.id.clone(),
            ..leaf("b")
        };
        assert!(matches!(
            validate_nested_request(&req(vec![a, dup], 2)),
            Err(OrchestrationError::DuplicateSpoke(_))
        ));
    }

    #[test]
    fn rejects_missing_dependency() {
        let mut a = leaf("a");
        a.waits_for = vec![AgentId::new()];
        assert!(matches!(
            validate_nested_request(&req(vec![a], 1)),
            Err(OrchestrationError::MissingDependency { .. })
        ));
    }

    #[test]
    fn rejects_dependency_cycle() {
        let mut a = leaf("a");
        let mut b = leaf("b");
        a.waits_for = vec![b.id.clone()];
        b.waits_for = vec![a.id.clone()];
        assert!(matches!(
            validate_nested_request(&req(vec![a, b], 2)),
            Err(OrchestrationError::DependencyCycle)
        ));
    }

    fn coordinator_with(
        label: &str,
        grandchildren: Vec<SpokeSpec>,
        concurrency: usize,
        wait_policy: WaitPolicy,
    ) -> SpokeSpec {
        let mut spoke = leaf(label);
        spoke.role = SpokeRole::Coordinator {
            grandchildren: grandchildren.into_boxed_slice(),
            concurrency,
            wait_policy,
        };
        spoke
    }

    #[test]
    fn rejects_nested_zero_concurrency() {
        // RC-6 (RC-B review): a coordinator's OWN nested concurrency is
        // front-loaded — refused at construction, before the coordinator can
        // launch and materialize its nested request.
        let c = coordinator_with("c", vec![leaf("g0")], 0, WaitPolicy::All);
        assert!(matches!(
            validate_nested_request(&req(vec![c], 1)),
            Err(OrchestrationError::InvalidConcurrency { bound, requested })
                if bound == FORK_JOIN_SPAWN_CAP && requested == 0
        ));
    }

    #[test]
    fn rejects_nested_concurrency_above_cap() {
        let c = coordinator_with(
            "c",
            vec![leaf("g0")],
            FORK_JOIN_SPAWN_CAP + 1,
            WaitPolicy::All,
        );
        assert!(matches!(
            validate_nested_request(&req(vec![c], 1)),
            Err(OrchestrationError::InvalidConcurrency { requested, .. })
                if requested == FORK_JOIN_SPAWN_CAP + 1
        ));
    }

    #[test]
    fn rejects_nested_quorum_wait_policy() {
        let c = coordinator_with("c", vec![leaf("g0")], 1, WaitPolicy::Quorum(1));
        assert!(matches!(
            validate_nested_request(&req(vec![c], 1)),
            Err(OrchestrationError::WaitPolicyUnsupported(_))
        ));
    }

    #[test]
    fn rejects_grandchild_dependency_cycle() {
        let mut g0 = leaf("g0");
        let mut g1 = leaf("g1");
        g0.waits_for = vec![g1.id.clone()];
        g1.waits_for = vec![g0.id.clone()];
        let c = coordinator_with("c", vec![g0, g1], 2, WaitPolicy::All);
        assert!(matches!(
            validate_nested_request(&req(vec![c], 1)),
            Err(OrchestrationError::DependencyCycle)
        ));
    }

    #[test]
    fn rejects_grandchild_missing_dependency() {
        let mut g0 = leaf("g0");
        g0.waits_for = vec![AgentId::new()];
        let c = coordinator_with("c", vec![g0], 1, WaitPolicy::All);
        assert!(matches!(
            validate_nested_request(&req(vec![c], 1)),
            Err(OrchestrationError::MissingDependency { .. })
        ));
    }
}
