//! Class-V transcript views (Story 21.1 S2, A10; RU-18).
//!
//! A *view* is a non-notice row the user asked to see (team log/status/board,
//! `/room`, `/artifact`, `/peer` output, transparency decision rows, recipient
//! outcomes). They keep their `feedback_blocks` storage, content and
//! commit-on-present until 21-7/21-8 migrate them, but there is exactly ONE
//! production writer of that map: [`show_view`]. Views carry no actions and no
//! "active" slot — the order "the view shown last renders last" is an insertion
//! sequence stored in the block itself, so it survives tab stash/restore.

use std::collections::BTreeMap;

use crate::domain::models::{FeedbackBlock, FeedbackLevel};

/// The closed list of view producers (RU-18 class V).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViewKind {
    TeamLog,
    TeamStatus,
    TeamBoard,
    TeamThread,
    TeamSend,
    TeamRetract,
    TeamProposalStatus,
    Transparency,
    TransparencyJournal,
    RecipientContext,
    PeerRetract,
    Room,
    Artifact,
    Peer,
}

impl ViewKind {
    /// Stable label for diagnostics.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::TeamLog => "team-log",
            Self::TeamStatus => "team-status",
            Self::TeamBoard => "team-board",
            Self::TeamThread => "team-thread",
            Self::TeamSend => "team-send",
            Self::TeamRetract => "team-retract",
            Self::TeamProposalStatus => "team-proposal-status",
            Self::Transparency => "transparency",
            Self::TransparencyJournal => "transparency-journal",
            Self::RecipientContext => "recipient-context",
            Self::PeerRetract => "peer-retract",
            Self::Room => "room",
            Self::Artifact => "artifact",
            Self::Peer => "peer",
        }
    }
}

/// Show (or replace) the view `id` as the newest view of `blocks`: it renders
/// after every other view of the map.
pub fn show_view(
    blocks: &mut BTreeMap<String, FeedbackBlock>,
    kind: ViewKind,
    id: impl Into<String>,
    level: FeedbackLevel,
    message: impl Into<String>,
) {
    let id = id.into();
    let seq = blocks
        .values()
        .map(|block| block.seq)
        .max()
        .map_or(1, |newest| newest.saturating_add(1));
    tracing::trace!(view = kind.label(), id = %id, seq, "view shown");
    blocks.insert(
        id.clone(),
        FeedbackBlock {
            id,
            level,
            message: message.into(),
            seq,
        },
    );
}

/// The blocks of `blocks` in render order: oldest view first, newest last.
pub fn in_render_order(
    blocks: &BTreeMap<String, FeedbackBlock>,
) -> impl Iterator<Item = &FeedbackBlock> {
    let mut ordered: Vec<&FeedbackBlock> = blocks.values().collect();
    ordered.sort_by_key(|block| block.seq);
    ordered.into_iter()
}

/// The views that answer one composer submission and leave at the next one (21-17):
/// the `/room role` result and the `/peer` output and alarm. Every other view keeps its
/// row until it is replaced.
const RETIRED_ON_SUBMIT: [&str; 3] = [
    super::handlers::room_command::ROOM_BLOCK_ID,
    super::handlers::peer_command::PEER_BLOCK_ID,
    super::handlers::peer_command::PEER_ALARM_BLOCK_ID,
];

/// Remove the [`RETIRED_ON_SUBMIT`] views before a composer submission runs.
pub fn retire_on_submit(blocks: &mut BTreeMap<String, FeedbackBlock>) {
    for id in RETIRED_ON_SUBMIT {
        blocks.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story21_1_the_view_shown_last_renders_last_regardless_of_its_id() {
        let mut blocks = BTreeMap::new();
        show_view(
            &mut blocks,
            ViewKind::TeamStatus,
            "z-status",
            FeedbackLevel::Info,
            "one",
        );
        show_view(
            &mut blocks,
            ViewKind::TeamBoard,
            "a-board",
            FeedbackLevel::Info,
            "two",
        );
        let order: Vec<&str> = in_render_order(&blocks).map(|b| b.id.as_str()).collect();
        assert_eq!(
            order,
            ["z-status", "a-board"],
            "insertion order, not id order"
        );
        // Showing an older view again moves it to the end.
        show_view(
            &mut blocks,
            ViewKind::TeamStatus,
            "z-status",
            FeedbackLevel::Info,
            "three",
        );
        let order: Vec<&str> = in_render_order(&blocks).map(|b| b.id.as_str()).collect();
        assert_eq!(order, ["a-board", "z-status"]);
        assert_eq!(blocks["z-status"].message, "three");
    }

    #[test]
    fn story21_1_a_restored_tab_map_keeps_its_order() {
        // The seq lives in the block, so a stashed map restored later still
        // renders in its own order and a newer view lands after it.
        let mut stash = BTreeMap::new();
        show_view(
            &mut stash,
            ViewKind::Room,
            "r2",
            FeedbackLevel::Info,
            "first",
        );
        show_view(
            &mut stash,
            ViewKind::Artifact,
            "r1",
            FeedbackLevel::Info,
            "second",
        );
        let mut restored = stash.clone();
        show_view(
            &mut restored,
            ViewKind::Peer,
            "r0",
            FeedbackLevel::Info,
            "third",
        );
        let order: Vec<&str> = in_render_order(&restored).map(|b| b.id.as_str()).collect();
        assert_eq!(order, ["r2", "r1", "r0"]);
    }
}
