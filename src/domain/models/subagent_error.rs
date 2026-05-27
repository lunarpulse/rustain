use thiserror::Error;

#[non_exhaustive]
#[derive(Error, Debug)]
pub enum SubagentError {
    #[error("spawn limit exceeded: {kind:?} limit={limit} attempted={attempted}")]
    SpawnLimitExceeded {
        kind: SpawnLimitKind,
        limit: usize,
        attempted: usize,
    },

    #[error("sandbox policy widens parent: dimension={dimension}")]
    PolicyWidensParent {
        dimension: String,
        child_request: String,
        parent_ceiling: String,
    },

    #[error("parent context budget exceeded: used={used} ceiling={ceiling}")]
    ParentContextBudgetExceeded { used: u32, ceiling: u32 }, // reserved for Story 10.7; do not construct here

    #[error("runner panicked: {0}")]
    Panicked(String),

    #[error("cancelled")]
    Cancelled,

    #[error("internal: {0}")]
    Internal(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnLimitKind {
    Depth,
    Children,
}
