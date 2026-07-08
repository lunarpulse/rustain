use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;

use crate::domain::models::{
    AgentId, Budget, CapabilityFlag, CapabilityToken, CapabilityTokenId, DelegateRequest,
};
use crate::domain::ports::AuthorityError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConservationSnapshot {
    pub total: Budget,
    pub available: Budget,
    pub live_reservations: Budget,
    pub consumed: Budget,
}

#[derive(Clone, Debug)]
struct LedgerEntry {
    token: CapabilityToken,
    total: Budget,
    available: Budget,
    consumed: Budget,
    uses_remaining: Option<u32>,
    settled: bool,
    revoked: bool,
}

impl LedgerEntry {
    fn reservation_remaining(&self) -> Budget {
        self.total.saturating_sub(self.consumed)
    }
}

#[derive(Default)]
struct AuthorityState {
    entries: BTreeMap<CapabilityTokenId, LedgerEntry>,
    scope_to_token: HashMap<AgentId, CapabilityTokenId>,
    children: BTreeMap<CapabilityTokenId, BTreeSet<CapabilityTokenId>>,
}

/// Synchronous authority ledger for Story 14.2.
///
/// # R1 Authority Invariants (post-review party-mode 2026-06-21)
/// - **Lifecycle gate, not a capability boundary:** the `CapabilityFlag`s gate
///   tool *lifecycle* (revoke / TTL / use-count / budget). Per-tool capability
///   *narrowing* is an R2 refinement; in R1 every delegated child carries all
///   four flags, so the gate is decisive on lifecycle, not on flag selection.
/// - **Revoke ⇒ immediate cascade kill (no graceful drain):** a revoked token's
///   node is terminated synchronously via `cascade_kill` in the same extent.
pub struct AuthorityLedger {
    state: Mutex<AuthorityState>, // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: AuthorityState single-writer map; ADR-14-2-01
}

impl AuthorityLedger {
    pub fn new(root: CapabilityToken) -> Self {
        let mut state = AuthorityState::default();
        let id = root.id;
        state.scope_to_token.insert(root.scope.clone(), id);
        state.entries.insert(
            id,
            LedgerEntry {
                total: root.budget,
                available: root.budget,
                consumed: Budget::ZERO,
                uses_remaining: root.uses_limit,
                token: root,
                settled: false,
                revoked: false,
            },
        );
        Self {
            state: Mutex::new(state),
        }
    }

    pub fn available(&self, id: &CapabilityTokenId) -> Result<Budget, AuthorityError> {
        let state = self.lock_state();
        state
            .entries
            .get(id)
            .map(|entry| entry.available)
            .ok_or(AuthorityError::NotFound)
    }

    pub fn token_for_scope(&self, scope: &AgentId) -> Result<CapabilityTokenId, AuthorityError> {
        let state = self.lock_state();
        state
            .scope_to_token
            .get(scope)
            .copied()
            .ok_or(AuthorityError::NotFound)
    }

    pub fn scope_for_token(&self, id: &CapabilityTokenId) -> Result<AgentId, AuthorityError> {
        let state = self.lock_state();
        state
            .entries
            .get(id)
            .map(|entry| entry.token.scope.clone())
            .ok_or(AuthorityError::NotFound)
    }

    pub fn depth_of(&self, id: &CapabilityTokenId) -> Result<usize, AuthorityError> {
        let state = self.lock_state();
        Self::depth_of_locked(&state, id)
    }

    pub fn delegate(
        &self,
        parent: &CapabilityToken,
        req: DelegateRequest,
    ) -> Result<CapabilityToken, AuthorityError> {
        if req.scope.0.is_empty() || !req.scope.is_local() || req.scope == AgentId::root() {
            return Err(AuthorityError::Malformed {
                reason: "scope must be one non-root local AgentId",
            });
        }

        let mut state = self.lock_state();
        let parent_depth = Self::depth_of_locked(&state, &parent.id)?;
        let attempted_depth = parent_depth + 1;
        let parent_entry = state
            .entries
            .get(&parent.id)
            .ok_or(AuthorityError::NotFound)?
            .clone();

        if parent_entry.revoked {
            return Err(AuthorityError::Revoked);
        }
        if parent_entry.settled {
            // A settled parent has already refunded its reservation; delegating
            // from it would re-spend budget the grandparent already recovered.
            return Err(AuthorityError::Revoked);
        }
        if parent_entry.token.id != parent.id {
            return Err(AuthorityError::NotFound);
        }
        if attempted_depth > parent_entry.token.constraint.max_depth {
            return Err(AuthorityError::MaxDepthExceeded {
                limit: parent_entry.token.constraint.max_depth,
                attempted: attempted_depth,
            });
        }
        if state.scope_to_token.contains_key(&req.scope) {
            return Err(AuthorityError::Malformed {
                reason: "scope already has a token",
            });
        }

        validate_request_subset(&parent_entry.token, &req)?;
        if !req.budget.is_within(parent_entry.available) {
            return Err(AuthorityError::BudgetExhausted);
        }

        let child = CapabilityToken::child(&parent_entry.token, req);
        let parent_mut = state
            .entries
            .get_mut(&parent.id)
            .ok_or(AuthorityError::NotFound)?;
        parent_mut.available = parent_mut.available - child.budget;

        state
            .children
            .entry(parent.id)
            .or_default()
            .insert(child.id);
        state.scope_to_token.insert(child.scope.clone(), child.id);
        state.entries.insert(
            child.id,
            LedgerEntry {
                total: child.budget,
                available: child.budget,
                consumed: Budget::ZERO,
                uses_remaining: child.uses_limit,
                token: child.clone(),
                settled: false,
                revoked: false,
            },
        );

        Ok(child)
    }

    pub fn validate(
        &self,
        token: &CapabilityToken,
        want: &CapabilityFlag,
        scope: &AgentId,
    ) -> Result<(), AuthorityError> {
        if token.malformed_signature_state() {
            return Err(AuthorityError::Malformed {
                reason: "issuer and signature must be present together",
            });
        }
        if token.has_signature() {
            return Err(AuthorityError::Malformed {
                reason: "signed tokens require an R2 verifier",
            });
        }
        if &token.scope != scope {
            return Err(AuthorityError::Malformed {
                reason: "scope mismatch",
            });
        }
        if !token.capabilities.contains(*want) {
            return Err(AuthorityError::Denied { flag: *want });
        }

        let state = self.lock_state();
        let entry = state
            .entries
            .get(&token.id)
            .ok_or(AuthorityError::NotFound)?;
        if entry.revoked {
            return Err(AuthorityError::Revoked);
        }
        // TTL (AC1): deny past `not_after`. `None` means "no expiry"; `Some` is
        // an absolute epoch-millis ceiling (clock-skew-tolerant per ADR-14-2-01).
        if let Some(not_after) = token.not_after {
            let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
            if now > not_after {
                return Err(AuthorityError::Expired);
            }
        }
        // Use-count (AC1): a token that has spent all its uses is denied.
        if entry.uses_remaining == Some(0) {
            return Err(AuthorityError::BudgetExhausted);
        }
        // Budget (AC1): deny if either dimension is exhausted. OR (not AND) — a
        // one-dimension-exhausted token cannot reliably serve an action, and
        // `consume()` would reject it on that dimension anyway.
        if entry.available.requests == 0 || entry.available.cost_micros == 0 {
            return Err(AuthorityError::BudgetExhausted);
        }
        let mut current = entry.token.parent;
        while let Some(parent_id) = current {
            let parent = state
                .entries
                .get(&parent_id)
                .ok_or(AuthorityError::NotFound)?;
            if parent.revoked {
                return Err(AuthorityError::Revoked);
            }
            current = parent.token.parent;
        }
        Ok(())
    }

    pub fn consume(&self, id: &CapabilityTokenId, amount: Budget) -> Result<(), AuthorityError> {
        let mut state = self.lock_state();
        let entry = state.entries.get_mut(id).ok_or(AuthorityError::NotFound)?;
        if entry.revoked {
            return Err(AuthorityError::Revoked);
        }
        if entry.settled {
            // A settled entry has already refunded its reservation; consuming
            // it again would double-spend budget the parent already recovered.
            return Err(AuthorityError::Revoked);
        }
        if entry.uses_remaining == Some(0) {
            return Err(AuthorityError::BudgetExhausted);
        }
        if !amount.is_within(entry.available) {
            return Err(AuthorityError::BudgetExhausted);
        }
        entry.available = entry.available - amount;
        entry.consumed += amount;
        // One spend consumes one use (AC1 use-count). saturating_sub keeps the
        // ledger panic-free if a None limit is later narrowed to Some(0).
        if let Some(ref mut uses) = entry.uses_remaining {
            *uses = uses.saturating_sub(1);
        }
        Ok(())
    }

    pub fn revoke(&self, id: &CapabilityTokenId) -> Result<(), AuthorityError> {
        let mut state = self.lock_state();
        if !state.entries.contains_key(id) {
            return Err(AuthorityError::NotFound);
        }
        let mut descendants = Self::descendants_locked(&state, id);
        // Post-order: settle leaves before parents so a parent propagates a
        // `consumed` total that already includes its children's consumption
        // (AC10 conservation; pre-order would strand descendant consumption).
        descendants.reverse();
        for descendant in descendants {
            if let Some(entry) = state.entries.get_mut(&descendant) {
                entry.revoked = true;
            }
            Self::settle_locked(&mut state, &descendant)?;
        }
        Ok(())
    }

    pub fn revoke_scope(&self, scope: &AgentId) -> Result<(), AuthorityError> {
        let id = self.token_for_scope(scope)?;
        self.revoke(&id)
    }

    pub fn settle(&self, id: &CapabilityTokenId) -> Result<(), AuthorityError> {
        let mut state = self.lock_state();
        Self::settle_locked(&mut state, id)
    }

    /// AC4/AC9: consume one use at the point of use. `validate()` checks the
    /// remaining count; this commits it (uses consumed at invoke are not
    /// refunded). Revoked/settled tokens are rejected; no budget debit.
    pub fn spend_use(&self, id: &CapabilityTokenId) -> Result<(), AuthorityError> {
        let mut state = self.lock_state();
        let entry = state.entries.get_mut(id).ok_or(AuthorityError::NotFound)?;
        if entry.revoked || entry.settled {
            return Err(AuthorityError::Revoked);
        }
        if entry.uses_remaining == Some(0) {
            return Err(AuthorityError::BudgetExhausted);
        }
        if let Some(ref mut uses) = entry.uses_remaining {
            *uses -= 1;
        }
        Ok(())
    }

    pub fn conservation(
        &self,
        id: &CapabilityTokenId,
    ) -> Result<ConservationSnapshot, AuthorityError> {
        let state = self.lock_state();
        let root = state.entries.get(id).ok_or(AuthorityError::NotFound)?;
        let mut live_reservations = Budget::ZERO;
        let mut consumed = root.consumed;
        for descendant_id in Self::descendants_locked(&state, id) {
            if descendant_id == *id {
                continue;
            }
            let entry = state
                .entries
                .get(&descendant_id)
                .ok_or(AuthorityError::NotFound)?;
            if entry.settled {
                continue;
            }
            live_reservations += entry.reservation_remaining();
            consumed += entry.consumed;
        }
        Ok(ConservationSnapshot {
            total: root.total,
            available: root.available,
            live_reservations,
            consumed,
        })
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, AuthorityState> {
        self.state.lock().unwrap_or_else(|err| err.into_inner())
    }

    fn depth_of_locked(
        state: &AuthorityState,
        id: &CapabilityTokenId,
    ) -> Result<usize, AuthorityError> {
        let max_iter = state.entries.len() + 1;
        let mut depth = 0usize;
        let mut current = state
            .entries
            .get(id)
            .ok_or(AuthorityError::NotFound)?
            .token
            .parent;
        while let Some(parent_id) = current {
            depth += 1;
            if depth > max_iter {
                // Defensive: a corrupted parent-chain cycle would otherwise
                // spin forever under the Mutex. Mirror the node-tree ancestors cap.
                return Err(AuthorityError::Malformed {
                    reason: "authority token parent-chain cycle detected",
                });
            }
            current = state
                .entries
                .get(&parent_id)
                .ok_or(AuthorityError::NotFound)?
                .token
                .parent;
        }
        Ok(depth)
    }

    fn descendants_locked(
        state: &AuthorityState,
        id: &CapabilityTokenId,
    ) -> Vec<CapabilityTokenId> {
        let mut out = Vec::new();
        let mut stack = vec![*id];
        while let Some(current) = stack.pop() {
            out.push(current);
            if let Some(children) = state.children.get(&current) {
                for child in children {
                    stack.push(*child);
                }
            }
        }
        out
    }

    fn settle_locked(
        state: &mut AuthorityState,
        id: &CapabilityTokenId,
    ) -> Result<(), AuthorityError> {
        let (parent_id, refund, consumed) = {
            let entry = state.entries.get_mut(id).ok_or(AuthorityError::NotFound)?;
            if entry.settled {
                return Ok(());
            }
            entry.settled = true;
            (
                entry.token.parent,
                entry.reservation_remaining(),
                entry.consumed,
            )
        };

        if let Some(parent_id) = parent_id {
            if let Some(parent) = state.entries.get_mut(&parent_id) {
                parent.available += refund;
                parent.consumed += consumed;
            }
        }
        Ok(())
    }
}

impl CapabilityToken {
    pub fn depth(&self, ledger: &AuthorityLedger) -> Result<usize, AuthorityError> {
        ledger.depth_of(&self.id)
    }
}

fn validate_request_subset(
    parent: &CapabilityToken,
    req: &DelegateRequest,
) -> Result<(), AuthorityError> {
    if !req.capabilities.is_subset_of(parent.capabilities) {
        return Err(AuthorityError::NonSubset {
            dimension: "capabilities",
        });
    }
    if !req.capabilities.is_subset_of(parent.constraint.allowed) {
        return Err(AuthorityError::NonSubset {
            dimension: "allowed",
        });
    }
    if !req
        .constraint
        .allowed
        .is_subset_of(parent.constraint.allowed)
    {
        return Err(AuthorityError::NonSubset {
            dimension: "constraint.allowed",
        });
    }
    if !req
        .constraint
        .max_subset
        .is_subset_of(parent.constraint.max_subset)
    {
        return Err(AuthorityError::NonSubset {
            dimension: "constraint.max_subset",
        });
    }
    if !req.budget.is_within(parent.budget) {
        return Err(AuthorityError::NonSubset {
            dimension: "budget",
        });
    }
    match (req.uses_limit, parent.uses_limit) {
        (_, None) => {}
        (Some(child), Some(parent)) if child <= parent => {}
        _ => return Err(AuthorityError::NonSubset { dimension: "uses" }),
    }
    match (req.not_after, parent.not_after) {
        (_, None) => {}
        (Some(child), Some(parent)) if child <= parent => {}
        _ => return Err(AuthorityError::NonSubset { dimension: "ttl" }),
    }
    if req.constraint.max_depth > parent.constraint.max_depth {
        return Err(AuthorityError::NonSubset { dimension: "depth" });
    }
    Ok(())
}
