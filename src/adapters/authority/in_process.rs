use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::models::{
    AgentId, CapabilityFlag, CapabilityToken, CapabilityTokenId, DelegateRequest,
};
use crate::domain::ports::{AuthorityError, AuthorityProvider};
use crate::domain::services::authority_ledger::AuthorityLedger;
use crate::infrastructure::subagent::NodeTree;

pub struct InProcessAuthorityProvider {
    ledger: Arc<AuthorityLedger>,
    /// AC5: trust-drop `revoke(token_id)` resolves the token's scope and routes
    /// into `NodeTree::cascade_kill` so the offending node subtree is killed by
    /// the single authoritative cascade. `None` in unit tests / fixtures.
    node_tree: Option<Arc<NodeTree>>,
}

impl InProcessAuthorityProvider {
    pub fn new(ledger: Arc<AuthorityLedger>) -> Self {
        Self {
            ledger,
            node_tree: None,
        }
    }

    /// AC5: attach the node tree so trust-drop `revoke` routes into `cascade_kill`.
    #[must_use]
    pub fn with_node_tree(mut self, tree: Arc<NodeTree>) -> Self {
        self.node_tree = Some(tree);
        self
    }

    pub fn ledger(&self) -> Arc<AuthorityLedger> {
        Arc::clone(&self.ledger)
    }
}

#[async_trait]
impl AuthorityProvider for InProcessAuthorityProvider {
    async fn delegate(
        &self,
        parent: &CapabilityToken,
        req: DelegateRequest,
    ) -> Result<CapabilityToken, AuthorityError> {
        self.ledger.delegate(parent, req)
    }

    async fn validate(
        &self,
        token: &CapabilityToken,
        want: &CapabilityFlag,
        scope: &AgentId,
    ) -> Result<(), AuthorityError> {
        self.ledger.validate(token, want, scope)
    }

    async fn revoke(&self, token: &CapabilityTokenId) -> Result<(), AuthorityError> {
        // AC5: resolve token_id -> scope and route into the single authoritative
        // cascade. R6 (post-review): fail-closed if no NodeTree is attached — a
        // silent no-op cascade would leave a revoked-but-running node (the same
        // hole P6 closed for validate_authority). Production attaches the tree at
        // startup; this guard makes any non-startup path loud rather than silent.
        let scope = self.ledger.scope_for_token(token).ok();
        if scope.is_some() && self.node_tree.is_none() {
            return Err(AuthorityError::Malformed {
                reason: "node tree not attached; cannot cascade on revoke",
            });
        }
        self.ledger.revoke(token)?;
        if let (Some(tree), Some(scope)) = (&self.node_tree, scope) {
            let _ = tree
                .cascade_kill(&scope, std::time::Duration::from_millis(200))
                .await;
        }
        Ok(())
    }

    async fn settle(&self, token: &CapabilityTokenId) -> Result<(), AuthorityError> {
        self.ledger.settle(token)
    }

    async fn spend_use(&self, token: &CapabilityTokenId) -> Result<(), AuthorityError> {
        self.ledger.spend_use(token)
    }
}
