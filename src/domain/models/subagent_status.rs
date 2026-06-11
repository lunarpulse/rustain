use serde::{Deserialize, Serialize};

/// `#[non_exhaustive]` so Story 10.9 (background subprocess tier) can add
/// `AwaitingApproval` and `Lost` variants without breaking match sites, and
/// Epic 14 (A2A peer agents) can add `RemotePeer` similarly.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    Idle,
    RunningFg,
    RunningBg, // present in v0 enum but unreachable until Story 10.9
    Completed,
    Failed,
    Killed,
}
