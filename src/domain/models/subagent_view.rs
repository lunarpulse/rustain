use crate::domain::models::{AgentId, SubagentRunStatus};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRowView {
    pub agent_id: AgentId,
    pub parent_id: AgentId,
    pub subagent_type: String,
    pub spawned_at: i64,
    pub depth: usize,
    pub current_status: SubagentRunStatus,
    pub ownership: OwnershipKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum OwnershipKind {
    #[default]
    Owned,
    Peer,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_row_view_roundtrip() {
        let view = AgentRowView {
            agent_id: AgentId::new(),
            parent_id: AgentId::root(),
            subagent_type: "code-reviewer".into(),
            spawned_at: 1000,
            depth: 1,
            current_status: SubagentRunStatus::Idle,
            ownership: OwnershipKind::Owned,
        };
        let debug_str = format!("{:?}", view);
        assert!(debug_str.contains("code-reviewer"));

        let view2 = view.clone();
        assert_eq!(view, view2);
    }

    #[test]
    fn ownership_kind_default_is_owned() {
        assert_eq!(OwnershipKind::default(), OwnershipKind::Owned);
    }
}
