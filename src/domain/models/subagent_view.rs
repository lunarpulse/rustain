use crate::domain::models::{AgentId, NodeState};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRowView {
    pub agent_id: AgentId,
    pub parent_id: AgentId,
    pub subagent_type: String,
    pub spawned_at: i64,
    pub depth: usize,
    pub current_status: NodeState,
    pub ownership: OwnershipKind,
    pub effective_model: String,
    pub tools_summary: String,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub turns: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OwnershipKind {
    Self_,
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
            current_status: NodeState::Created,
            ownership: OwnershipKind::Owned,
            effective_model: String::new(),
            tools_summary: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            turns: 0,
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

    #[test]
    fn ownership_kind_self_variant_exists() {
        let self_kind = OwnershipKind::Self_;
        assert_ne!(self_kind, OwnershipKind::Owned);
        assert_ne!(self_kind, OwnershipKind::Peer);
    }
}
