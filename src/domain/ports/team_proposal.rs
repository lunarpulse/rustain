use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::models::{ProposalRaiseOutcome, TeamProposal};

/// Runtime surface for presenting a proposal for operator review.
#[async_trait]
pub trait TeamProposalSink: Send + Sync {
    async fn raise(&self, proposal: Arc<TeamProposal>) -> ProposalRaiseOutcome;
}
