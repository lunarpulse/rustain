use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use ed25519_dalek::{SigningKey, VerifyingKey};

use crate::domain::clock::Clock;

use crate::domain::models::{
    AgentId, Budget, CapabilityFlag, CapabilityToken, CapabilityTokenId, DelegateRequest,
    JournaledTerminalCheckpoint, LedgerConservationRecord, PeerId, PeerIdentity,
};
use crate::domain::ports::AuthorityError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConservationSnapshot {
    pub total: Budget,
    pub available: Budget,
    pub live_reservations: Budget,
    pub consumed: Budget,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActiveChainFacts {
    depth: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
    trusted_issuers: HashMap<PeerId, VerifyingKey>,
    /// Highest trusted wall-clock value observed by an authority operation.
    authority_time_ms: u64,
    /// Terminal entries that are settled/revoked but still have children: their
    /// proof is stashed so the last child's prune retries the parent. Without
    /// this, a parent settled before its children leaks in the map forever.
    pending_prune: BTreeMap<CapabilityTokenId, JournaledTerminalCheckpoint>,
    /// Story 17.2c (D4): conservation-head snapshots staged under the lock,
    /// awaiting a write-ahead flush through the `LedgerJournalSink`. Drained by
    /// `journal_head`. Never pushed when no sink is attached.
    outbox: Vec<LedgerConservationRecord>,
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
    clock: Arc<dyn Clock>,
    /// Story 17.2c (D4): durable conservation-head recorder. `Some` only after
    /// `with_journal_sink` at the composition root. A `domain/ports` trait, so
    /// `domain/services` still imports nothing from `infrastructure/`.
    sink: Option<Arc<dyn crate::domain::ports::LedgerJournalSink>>,
    /// Test-only ledger-lock acquisition counter. Deterministically proves RC-3
    /// mutant (b) ("validate-then-mutate across two lock acquisitions") RED:
    /// every mutator acquires `lock_state()` exactly once, so a two-acquisition
    /// regression reads 2 where the single-lock discipline reads 1. Not a
    /// `std::sync` lock, so the `MAX_KNOWN_STD_SYNC_LOCKS` ratchet is unaffected.
    #[cfg(any(test, feature = "test-instrumentation"))]
    lock_acquisitions: std::sync::atomic::AtomicU64,
}
impl AuthorityLedger {
    /// Construct a ledger for a signed authority root and register the sole
    /// trust anchor that may attest cross-process tokens.
    pub fn new_with_trusted_issuer(
        root: CapabilityToken,
        issuer: &PeerIdentity,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, AuthorityError> {
        let ledger = Self::new(root.clone(), clock);
        ledger.trust_issuer(issuer)?;
        ledger.verify_signed_token(&root)?;
        Ok(ledger)
    }

    /// Register an explicitly trusted issuer. Trust establishment is a caller
    /// policy decision (for example a handshake-pinned PeerIdentity); this
    /// method validates the PeerId/public-key binding before storing it.
    pub fn trust_issuer(&self, issuer: &PeerIdentity) -> Result<(), AuthorityError> {
        issuer
            .verify_binding()
            .map_err(|_| AuthorityError::Malformed {
                reason: "trusted issuer identity binding is invalid",
            })?;
        let key_bytes: [u8; 32] =
            issuer
                .public_key
                .as_slice()
                .try_into()
                .map_err(|_| AuthorityError::Malformed {
                    reason: "trusted issuer public key length is invalid",
                })?;
        let key = VerifyingKey::from_bytes(&key_bytes).map_err(|_| AuthorityError::Malformed {
            reason: "trusted issuer public key is invalid",
        })?;
        self.lock_state()
            .trusted_issuers
            .insert(issuer.peer_id.clone(), key);
        Ok(())
    }

    fn verify_signed_token(&self, token: &CapabilityToken) -> Result<(), AuthorityError> {
        let state = self.lock_state();
        Self::verify_signed_token_locked(&state, token)
    }
}

impl AuthorityLedger {
    pub fn new(root: CapabilityToken, clock: Arc<dyn Clock>) -> Self {
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
            clock,
            sink: None,
            #[cfg(any(test, feature = "test-instrumentation"))]
            lock_acquisitions: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Bind the durable conservation-head recorder (Story 17.2c / D4). Mirrors
    /// `Supervisor::with_journal`; called at the composition root before the
    /// ledger is shared. The ledger stays synchronous — the sink is touched only
    /// by the async `journal_head` flush, never under the state lock.
    #[must_use]
    pub fn with_journal_sink(
        mut self,
        sink: Arc<dyn crate::domain::ports::LedgerJournalSink>,
    ) -> Self {
        self.sink = Some(sink);
        self
    }

    fn snapshot_of(entry: &LedgerEntry) -> LedgerConservationRecord {
        LedgerConservationRecord {
            token: entry.token.id,
            total: entry.total,
            available: entry.available,
            consumed: entry.consumed,
            uses_remaining: entry.uses_remaining,
            settled: entry.settled,
            revoked: entry.revoked,
        }
    }

    /// Stage the conservation head of `id` and its ancestor chain into the
    /// outbox for a later write-ahead flush (17-2c D4). Walking to the root
    /// captures the settle-propagation that rolls a child's consumption up to
    /// its parents, so the ROOT head (always present after a restart) reflects
    /// every settled descendant. No-op unless a sink is attached, so the
    /// 49-assertion conservation matrix (no sink) is behaviorally untouched.
    fn stage_head_locked(state: &mut AuthorityState, id: &CapabilityTokenId) {
        let mut current = Some(*id);
        while let Some(token_id) = current {
            let (snapshot, parent) = match state.entries.get(&token_id) {
                Some(entry) => (Self::snapshot_of(entry), entry.token.parent),
                None => break,
            };
            current = parent;
            state.outbox.push(snapshot);
        }
    }

    /// Write-ahead flush (17-2c D4). Drains the staged conservation records and
    /// journals each through the sink OUTSIDE the state lock (so
    /// `clippy::await_holding_lock` stays clean and the leaf lock never
    /// `.await`s). Callers invoke this after a budget/grant mutation, BEFORE the
    /// side-effect (spawn/settle) is externally observable. NOT a background
    /// drain — the caller awaits it (party ruling fork 2: a drain window would
    /// resurrect spent budget / double-count a grant on recovery).
    pub async fn journal_head(&self) -> Result<(), crate::domain::ports::LedgerJournalError> {
        let Some(sink) = self.sink.clone() else {
            return Ok(());
        };
        let mut pending = {
            let mut state = self.lock_state();
            std::mem::take(&mut state.outbox).into_iter()
        };
        while let Some(record) = pending.next() {
            if let Err(error) = sink.journal_conservation(record.clone()).await {
                let mut unflushed = Vec::with_capacity(pending.len() + 1);
                unflushed.push(record);
                unflushed.extend(pending);
                let mut state = self.lock_state();
                unflushed.append(&mut state.outbox);
                state.outbox = unflushed;
                return Err(error);
            }
        }
        Ok(())
    }

    /// Recovery replay (17-2c D4): restore each token's conservation head from
    /// the journaled snapshots (latest per token wins — records arrive in
    /// journal order). Idempotent; a snapshot for a token absent from the fresh
    /// ledger (a dead child never re-delegated) is skipped — its budget is
    /// reclaimed, which fails safe. The root head, always present, is restored
    /// so spent budget cannot silently reappear across a restart.
    pub fn recover_conservation(
        &self,
        records: impl IntoIterator<Item = LedgerConservationRecord>,
    ) {
        let mut state = self.lock_state();
        for record in records {
            if let Some(entry) = state.entries.get_mut(&record.token) {
                entry.available = record.available;
                entry.consumed = record.consumed;
                entry.uses_remaining = record.uses_remaining;
                entry.settled = record.settled;
                entry.revoked = record.revoked;
            }
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
        let mut state = self.lock_state();
        let now_ms = self.observe_authority_time_locked(&mut state);
        Self::delegate_locked(&mut state, parent, req, self.sink.is_some(), now_ms)
    }

    /// Delegate and attest the new grant for a cross-process recipient. The
    /// token id is computed before signing and remains the ledger key; callers
    /// register the issuer at receiving ledgers through [`Self::trust_issuer`].
    pub fn delegate_signed(
        &self,
        parent: &CapabilityToken,
        req: DelegateRequest,
        signing_key: &SigningKey,
        issuer: PeerId,
    ) -> Result<CapabilityToken, AuthorityError> {
        let mut state = self.lock_state();
        let now_ms = self.observe_authority_time_locked(&mut state);
        let child = Self::delegate_locked(&mut state, parent, req, self.sink.is_some(), now_ms)?;
        let mut signed = child.clone();
        signed.sign(signing_key, issuer);
        debug_assert_eq!(signed.id, child.id);
        state
            .entries
            .get_mut(&signed.id)
            .ok_or(AuthorityError::NotFound)?
            .token = signed.clone();
        Ok(signed)
    }

    pub fn validate(
        &self,
        token: &CapabilityToken,
        want: &CapabilityFlag,
        scope: &AgentId,
    ) -> Result<(), AuthorityError> {
        self.validate_inner(token, want, scope, true)
    }

    /// Admission check for a delegation/coordination action (spawning a
    /// sub-wave), NOT a leaf tool use. Identical to [`Self::validate`] EXCEPT it
    /// does not consult `uses_remaining`: `delegate` (the actual operation this
    /// gate fronts) never checks the parent's use-count, so gating admission on
    /// it would refuse a coordinator that has already run one tool batch while
    /// the delegation itself would still succeed. Mirrors `delegate`'s
    /// admission surface (revoked/settled/TTL/budget/ancestor-revoked).
    pub fn validate_delegation(
        &self,
        token: &CapabilityToken,
        want: &CapabilityFlag,
        scope: &AgentId,
    ) -> Result<(), AuthorityError> {
        self.validate_inner(token, want, scope, false)
    }

    fn validate_inner(
        &self,
        token: &CapabilityToken,
        want: &CapabilityFlag,
        scope: &AgentId,
        count_uses: bool,
    ) -> Result<(), AuthorityError> {
        let mut state = self.lock_state();
        let now_ms = self.observe_authority_time_locked(&mut state);
        Self::validate_active_chain_locked(&state, token, now_ms)?;
        let entry = state
            .entries
            .get(&token.id)
            .ok_or(AuthorityError::NotFound)?;
        if &entry.token.scope != scope {
            return Err(AuthorityError::Malformed {
                reason: "scope mismatch",
            });
        }
        if !entry.token.capabilities.contains(*want) {
            return Err(AuthorityError::Denied { flag: *want });
        }
        if count_uses && entry.uses_remaining == Some(0) {
            return Err(AuthorityError::BudgetExhausted);
        }
        if entry.available.requests == 0 || entry.available.cost_micros == 0 {
            return Err(AuthorityError::BudgetExhausted);
        }
        Ok(())
    }

    pub fn consume(&self, id: &CapabilityTokenId, amount: Budget) -> Result<(), AuthorityError> {
        let mut state = self.lock_state();
        let token = state
            .entries
            .get(id)
            .ok_or(AuthorityError::NotFound)?
            .token
            .clone();
        let now_ms = self.observe_authority_time_locked(&mut state);
        Self::validate_active_chain_locked(&state, &token, now_ms)?;
        let entry = state.entries.get_mut(id).ok_or(AuthorityError::NotFound)?;
        if entry.uses_remaining == Some(0) || !amount.is_within(entry.available) {
            return Err(AuthorityError::BudgetExhausted);
        }
        entry.available = entry.available - amount;
        entry.consumed += amount;
        if let Some(uses) = &mut entry.uses_remaining {
            *uses = uses.saturating_sub(1);
        }
        if self.sink.is_some() {
            Self::stage_head_locked(&mut state, id);
        }
        Ok(())
    }

    /// Debit coordinator overhead without spending a leaf-action use.
    ///
    /// Delegation gates and deterministic synthesis are orchestration
    /// accounting, not tool invocations. They still consume both budget
    /// dimensions and obey revocation/settlement, but must not make a
    /// uses-exhausted coordinator unable to delegate.
    pub fn debit_budget(
        &self,
        id: &CapabilityTokenId,
        amount: Budget,
    ) -> Result<(), AuthorityError> {
        let mut state = self.lock_state();
        let token = state
            .entries
            .get(id)
            .ok_or(AuthorityError::NotFound)?
            .token
            .clone();
        let now_ms = self.observe_authority_time_locked(&mut state);
        Self::validate_active_chain_locked(&state, &token, now_ms)?;
        let entry = state.entries.get_mut(id).ok_or(AuthorityError::NotFound)?;
        if !amount.is_within(entry.available) {
            return Err(AuthorityError::BudgetExhausted);
        }
        entry.available = entry.available - amount;
        entry.consumed += amount;
        if self.sink.is_some() {
            Self::stage_head_locked(&mut state, id);
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
        Self::settle_locked(&mut state, id)?;
        if self.sink.is_some() {
            Self::stage_head_locked(&mut state, id);
        }
        Ok(())
    }

    /// Bounded-memory GC guarded by an fsynced terminal-checkpoint proof.
    /// Settlement performs the conservation transfer; pruning only removes the
    /// now-inert maps and never creates another refund.
    pub fn prune_terminal(
        &self,
        terminal: &JournaledTerminalCheckpoint,
    ) -> Result<bool, AuthorityError> {
        let checkpoint = terminal.checkpoint();
        debug_assert!(checkpoint.state.is_terminal());
        let mut state = self.lock_state();
        Self::try_prune_locked(&mut state, &checkpoint.token, terminal)
    }

    /// Prune one settled/revoked terminal entry, cascading to a parent whose
    /// last child this was. A terminal entry that still has children stashes its
    /// proof so a later child prune retries it (P7 — a parent settled before its
    /// children no longer leaks forever).
    fn try_prune_locked(
        state: &mut AuthorityState,
        token: &CapabilityTokenId,
        proof: &JournaledTerminalCheckpoint,
    ) -> Result<bool, AuthorityError> {
        let checkpoint = proof.checkpoint();
        let Some(entry) = state.entries.get(token) else {
            state.pending_prune.remove(token);
            return Ok(false);
        };
        if entry.token.parent.is_none() {
            return Ok(false);
        }
        if entry.token.scope != checkpoint.id {
            return Err(AuthorityError::Malformed {
                reason: "terminal checkpoint scope/token mismatch",
            });
        }
        if !entry.settled && !entry.revoked {
            return Ok(false);
        }
        if state
            .children
            .get(token)
            .is_some_and(|children| !children.is_empty())
        {
            // Terminal + settled but children remain: remember the proof so the
            // last child's prune can retry this parent.
            state.pending_prune.insert(*token, proof.clone());
            return Ok(false);
        }

        let entry = state
            .entries
            .remove(token)
            .expect("entry existence checked above");
        state.scope_to_token.remove(&entry.token.scope);
        state.children.remove(token);
        state.pending_prune.remove(token);
        if let Some(parent) = entry.token.parent {
            let parent_now_empty = if let Some(siblings) = state.children.get_mut(&parent) {
                siblings.remove(token);
                siblings.is_empty()
            } else {
                false
            };
            if parent_now_empty {
                state.children.remove(&parent);
                // The parent may have been terminal-but-blocked on this child.
                if let Some(parent_proof) = state.pending_prune.remove(&parent) {
                    Self::try_prune_locked(state, &parent, &parent_proof)?;
                }
            }
        }
        Ok(true)
    }

    /// AC4/AC9: consume one use at the point of use. `validate()` checks the
    /// remaining count; this commits it (uses consumed at invoke are not
    /// refunded). Revoked/settled tokens are rejected; no budget debit.
    pub fn spend_use(&self, id: &CapabilityTokenId) -> Result<(), AuthorityError> {
        let mut state = self.lock_state();
        let token = state
            .entries
            .get(id)
            .ok_or(AuthorityError::NotFound)?
            .token
            .clone();
        let now_ms = self.observe_authority_time_locked(&mut state);
        Self::validate_active_chain_locked(&state, &token, now_ms)?;
        let entry = state.entries.get_mut(id).ok_or(AuthorityError::NotFound)?;
        if entry.uses_remaining == Some(0) {
            return Err(AuthorityError::BudgetExhausted);
        }
        if let Some(uses) = &mut entry.uses_remaining {
            *uses -= 1;
        }
        if self.sink.is_some() {
            Self::stage_head_locked(&mut state, id);
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

    fn observe_authority_time_locked(&self, state: &mut AuthorityState) -> u64 {
        let wall_ms = self.clock.wall_now_ms().max(0) as u64;
        state.authority_time_ms = state.authority_time_ms.max(wall_ms);
        state.authority_time_ms
    }

    fn verify_signed_token_locked(
        state: &AuthorityState,
        token: &CapabilityToken,
    ) -> Result<(), AuthorityError> {
        if token.malformed_signature_state() {
            return Err(AuthorityError::Malformed {
                reason: "issuer and signature must be present together",
            });
        }
        let Some(issuer) = token.issuer.as_ref() else {
            return Ok(());
        };
        let key = state
            .trusted_issuers
            .get(issuer)
            .ok_or(AuthorityError::Malformed {
                reason: "signed token issuer is not trusted",
            })?;
        token.verify(key).map_err(|_| AuthorityError::Malformed {
            reason: "signed token cryptographic verification failed",
        })
    }

    fn validate_active_chain_locked(
        state: &AuthorityState,
        token: &CapabilityToken,
        now_ms: u64,
    ) -> Result<ActiveChainFacts, AuthorityError> {
        let max_iter = state.entries.len() + 1;
        let mut visited = BTreeSet::new();
        let mut current = Some(token.id);
        let mut child_id = None;
        let mut depth = 0usize;

        while let Some(id) = current {
            if !visited.insert(id) || visited.len() > max_iter {
                return Err(AuthorityError::Malformed {
                    reason: "authority token parent-chain cycle detected",
                });
            }
            let entry = state.entries.get(&id).ok_or(AuthorityError::NotFound)?;
            if entry.revoked || entry.settled {
                return Err(AuthorityError::Revoked);
            }
            if entry
                .token
                .not_after
                .is_some_and(|not_after| now_ms > not_after)
            {
                return Err(AuthorityError::Expired);
            }
            if id == token.id && entry.token != *token {
                return Err(AuthorityError::Malformed {
                    reason: "authority token does not match ledger entry",
                });
            }
            if entry.token.id != id || entry.token.compute_id() != id {
                return Err(AuthorityError::Malformed {
                    reason: "authority token identity is inconsistent",
                });
            }
            if state.scope_to_token.get(&entry.token.scope) != Some(&id) {
                return Err(AuthorityError::Malformed {
                    reason: "authority token scope index is inconsistent",
                });
            }
            Self::verify_signed_token_locked(state, &entry.token)?;
            if let Some(child) = child_id {
                if !state
                    .children
                    .get(&id)
                    .is_some_and(|children| children.contains(&child))
                {
                    return Err(AuthorityError::Malformed {
                        reason: "authority token child index is inconsistent",
                    });
                }
                let child_token = &state
                    .entries
                    .get(&child)
                    .ok_or(AuthorityError::NotFound)?
                    .token;
                match (child_token.not_after, entry.token.not_after) {
                    (_, None) => {}
                    (Some(child_expiry), Some(parent_expiry)) if child_expiry <= parent_expiry => {}
                    _ => {
                        return Err(AuthorityError::NonSubset { dimension: "ttl" });
                    }
                }
                depth += 1;
            }
            child_id = Some(id);
            current = entry.token.parent;
        }

        Ok(ActiveChainFacts { depth })
    }

    fn delegate_locked(
        state: &mut AuthorityState,
        parent: &CapabilityToken,
        req: DelegateRequest,
        stage_head: bool,
        now_ms: u64,
    ) -> Result<CapabilityToken, AuthorityError> {
        if req.scope.as_str().is_empty() || !req.scope.is_local() || req.scope == AgentId::root() {
            return Err(AuthorityError::Malformed {
                reason: "scope must be one non-root local AgentId",
            });
        }
        let facts = Self::validate_active_chain_locked(state, parent, now_ms)?;
        let attempted_depth = facts.depth + 1;
        let parent_entry = state
            .entries
            .get(&parent.id)
            .ok_or(AuthorityError::NotFound)?
            .clone();
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
        state
            .entries
            .get_mut(&parent.id)
            .ok_or(AuthorityError::NotFound)?
            .available = parent_entry.available - child.budget;
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
        if stage_head {
            Self::stage_head_locked(state, &child.id);
        }
        Ok(child)
    }
    fn lock_state(&self) -> std::sync::MutexGuard<'_, AuthorityState> {
        #[cfg(any(test, feature = "test-instrumentation"))]
        self.lock_acquisitions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.state.lock().unwrap_or_else(|err| err.into_inner())
    }

    /// Test-only: total `lock_state()` acquisitions observed so far. RC-3 mutant
    /// (b) guard — see [`Self::lock_acquisitions`].
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn lock_acquisition_count(&self) -> u64 {
        self.lock_acquisitions
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Test-only: reset the acquisition counter before exercising one mutator.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn reset_lock_acquisition_count(&self) {
        self.lock_acquisitions
            .store(0, std::sync::atomic::Ordering::Relaxed);
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

#[cfg(test)]
mod signed_token_ledger_tests {
    use super::*;
    use crate::domain::models::agent_id::AgentId;
    use ed25519_dalek::SigningKey;

    fn signed_root(scope: &str) -> (CapabilityToken, SigningKey, PeerIdentity) {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let identity =
            PeerIdentity::from_public_key(key.verifying_key().to_bytes().to_vec()).unwrap();
        let mut token = CapabilityToken::r1_root(AgentId::parse(scope).unwrap());
        token.sign(&key, identity.peer_id.clone());
        (token, key, identity)
    }

    #[test]
    fn receiver_ledger_accepts_trusted_signed_token() {
        let (token, _key, identity) = signed_root("peer/agent");
        let scope = token.scope.clone();
        let ledger = AuthorityLedger::new_with_trusted_issuer(
            token.clone(),
            &identity,
            std::sync::Arc::new(crate::domain::clock::SystemClock::default()),
        )
        .unwrap();
        ledger
            .validate(&token, &CapabilityFlag::Spawn, &scope)
            .expect("trusted signed token must pass the ledger gate");
    }

    #[test]
    fn delegate_signed_preserves_ledger_identity_and_verifies_at_receiver() {
        let (root, key, identity) = signed_root("peer/agent");
        let ledger = AuthorityLedger::new_with_trusted_issuer(
            root.clone(),
            &identity,
            std::sync::Arc::new(crate::domain::clock::SystemClock::default()),
        )
        .unwrap();
        let request = CapabilityToken::r1_child_request(AgentId::parse("child").unwrap());
        let expected_id = CapabilityToken::child(&root, request.clone()).id;
        let child = ledger
            .delegate_signed(&root, request, &key, identity.peer_id.clone())
            .unwrap();
        assert_eq!(
            child.id, expected_id,
            "signature must not re-key the ledger entry"
        );
        ledger
            .validate(&child, &CapabilityFlag::Spawn, &child.scope)
            .expect("trusted signed delegation must validate at the receiver");
    }

    #[test]
    fn ledger_rejects_unknown_issuer_and_tampered_signed_token() {
        let (token, _key, identity) = signed_root("peer/agent");
        let scope = token.scope.clone();
        let untrusted = AuthorityLedger::new(
            token.clone(),
            std::sync::Arc::new(crate::domain::clock::SystemClock::default()),
        );
        assert!(matches!(
            untrusted.validate(&token, &CapabilityFlag::Spawn, &scope),
            Err(AuthorityError::Malformed { .. })
        ));

        let ledger = AuthorityLedger::new_with_trusted_issuer(
            token.clone(),
            &identity,
            std::sync::Arc::new(crate::domain::clock::SystemClock::default()),
        )
        .unwrap();
        let mut tampered = token.clone();
        tampered.budget.requests += 1;
        assert!(matches!(
            ledger.validate(&tampered, &CapabilityFlag::Spawn, &scope),
            Err(AuthorityError::Malformed { .. })
        ));
    }

    #[test]
    fn ledger_still_rejects_malformed_signature_state() {
        let root = CapabilityToken::r1_root(AgentId::parse("peer/agent").unwrap());
        let ledger = AuthorityLedger::new(
            root.clone(),
            std::sync::Arc::new(crate::domain::clock::SystemClock::default()),
        );
        let mut bad = root.clone();
        bad.issuer = Some(PeerId::from_public_key(&[7u8; 32]).unwrap());
        assert!(matches!(
            ledger.validate(&bad, &CapabilityFlag::Spawn, &root.scope),
            Err(AuthorityError::Malformed { .. })
        ));
    }

    #[test]
    fn expired_immediate_parent_refuses_delegation() {
        let mut root = CapabilityToken::r1_root(AgentId::root());
        root.not_after = Some(1);
        root.id = root.compute_id();
        let ledger = AuthorityLedger::new(
            root.clone(),
            std::sync::Arc::new(crate::domain::clock::SystemClock::default()),
        );

        assert_eq!(
            ledger.delegate(
                &root,
                CapabilityToken::r1_child_request(AgentId::parse("expired-child").unwrap()),
            ),
            Err(AuthorityError::Expired),
        );
    }

    #[test]
    fn expired_signed_authority_refuses_delegation() {
        let key = SigningKey::from_bytes(&[9u8; 32]);
        let identity =
            PeerIdentity::from_public_key(key.verifying_key().to_bytes().to_vec()).unwrap();
        let mut root = CapabilityToken::r1_root(AgentId::root());
        root.not_after = Some(1);
        root.id = root.compute_id();
        root.sign(&key, identity.peer_id.clone());
        let ledger = AuthorityLedger::new_with_trusted_issuer(
            root.clone(),
            &identity,
            std::sync::Arc::new(crate::domain::clock::SystemClock::default()),
        )
        .unwrap();

        assert_eq!(
            ledger.delegate_signed(
                &root,
                CapabilityToken::r1_child_request(AgentId::parse("signed-expired").unwrap()),
                &key,
                identity.peer_id,
            ),
            Err(AuthorityError::Expired),
        );
    }
}

#[cfg(test)]
mod atomic_authority_chain_tests {
    use super::*;
    use crate::domain::clock::MockClock;
    use crate::domain::models::{CapabilitySet, DelegateConstraint};
    use std::sync::{Arc, Barrier};

    type StateFingerprint = (
        BTreeMap<CapabilityTokenId, LedgerEntry>,
        HashMap<AgentId, CapabilityTokenId>,
        BTreeMap<CapabilityTokenId, BTreeSet<CapabilityTokenId>>,
        Vec<LedgerConservationRecord>,
    );

    fn request(
        scope: &str,
        budget: Budget,
        not_after: Option<u64>,
        uses_limit: Option<u32>,
    ) -> DelegateRequest {
        let capabilities = CapabilitySet::from_flags(&[CapabilityFlag::Spawn]);
        DelegateRequest {
            scope: AgentId::parse(scope).unwrap(),
            capabilities,
            constraint: DelegateConstraint {
                allowed: capabilities,
                max_depth: 3,
                max_subset: capabilities,
            },
            budget,
            not_after,
            uses_limit,
        }
    }

    fn chain(
        root_expiry: Option<u64>,
        parent_expiry: Option<u64>,
        child_expiry: Option<u64>,
        wall_ms: i64,
    ) -> (
        Arc<AuthorityLedger>,
        Arc<MockClock>,
        CapabilityToken,
        CapabilityToken,
        CapabilityToken,
    ) {
        let clock = Arc::new(MockClock::at_wall_ms(wall_ms));
        let root = CapabilityToken::root(
            AgentId::root(),
            CapabilitySet::from_flags(&[CapabilityFlag::Spawn]),
            Budget {
                requests: 100,
                cost_micros: 100_000,
            },
            3,
            root_expiry,
            Some(100),
        );
        let ledger = Arc::new(AuthorityLedger::new(root.clone(), clock.clone()));
        let parent = ledger
            .delegate(
                &root,
                request(
                    "parent",
                    Budget {
                        requests: 20,
                        cost_micros: 20_000,
                    },
                    parent_expiry,
                    Some(20),
                ),
            )
            .unwrap();
        let child = ledger
            .delegate(
                &parent,
                request(
                    "child",
                    Budget {
                        requests: 5,
                        cost_micros: 5_000,
                    },
                    child_expiry,
                    Some(5),
                ),
            )
            .unwrap();
        (ledger, clock, root, parent, child)
    }

    fn fingerprint(ledger: &AuthorityLedger) -> StateFingerprint {
        let state = ledger.lock_state();
        (
            state.entries.clone(),
            state.scope_to_token.clone(),
            state.children.clone(),
            state.outbox.clone(),
        )
    }

    #[test]
    fn mutant_skip_ancestor_rejects_revoked_settled_missing_and_expired_ancestors() {
        for expected in [
            AuthorityError::Revoked,
            AuthorityError::Revoked,
            AuthorityError::NotFound,
            AuthorityError::Expired,
        ] {
            let (ledger, _clock, root, _parent, child) = chain(None, None, None, 10);
            {
                let mut state = ledger.lock_state();
                match expected {
                    AuthorityError::Revoked if !state.entries[&root.id].revoked => {
                        state.entries.get_mut(&root.id).unwrap().revoked = true;
                    }
                    AuthorityError::Revoked => {
                        state.entries.get_mut(&root.id).unwrap().settled = true;
                    }
                    AuthorityError::NotFound => {
                        state.entries.remove(&root.id);
                    }
                    AuthorityError::Expired => {
                        state.entries.get_mut(&root.id).unwrap().token.not_after = Some(5);
                    }
                    _ => unreachable!(),
                }
            }
            assert_eq!(ledger.spend_use(&child.id), Err(expected));
        }
    }

    #[test]
    fn child_expiry_must_narrow_an_expiring_parent() {
        let clock = Arc::new(MockClock::at_wall_ms(10));
        let mut root = CapabilityToken::r1_root(AgentId::root());
        root.not_after = Some(100);
        root.id = root.compute_id();
        let ledger = AuthorityLedger::new(root.clone(), clock);

        for (scope, not_after) in [("omitted", None), ("wider", Some(101))] {
            assert_eq!(
                ledger.delegate(
                    &root,
                    request(
                        scope,
                        Budget {
                            requests: 1,
                            cost_micros: 1_000,
                        },
                        not_after,
                        Some(1),
                    ),
                ),
                Err(AuthorityError::NonSubset { dimension: "ttl" }),
            );
        }
    }

    #[test]
    fn mutant_raw_wall_clock_cannot_revive_expired_authority_after_rollback() {
        let (ledger, clock, _root, _parent, child) = chain(Some(100), Some(100), Some(100), 10);
        clock.set_wall_anchor_ms(101);
        assert_eq!(
            ledger.validate(&child, &CapabilityFlag::Spawn, &child.scope),
            Err(AuthorityError::Expired),
        );

        clock.set_wall_anchor_ms(1);
        assert_eq!(
            ledger.delegate(
                &child,
                request(
                    "grandchild",
                    Budget {
                        requests: 1,
                        cost_micros: 1_000,
                    },
                    Some(100),
                    Some(1),
                ),
            ),
            Err(AuthorityError::Expired),
        );
        assert_eq!(
            ledger.debit_budget(
                &child.id,
                Budget {
                    requests: 1,
                    cost_micros: 1,
                },
            ),
            Err(AuthorityError::Expired),
        );
        assert_eq!(ledger.spend_use(&child.id), Err(AuthorityError::Expired));
    }

    #[test]
    fn every_expiry_refusal_preserves_indexes_budget_and_staged_head() {
        let clock = Arc::new(MockClock::at_wall_ms(10));
        let mut root = CapabilityToken::r1_root(AgentId::root());
        root.not_after = Some(5);
        root.id = root.compute_id();
        let ledger = AuthorityLedger::new(root.clone(), clock);
        {
            let mut state = ledger.lock_state();
            let head = AuthorityLedger::snapshot_of(&state.entries[&root.id]);
            state.outbox.push(head);
        }
        let before = fingerprint(&ledger);
        let key = SigningKey::from_bytes(&[11u8; 32]);
        let issuer = PeerId::from_public_key(&key.verifying_key().to_bytes()).unwrap();

        assert_eq!(
            ledger.delegate(
                &root,
                CapabilityToken::r1_child_request(AgentId::parse("delegate").unwrap()),
            ),
            Err(AuthorityError::Expired),
        );
        assert_eq!(fingerprint(&ledger), before);
        assert_eq!(
            ledger.delegate_signed(
                &root,
                CapabilityToken::r1_child_request(AgentId::parse("signed").unwrap()),
                &key,
                issuer,
            ),
            Err(AuthorityError::Expired),
        );
        assert_eq!(fingerprint(&ledger), before);
        assert_eq!(
            ledger.consume(
                &root.id,
                Budget {
                    requests: 1,
                    cost_micros: 1,
                },
            ),
            Err(AuthorityError::Expired),
        );
        assert_eq!(fingerprint(&ledger), before);
        assert_eq!(
            ledger.debit_budget(
                &root.id,
                Budget {
                    requests: 1,
                    cost_micros: 1,
                },
            ),
            Err(AuthorityError::Expired),
        );
        assert_eq!(fingerprint(&ledger), before);
        assert_eq!(ledger.spend_use(&root.id), Err(AuthorityError::Expired));
        assert_eq!(fingerprint(&ledger), before);
    }

    #[test]
    fn concurrent_revoke_and_delegate_linearize_without_live_child() {
        // Mutant (b) killer ("validate-then-mutate across two lock acquisitions"):
        // a two-lock split lets a revoke linearize between delegate's parent
        // validation and the child insert, minting a *live, non-revoked* child
        // under an already-settled parent. Under the single-lock discipline a
        // child is only ever inserted while the parent is live, so any child the
        // revoke cascade later observes is revoked (and pruned), and a delegate
        // that loses the race is refused before minting. Either way no
        // non-revoked child for the racing scope may survive. `validate(&child)`
        // alone cannot distinguish the two regimes (it returns `Revoked` from the
        // parent in both), so the discriminator is the *presence of a live child
        // entry*. The loop makes the interleave window reliably observable.
        let child_scope = AgentId::parse("racing-child").unwrap();
        for _ in 0..256 {
            let clock = Arc::new(MockClock::at_wall_ms(10));
            let root = CapabilityToken::r1_root(AgentId::root());
            let ledger = Arc::new(AuthorityLedger::new(root.clone(), clock));
            let barrier = Arc::new(Barrier::new(3));
            let revoke_ledger = ledger.clone();
            let revoke_barrier = barrier.clone();
            let revoke_id = root.id;
            let delegate_ledger = ledger.clone();
            let delegate_barrier = barrier.clone();
            let delegate_parent = root.clone();
            let delegate_scope = child_scope.clone();

            let revoke = std::thread::spawn(move || {
                revoke_barrier.wait();
                revoke_ledger.revoke(&revoke_id)
            });
            let delegate = std::thread::spawn(move || {
                delegate_barrier.wait();
                delegate_ledger.delegate(
                    &delegate_parent,
                    CapabilityToken::r1_child_request(delegate_scope),
                )
            });
            barrier.wait();
            revoke.join().unwrap().unwrap();
            let delegated = delegate.join().unwrap();

            // Linearizes to exactly one winner.
            if let Ok(ref child) = delegated {
                assert_eq!(
                    ledger.validate(child, &CapabilityFlag::Spawn, &child.scope),
                    Err(AuthorityError::Revoked),
                );
            } else {
                assert_eq!(delegated, Err(AuthorityError::Revoked));
            }

            // Mutant discriminator: no live (non-revoked) child may exist for the
            // racing scope. A two-lock delegate leaves exactly such an orphan.
            let state = ledger.lock_state();
            let live_orphan = state
                .entries
                .values()
                .any(|entry| entry.token.scope == child_scope && !entry.revoked);
            assert!(
                !live_orphan,
                "two-lock delegate left a live child under a revoked parent",
            );
            drop(state);
            // Conservation holds wherever the root head survives pruning.
            if let Ok(snapshot) = ledger.conservation(&root.id) {
                assert_eq!(
                    snapshot.available + snapshot.live_reservations + snapshot.consumed,
                    snapshot.total,
                );
            }
        }
    }

    #[test]
    fn concurrent_settle_and_budget_mutations_have_one_linearized_result() {
        for debit_only in [false, true] {
            let (ledger, _clock, root, _parent, child) = chain(None, None, None, 10);
            let barrier = Arc::new(Barrier::new(3));
            let settle_ledger = ledger.clone();
            let settle_barrier = barrier.clone();
            let settle_id = child.id;
            let mutate_ledger = ledger.clone();
            let mutate_barrier = barrier.clone();
            let mutate_id = child.id;
            let settle = std::thread::spawn(move || {
                settle_barrier.wait();
                settle_ledger.settle(&settle_id)
            });
            let mutate = std::thread::spawn(move || {
                mutate_barrier.wait();
                let amount = Budget {
                    requests: 1,
                    cost_micros: 100,
                };
                if debit_only {
                    mutate_ledger.debit_budget(&mutate_id, amount)
                } else {
                    mutate_ledger.consume(&mutate_id, amount)
                }
            });
            barrier.wait();
            settle.join().unwrap().unwrap();
            assert!(matches!(
                mutate.join().unwrap(),
                Ok(()) | Err(AuthorityError::Revoked)
            ));
            let snapshot = ledger.conservation(&root.id).unwrap();
            assert_eq!(
                snapshot.available + snapshot.live_reservations + snapshot.consumed,
                snapshot.total,
            );
        }
    }

    #[test]
    fn concurrent_revoke_and_spend_use_have_one_linearized_result() {
        let (ledger, _clock, _root, _parent, child) = chain(None, None, None, 10);
        let barrier = Arc::new(Barrier::new(3));
        let revoke_ledger = ledger.clone();
        let revoke_barrier = barrier.clone();
        let child_id = child.id;
        let spend_ledger = ledger.clone();
        let spend_barrier = barrier.clone();
        let spend_id = child.id;
        let revoke = std::thread::spawn(move || {
            revoke_barrier.wait();
            revoke_ledger.revoke(&child_id)
        });
        let spend = std::thread::spawn(move || {
            spend_barrier.wait();
            spend_ledger.spend_use(&spend_id)
        });
        barrier.wait();
        revoke.join().unwrap().unwrap();
        assert!(matches!(
            spend.join().unwrap(),
            Ok(()) | Err(AuthorityError::Revoked)
        ));
        let state = ledger.lock_state();
        let entry = &state.entries[&child.id];
        assert!(entry.revoked && entry.settled);
        assert!(matches!(entry.uses_remaining, Some(4 | 5)));
    }

    #[test]
    fn every_mutator_acquires_the_state_lock_exactly_once() {
        // RC-3 mutant (b) deterministic guard. The single-lock discipline means
        // each authority mutator validates AND mutates under ONE `lock_state()`
        // acquisition. Reintroducing a validate-then-mutate split across two
        // acquisitions (the named mutant) reads 2 here — RED, for every mutator,
        // with no threads or timing dependence. A behavioural race cannot force
        // this window because the mutex structurally serializes correct code.
        let clock = Arc::new(MockClock::at_wall_ms(10));
        let root = CapabilityToken::r1_root(AgentId::root());
        let ledger = AuthorityLedger::new(root.clone(), clock);
        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let issuer = PeerId::from_public_key(&signing_key.verifying_key().to_bytes()).unwrap();
        let small = Budget {
            requests: 1,
            cost_micros: 1,
        };

        ledger.reset_lock_acquisition_count();
        ledger
            .delegate(&root, request("ratchet-delegate", small, None, Some(1)))
            .unwrap();
        assert_eq!(
            ledger.lock_acquisition_count(),
            1,
            "delegate must acquire the ledger state lock exactly once",
        );

        ledger.reset_lock_acquisition_count();
        ledger
            .delegate_signed(
                &root,
                request("ratchet-signed", small, None, Some(1)),
                &signing_key,
                issuer,
            )
            .unwrap();
        assert_eq!(
            ledger.lock_acquisition_count(),
            1,
            "delegate_signed must acquire the ledger state lock exactly once",
        );

        ledger.reset_lock_acquisition_count();
        ledger.consume(&root.id, small).unwrap();
        assert_eq!(
            ledger.lock_acquisition_count(),
            1,
            "consume must acquire the ledger state lock exactly once",
        );

        ledger.reset_lock_acquisition_count();
        ledger.debit_budget(&root.id, small).unwrap();
        assert_eq!(
            ledger.lock_acquisition_count(),
            1,
            "debit_budget must acquire the ledger state lock exactly once",
        );

        ledger.reset_lock_acquisition_count();
        ledger.spend_use(&root.id).unwrap();
        assert_eq!(
            ledger.lock_acquisition_count(),
            1,
            "spend_use must acquire the ledger state lock exactly once",
        );
    }
}
