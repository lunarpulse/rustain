use crate::domain::models::orchestration::WaitReason;
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
    /// P9 (TUI): renders the ⊙ iso indicator when true.
    pub isolated: bool,
    /// 17.5b (AC8): the typed reason a `Waiting` node is parked, so the
    /// Agents panel renders `‖ awaiting your answer` instead of the static
    /// `"waiting"`. `None` for every non-`Waiting` or unstamped node.
    pub wait_reason: Option<WaitReason>,
}

/// Wire/checkpoint serialization type — **structurally has no `Self_` variant**.
///
/// A forged `"self_"` cannot deserialize into this type (parse-don't-validate).
/// Inbound conversion `WireOwnershipKind → OwnershipKind` is total and **never
/// yields `Self_`**. Used by `NodeCheckpoint` for serialization so a checkpoint
/// cannot carry `Self_` (DD2, Winston — Story 14.6 AC4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WireOwnershipKind {
    #[default]
    Owned,
    Peer,
}

/// In-process ownership tier. `Self_` is the privileged root tier.
///
/// `Self_` carries a [`SealedSelf`] proof token whose single field is
/// private to this module — so the variant's *payload* can only be produced
/// by [`OwnershipKind::self_root()`]. Unlike a bare unit variant (which any
/// code with `OwnershipKind` in scope could write directly — `enum` variants
/// always inherit the enum's own `pub` visibility in Rust, there is no way to
/// mark one variant private), a tuple variant's field keeps its own
/// visibility, so `OwnershipKind::Self_(SealedSelf(()))` fails to compile
/// everywhere outside this module: the `()` field of `SealedSelf` is
/// private, so external code cannot construct a `SealedSelf` to fill the
/// slot, and cannot pattern-bind one out of nothing either — only match on
/// `OwnershipKind::Self_(_)`.
///
/// `Serialize` / `Deserialize` are deliberately **not derived** so the domain
/// type never rides a wire boundary that could forge `Self_`.
///
/// For serialization across wire/checkpoint boundaries, use [`WireOwnershipKind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum OwnershipKind {
    /// Privileged root tier — only constructible via [`Self::self_root()`];
    /// the payload proves the variant was minted by the sealed ctor.
    Self_(SealedSelf),
    #[default]
    Owned,
    Peer,
}

/// Zero-sized proof token that an `OwnershipKind::Self_` was minted by the
/// sealed root constructor. The single field's visibility defaults to
/// private-to-this-module, so no code outside `subagent_view` — not even a
/// sibling module in this same crate — can construct one directly; the only
/// path is [`OwnershipKind::self_root()`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SealedSelf(());

impl OwnershipKind {
    /// Sealed root constructor — the **only** path to `Self_`.
    ///
    /// `pub(crate)` ensures external crates and wire payloads cannot forge
    /// the privileged tier; the private `SealedSelf` field additionally
    /// ensures no other in-crate module can construct the payload by hand
    /// (only literal `OwnershipKind::Self_` writes elsewhere fail to compile
    /// since the tuple field is unreachable outside this module). On R2
    /// durable resume the runtime re-establishes the root in-process and
    /// rehydrates only `Owned`/`Peer` children.
    pub(crate) fn self_root() -> Self {
        Self::Self_(SealedSelf(()))
    }

    /// Convert to the wire-safe representation.
    ///
    /// The root serializes as `Owned` — self-authority is an in-process runtime
    /// fact, never persisted, never transmitted. A `Self_` arriving from storage
    /// would itself be the vulnerability (DD2, Murat).
    pub fn wire(&self) -> WireOwnershipKind {
        match self {
            Self::Self_(_) | Self::Owned => WireOwnershipKind::Owned,
            Self::Peer => WireOwnershipKind::Peer,
        }
    }
}

impl From<WireOwnershipKind> for OwnershipKind {
    /// Total conversion — **never yields `Self_`**.
    fn from(w: WireOwnershipKind) -> Self {
        match w {
            WireOwnershipKind::Owned => Self::Owned,
            WireOwnershipKind::Peer => Self::Peer,
        }
    }
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
            isolated: false,
            wait_reason: None,
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
        let self_kind = OwnershipKind::self_root();
        assert_ne!(self_kind, OwnershipKind::Owned);
        assert_ne!(self_kind, OwnershipKind::Peer);
    }
}
