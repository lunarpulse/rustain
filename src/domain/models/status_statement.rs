//! Host-fact status statements, bounded by the member's disclosure level.

use super::{
    ReceivedStories, RecipientItemState, RecipientItemView, SemanticMessageType, StoryHolder,
    StoryKey, carries_story_list,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt::Write;

pub const NOT_SHARED: &str = "Not shared: conversations, plans, files, activity";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusDisclosure {
    #[default]
    None,
    Minimal,
    Status,
}

impl StatusDisclosure {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Status => "status",
        }
    }

    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "none" => Some(Self::None),
            "minimal" => Some(Self::Minimal),
            "status" => Some(Self::Status),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReceiptState {
    Received,
    Acknowledged,
}

impl ReceiptState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Acknowledged => "acknowledged",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldStory {
    pub key: StoryKey,
    pub state: ReceiptState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedStory {
    pub key: StoryKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<ReceiptState>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusStatement {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<StatusDisclosure>,
    pub stories: Vec<SharedStory>,
}

/// A latest live readable statement per sender and key. Borrowed from the
/// folded item; no cross-sender deduplication or aggregate cap belongs here.
#[derive(Clone, Copy, Debug)]
pub struct StoryStatement<'a> {
    pub item: &'a RecipientItemView,
    pub index: usize,
    pub story: &'a super::ReceivedStory,
}

pub fn story_statements<'a>(
    views: impl IntoIterator<Item = &'a RecipientItemView>,
) -> Vec<StoryStatement<'a>> {
    let mut items = views
        .into_iter()
        .filter(|item| {
            carries_story_list(item.message_type)
                && item.retracted_at_ms.is_none()
                && !matches!(item.state, RecipientItemState::Removed { .. })
                && matches!(item.stories, Some(ReceivedStories::Listed { .. }))
        })
        .collect::<Vec<_>>();
    items.sort_unstable_by_key(|item| item.journal_order);
    let mut seen_sender_keys = HashSet::new();
    let mut statements = Vec::new();
    for item in items.into_iter().rev() {
        let Some(ReceivedStories::Listed { stories }) = &item.stories else {
            continue;
        };
        for (index, story) in stories.iter().enumerate() {
            if !seen_sender_keys.insert((item.address.principal(), &story.key)) {
                continue;
            }
            statements.push(StoryStatement { item, index, story });
        }
    }
    statements
}

/// Per-sender supersession follows Story 19.21d D6, using the same walk as
/// recipient selection. Only this status projection filters for this host.
pub fn held_stories<'a>(views: impl IntoIterator<Item = &'a RecipientItemView>) -> Vec<HeldStory> {
    let mut held_keys = HashSet::new();
    let mut held = story_statements(views)
        .into_iter()
        .filter(|statement| {
            matches!(statement.story.holder, StoryHolder::You)
                && held_keys.insert(&statement.story.key)
        })
        .collect::<Vec<_>>();
    held.sort_unstable_by_key(|statement| (statement.item.journal_order, statement.index));
    held.into_iter()
        .take(64)
        .map(|statement| HeldStory {
            key: statement.story.key.clone(),
            state: match statement.item.state {
                RecipientItemState::Acknowledged { .. } => ReceiptState::Acknowledged,
                RecipientItemState::Received { .. } => ReceiptState::Received,
                RecipientItemState::Removed { .. } => unreachable!("removed lists excluded"),
            },
        })
        .collect()
}

pub fn compose(level: StatusDisclosure, held: &[HeldStory]) -> StatusStatement {
    if level == StatusDisclosure::None || held.is_empty() {
        return StatusStatement::nothing();
    }
    let mut seen = HashSet::new();
    let stories = held
        .iter()
        .filter(|story| seen.insert(&story.key))
        .take(64)
        .map(|story| SharedStory {
            key: story.key.clone(),
            state: (level == StatusDisclosure::Status).then_some(story.state),
        })
        .collect();
    StatusStatement {
        level: Some(level),
        stories,
    }
}

impl StatusStatement {
    pub const fn nothing() -> Self {
        Self {
            level: None,
            stories: Vec::new(),
        }
    }

    pub fn is_nothing(&self) -> bool {
        self.stories.is_empty()
    }

    /// The one distrust-boundary validator, shared by journal replay and asker.
    pub fn from_wire(value: &serde_json::Value) -> Option<Self> {
        let object = value.as_object()?;
        let entries = object.get("stories")?.as_array()?;
        if entries.len() > 64 {
            return None;
        }
        if entries.is_empty() {
            return (!object.contains_key("level")).then(Self::nothing);
        }
        let level = StatusDisclosure::from_token(object.get("level")?.as_str()?)?;
        if level == StatusDisclosure::None {
            return None;
        }
        let mut seen = HashSet::with_capacity(entries.len());
        let mut stories = Vec::with_capacity(entries.len());
        for entry in entries {
            let entry = entry.as_object()?;
            let raw_key = entry.get("key")?.as_str()?;
            let key = StoryKey::parse(raw_key).ok()?;
            if !seen.insert(raw_key) {
                return None;
            }
            let state = if level == StatusDisclosure::Status {
                Some(match entry.get("state")?.as_str()? {
                    "received" => ReceiptState::Received,
                    "acknowledged" => ReceiptState::Acknowledged,
                    _ => return None,
                })
            } else if entry.contains_key("state") {
                return None;
            } else {
                None
            };
            stories.push(SharedStory { key, state });
        }
        Some(Self {
            level: Some(level),
            stories,
        })
    }

    pub fn to_wire(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("status fields are JSON values")
    }

    pub fn text(&self) -> String {
        let Some(level) = self.level.filter(|_| !self.is_nothing()) else {
            return "No status shared.".to_owned();
        };
        let mut text = format!("Status ({}): ", level.as_str());
        for (index, story) in self.stories.iter().enumerate() {
            if index != 0 {
                text.push_str(", ");
            }
            text.push_str(story.key.as_str());
            if let Some(state) = story.state {
                let _ = write!(text, " {}", state.as_str());
            }
        }
        text
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NothingBecause {
    LevelNone,
    NoStoriesHeld,
}

/// Arrival-time consent preview. The reason never enters reply text or wire status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusPreview {
    pub statement: StatusStatement,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nothing_because: Option<NothingBecause>,
}

impl StatusPreview {
    pub fn new(level: StatusDisclosure, held: &[HeldStory]) -> Self {
        Self {
            statement: compose(level, held),
            nothing_because: if level == StatusDisclosure::None {
                Some(NothingBecause::LevelNone)
            } else if held.is_empty() {
                Some(NothingBecause::NoStoriesHeld)
            } else {
                None
            },
        }
    }
}

/// Preserve present JSON null as a malformed payload, distinct from a legacy
/// absent field. Journal replay must not turn malformed status into an Answer.
pub fn deserialize_raw_status<'de, D>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde_json::Value::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{
        Authorship, ItemAddress, ItemId, PeerId, ReceivedStory, SendAuthorization,
    };
    use serde_json::json;

    fn held(count: usize) -> Vec<HeldStory> {
        (0..count)
            .map(|i| HeldStory {
                key: StoryKey::parse(&format!("S7-{i:02}")).unwrap(),
                state: if i % 2 == 0 {
                    ReceiptState::Acknowledged
                } else {
                    ReceiptState::Received
                },
            })
            .collect()
    }

    fn item(sender: &str, order: u64, entries: &[(&str, StoryHolder)]) -> RecipientItemView {
        RecipientItemView {
            address: ItemAddress::from_a2a_ingress(
                PeerId::from_public_key(&[sender.as_bytes()[sender.len() - 1]; 32]).unwrap(),
                ItemId::from_replay(format!("ri-{order}")),
            ),
            task: format!("task-{order}"),
            alias: Some("private alias".into()),
            message_type: SemanticMessageType::StoryAssignment,
            stories: Some(ReceivedStories::Listed {
                stories: entries
                    .iter()
                    .map(|(key, holder)| ReceivedStory {
                        key: StoryKey::parse(key).unwrap(),
                        holder: holder.clone(),
                    })
                    .collect(),
            }),
            selected_for: None,
            authorship: Authorship::HumanWritten,
            send_authorization: SendAuthorization::HumanDecided,
            state: RecipientItemState::Received {
                content: "private body".into(),
            },
            journal_order: order,
            retracted_at_ms: None,
            in_reply_to: None,
            capability_hash: None,
            reply_capability_matched: false,
        }
    }

    #[test]
    fn tokens_and_order_are_closed() {
        for (token, level) in [
            ("none", StatusDisclosure::None),
            ("minimal", StatusDisclosure::Minimal),
            ("status", StatusDisclosure::Status),
        ] {
            assert_eq!(StatusDisclosure::from_token(token), Some(level));
            assert_eq!(level.as_str(), token);
        }
        for token in ["", "STATUS", "story-only", " status"] {
            assert_eq!(StatusDisclosure::from_token(token), None);
        }
        assert!(StatusDisclosure::None < StatusDisclosure::Minimal);
        assert!(StatusDisclosure::Minimal < StatusDisclosure::Status);
        assert_eq!(StatusDisclosure::default(), StatusDisclosure::None);
    }

    #[test]
    fn compose_matrix_is_bounded_and_never_adds_fields() {
        for level in [
            StatusDisclosure::None,
            StatusDisclosure::Minimal,
            StatusDisclosure::Status,
        ] {
            for count in [0, 2, 65] {
                let statement = compose(level, &held(count));
                if level == StatusDisclosure::None || count == 0 {
                    assert_eq!(statement, StatusStatement::nothing());
                    assert_eq!(statement.to_wire(), json!({"stories": []}));
                    assert!(statement.is_nothing());
                } else {
                    assert_eq!(statement.stories.len(), count.min(64));
                    assert_eq!(statement.level, Some(level));
                    for story in &statement.stories {
                        assert_eq!(story.state.is_some(), level == StatusDisclosure::Status);
                    }
                    assert_eq!(
                        StatusStatement::from_wire(&statement.to_wire()),
                        Some(statement)
                    );
                }
            }
        }
        assert_eq!(
            NOT_SHARED,
            "Not shared: conversations, plans, files, activity"
        );
    }

    #[test]
    fn fold_uses_latest_readable_list_per_sender_then_newest_holding_item() {
        let first = item(
            "sender-a",
            1,
            &[("S1", StoryHolder::You), ("S2", StoryHolder::You)],
        );
        let second = item(
            "sender-a",
            2,
            &[(
                "S1",
                StoryHolder::Member {
                    alias: "you".into(),
                    peer: None,
                },
            )],
        );
        let mut third = item(
            "sender-b",
            3,
            &[("S2", StoryHolder::You), ("S3", StoryHolder::You)],
        );
        third.state = RecipientItemState::Acknowledged {
            content: "private".into(),
        };
        let mut removed = item("sender-a", 4, &[("S2", StoryHolder::NoHolder)]);
        removed.state = RecipientItemState::Removed {
            acknowledged_before: false,
        };
        let mut retracted = item("sender-b", 5, &[("S2", StoryHolder::NoHolder)]);
        retracted.retracted_at_ms = Some(1);
        let mut unreadable = item("sender-b", 6, &[]);
        unreadable.stories = Some(ReceivedStories::Unreadable);
        let mut other_type = item("sender-b", 7, &[("S3", StoryHolder::NoHolder)]);
        other_type.message_type = SemanticMessageType::Consultation;
        let views = [
            other_type, third, second, unreadable, first, retracted, removed,
        ];
        assert_eq!(
            held_stories(views.iter()),
            vec![
                HeldStory {
                    key: StoryKey::parse("S2").unwrap(),
                    state: ReceiptState::Acknowledged
                },
                HeldStory {
                    key: StoryKey::parse("S3").unwrap(),
                    state: ReceiptState::Acknowledged
                },
            ]
        );
        let reverse = held_stories(views.iter().rev());
        assert_eq!(reverse, held_stories(views.iter()));
    }

    #[test]
    fn story19_22_scope_change_supersedes_a_holding_list() {
        let first = item("sender-a", 1, &[("S7-07", StoryHolder::You)]);
        let mut release = item("sender-a", 2, &[("S7-07", StoryHolder::NoHolder)]);
        release.message_type = SemanticMessageType::ScopeChange;
        assert_eq!(held_stories([&first]).len(), 1);
        assert!(held_stories([&first, &release]).is_empty());
        release.stories = Some(ReceivedStories::Listed {
            stories: vec![ReceivedStory {
                key: StoryKey::parse("S7-07").unwrap(),
                holder: StoryHolder::You,
            }],
        });
        assert_eq!(held_stories([&first, &release]).len(), 1);
    }

    #[test]
    fn wire_rejects_every_malformed_shape_and_canonicalizes_nothing() {
        let too_many = json!({"level": "minimal", "stories": (0..65).map(|i| json!({"key":format!("S{i}")})).collect::<Vec<_>>()});
        let invalid = vec![
            json!(null),
            json!([]),
            json!({}),
            json!({"stories": null}),
            too_many,
            json!({"level":"minimal","stories":[{"key":"bad!"}]}),
            json!({"level":"minimal","stories":[{"key":"S1"},{"key":"S1"}]}),
            json!({"stories":[{"key":"S1"}]}),
            json!({"level":"unknown","stories":[{"key":"S1"}]}),
            json!({"level":"minimal","stories":[]}),
            json!({"level":null,"stories":[]}),
            json!({"level":"status","stories":[{"key":"S1"}]}),
            json!({"level":"minimal","stories":[{"key":"S1","state":"received"}]}),
            json!({"level":"minimal","stories":[{"key":"S1","state":null}]}),
            json!({"level":"status","stories":[{"key":"S1","state":"removed"}]}),
            json!({"level":"status","stories":[{"key":1,"state":"received"}]}),
        ];
        for value in invalid {
            assert_eq!(StatusStatement::from_wire(&value), None, "{value}");
        }
        assert_eq!(
            StatusStatement::from_wire(&json!({"stories":[]})),
            Some(StatusStatement::nothing())
        );
        assert_eq!(StatusStatement::nothing().to_wire(), json!({"stories":[]}));
        for level in [StatusDisclosure::Minimal, StatusDisclosure::Status] {
            let statement = compose(level, &held(2));
            assert_eq!(
                StatusStatement::from_wire(&statement.to_wire()),
                Some(statement)
            );
        }
    }

    #[test]
    fn text_contains_only_the_allowed_host_facts() {
        assert_eq!(StatusStatement::nothing().text(), "No status shared.");
        assert_eq!(
            compose(StatusDisclosure::Minimal, &held(2)).text(),
            "Status (minimal): S7-00, S7-01"
        );
        assert_eq!(
            compose(StatusDisclosure::Status, &held(2)).text(),
            "Status (status): S7-00 acknowledged, S7-01 received"
        );
    }
}
