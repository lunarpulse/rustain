use serde::{Deserialize, Serialize};

/// Opaque identifier for a checkpoint in a conversation.
///
/// In Story 4-3a, `CheckpointId` maps 1:1 with `message_index` via
/// `CheckpointId(message_index as u64)`.  Full checkpoint creation
/// protocol (create_checkpoint on each turn) is implemented in Story 4-3b.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CheckpointId(pub u64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_checkpoint_id_ord() {
        let a = CheckpointId(0);
        let b = CheckpointId(1);
        let c = CheckpointId(2);
        assert!(a < b);
        assert!(b < c);
        assert!(a < c);
    }

    #[test]
    fn test_checkpoint_id_eq() {
        let a = CheckpointId(42);
        let b = CheckpointId(42);
        let c = CheckpointId(43);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn test_checkpoint_id_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(CheckpointId(1));
        set.insert(CheckpointId(2));
        set.insert(CheckpointId(1)); // duplicate
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_checkpoint_id_serde_roundtrip() {
        let original = CheckpointId(99);
        let json = serde_json::to_string(&original).unwrap();
        let decoded: CheckpointId = serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);
    }
}
