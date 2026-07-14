use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use ed25519_dalek::{SigningKey, VerifyingKey};

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
    trusted_issuers: HashMap<PeerId, VerifyingKey>,
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
    /// Story 17.2c (D4): durable conservation-head recorder. `Some` only after
    /// `with_journal_sink` at the composition root. A `domain/ports` trait, so
    /// `domain/services` still imports nothing from `infrastructure/`.
    sink: Option<Arc<dyn crate::domain::ports::LedgerJournalSink>>,
}
impl AuthorityLedger {
    /// Construct a ledger for a signed authority root and register the sole
    /// trust anchor that may attest cross-process tokens.
    pub fn new_with_trusted_issuer(
        root: CapabilityToken,
        issuer: &PeerIdentity,
    ) -> Result<Self, AuthorityError> {
        let ledger = Self::new(root.clone());
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
        if token.malformed_signature_state() {
            return Err(AuthorityError::Malformed {
                reason: "issuer and signature must be present together",
            });
        }
        let Some(issuer) = token.issuer.as_ref() else {
            return Ok(());
        };
        let key = self
            .lock_state()
            .trusted_issuers
            .get(issuer)
            .cloned()
            .ok_or(AuthorityError::Malformed {
                reason: "signed token issuer is not trusted",
            })?;
        token.verify(&key).map_err(|_| AuthorityError::Malformed {
            reason: "signed token cryptographic verification failed",
        })
    }
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
            sink: None,
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
        self.verify_signed_token(parent)?;
        if req.scope.as_str().is_empty() || !req.scope.is_local() || req.scope == AgentId::root() {
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

        if self.sink.is_some() {
            Self::stage_head_locked(&mut state, &child.id);
        }
        Ok(child)
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
        let child = self.delegate(parent, req)?;
        let mut signed = child.clone();
        signed.sign(signing_key, issuer);
        debug_assert_eq!(signed.id, child.id);
        let mut state = self.lock_state();
        let entry = state
            .entries
            .get_mut(&signed.id)
            .ok_or(AuthorityError::NotFound)?;
        entry.token = signed.clone();
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
        // Signed cross-process grants are trusted only after the ledger has
        // resolved their issuer through an explicitly registered PeerIdentity
        // and verified the Ed25519 attestation. Hash self-consistency alone is
        // not authority: an attacker can recompute a hash.
        self.verify_signed_token(token)?;
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
        if entry.revoked || entry.settled {
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
        // Use-count (AC1): a token that has spent all its uses is denied its
        // next LEAF action. Skipped for delegation admission (`count_uses`
        // false): coordinating a sub-wave is not a leaf use.
        if count_uses && entry.uses_remaining == Some(0) {
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
        let ledger = AuthorityLedger::new_with_trusted_issuer(token.clone(), &identity).unwrap();
        ledger
            .validate(&token, &CapabilityFlag::Spawn, &scope)
            .expect("trusted signed token must pass the ledger gate");
    }

    #[test]
    fn delegate_signed_preserves_ledger_identity_and_verifies_at_receiver() {
        let (root, key, identity) = signed_root("peer/agent");
        let ledger = AuthorityLedger::new_with_trusted_issuer(root.clone(), &identity).unwrap();
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
        let untrusted = AuthorityLedger::new(token.clone());
        assert!(matches!(
            untrusted.validate(&token, &CapabilityFlag::Spawn, &scope),
            Err(AuthorityError::Malformed { .. })
        ));

        let ledger = AuthorityLedger::new_with_trusted_issuer(token.clone(), &identity).unwrap();
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
        let ledger = AuthorityLedger::new(root.clone());
        let mut bad = root.clone();
        bad.issuer = Some(PeerId::from_public_key(&[7u8; 32]).unwrap());
        assert!(matches!(
            ledger.validate(&bad, &CapabilityFlag::Spawn, &root.scope),
            Err(AuthorityError::Malformed { .. })
        ));
    }
}
