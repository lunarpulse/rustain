//! Sender-stated story relationships. These data grant no execution authority.

use super::{PeerId, SemanticMessageType};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;

pub const MAX_STORIES: usize = 64;
pub const MAX_STORY_ALIAS_BYTES: usize = 256;

/// The message types whose durable carrier states story relationships.
pub const fn carries_story_list(kind: SemanticMessageType) -> bool {
    matches!(
        kind,
        SemanticMessageType::StoryAssignment | SemanticMessageType::ScopeChange
    )
}

/// A sender-local key; deserialized manifests are checked at the send boundary.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StoryKey(String);

impl StoryKey {
    pub fn parse(key: &str) -> Result<Self, StoryRefusal> {
        Self::check_key(key)?;
        Ok(Self(key.to_owned()))
    }

    fn check_key(key: &str) -> Result<(), StoryRefusal> {
        if key.is_empty()
            || key.len() > 32
            || !key.as_bytes()[0].is_ascii_alphanumeric()
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(StoryRefusal::InvalidKey(key.to_owned()));
        }
        Ok(())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StoryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoryEntry {
    pub key: StoryKey,
    pub to: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoryManifest {
    pub stories: Vec<StoryEntry>,
}

impl StoryManifest {
    pub fn parse(token: &str) -> Result<Self, StoryRefusal> {
        if token.is_empty() {
            return Err(StoryRefusal::Missing);
        }
        let manifest = Self {
            stories: token
                .split(',')
                .map(|entry| {
                    let (key, to) = entry
                        .split_once('=')
                        .map_or((entry, None), |(key, alias)| (key, Some(alias.to_owned())));
                    Ok(StoryEntry {
                        key: StoryKey::parse(key)?,
                        to,
                    })
                })
                .collect::<Result<_, StoryRefusal>>()?,
        };
        manifest.check_shape()?;
        Ok(manifest)
    }

    fn check_shape(&self) -> Result<(), StoryRefusal> {
        if self.stories.is_empty() {
            return Err(StoryRefusal::Missing);
        }
        if self.stories.len() > MAX_STORIES {
            return Err(StoryRefusal::TooMany);
        }
        let mut keys = HashSet::with_capacity(self.stories.len());
        for story in &self.stories {
            StoryKey::check_key(story.key.as_str())?;
            if !keys.insert(&story.key) {
                return Err(StoryRefusal::DuplicateKey(story.key.to_string()));
            }
            if story
                .to
                .as_ref()
                .is_some_and(|alias| alias.len() > MAX_STORY_ALIAS_BYTES)
            {
                return Err(StoryRefusal::AliasTooLong(story.key.to_string()));
            }
        }
        Ok(())
    }

    pub fn check(
        &self,
        message_type: Option<SemanticMessageType>,
        recipients: &[String],
    ) -> Result<(), StoryRefusal> {
        if !message_type.is_some_and(carries_story_list) {
            return Err(StoryRefusal::WrongType);
        }
        self.check_shape()?;
        for story in &self.stories {
            if let Some(alias) = &story.to
                && (alias.is_empty() || !recipients.contains(alias))
            {
                return Err(StoryRefusal::NotAddressed {
                    key: story.key.to_string(),
                    alias: alias.clone(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoryRefusal {
    WrongType,
    Missing,
    InvalidKey(String),
    DuplicateKey(String),
    TooMany,
    NotAddressed { key: String, alias: String },
    HolderNotAddressed { key: String, alias: String },
    AliasTooLong(String),
}

impl fmt::Display for StoryRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongType => {
                f.write_str("'--stories' needs '--type story_assignment' or '--type scope_change'")
            }
            Self::Missing => f.write_str("Missing story list after '/team send --stories'"),
            Self::InvalidKey(key) => write!(
                f,
                "story key '{key}' is not valid: 1–32 characters from A–Z a–z 0–9 . _ -, starting with a letter or digit"
            ),
            Self::DuplicateKey(key) => write!(f, "story key '{key}' is listed twice"),
            Self::TooMany => f.write_str("a send lists at most 64 stories"),
            Self::NotAddressed { key, alias } => write!(
                f,
                "story '{key}' is assigned to '{alias}', who is not addressed by this send"
            ),
            Self::HolderNotAddressed { key, alias } => write!(
                f,
                "story '{key}' was assigned to '{alias}' by your earlier send; address '{alias}' too, so the change is not silent"
            ),
            Self::AliasTooLong(key) => write!(
                f,
                "story '{key}' is assigned to an alias longer than 256 bytes"
            ),
        }
    }
}

impl std::error::Error for StoryRefusal {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedStory {
    pub key: StoryKey,
    pub to: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state")]
pub enum ReceivedStories {
    Listed {
        stories: Vec<ReceivedStory>,
    },
    Unreadable,
    #[serde(other)]
    Unrecognized,
}

/// Story 19.22 `D6` — which verdicts an item's story list offers; gate and
/// surface state only, never on a wire or in the journal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VerdictOffer {
    #[default]
    None,
    DiscussOnly,
    AcceptOrDiscuss,
}

impl ReceivedStories {
    pub fn verdict_offer(&self) -> VerdictOffer {
        match self {
            Self::Listed { stories }
                if stories
                    .iter()
                    .any(|story| matches!(story.holder, StoryHolder::You)) =>
            {
                VerdictOffer::AcceptOrDiscuss
            }
            Self::Listed { .. } => VerdictOffer::DiscussOnly,
            Self::Unreadable | Self::Unrecognized => VerdictOffer::None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceivedStory {
    pub key: StoryKey,
    pub holder: StoryHolder,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum StoryHolder {
    NoHolder,
    You,
    Member {
        alias: String,
        peer: Option<PeerId>,
    },
    #[serde(other)]
    Unrecognized,
}

/// Display-only projection from a folded recipient item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StorySummary(pub String);

pub fn story_summary(list: &ReceivedStories) -> String {
    let ReceivedStories::Listed { stories } = list else {
        return "story list not readable by this build".to_owned();
    };
    let n = stories.len();
    let m = stories
        .iter()
        .filter(|story| matches!(story.holder, StoryHolder::You))
        .count();
    let noun = if n == 1 { "story" } else { "stories" };
    if m == 0 {
        format!("{n} {noun}, none assigned to you")
    } else {
        format!("{n} {noun}, {m} assigned to you")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{PeerId, SemanticMessageType};

    #[test]
    fn story19_22_list_types_and_verdict_offer_matrix() {
        for (kind, expected) in [
            (SemanticMessageType::StoryAssignment, true),
            (SemanticMessageType::ScopeChange, true),
            (SemanticMessageType::DesignUpdate, false),
            (SemanticMessageType::Consultation, false),
            (SemanticMessageType::Unknown, false),
        ] {
            assert_eq!(carries_story_list(kind), expected);
        }
        for (holders, expected) in [
            (vec![StoryHolder::You], VerdictOffer::AcceptOrDiscuss),
            (
                vec![StoryHolder::NoHolder, StoryHolder::You],
                VerdictOffer::AcceptOrDiscuss,
            ),
            (vec![StoryHolder::NoHolder], VerdictOffer::DiscussOnly),
            (
                vec![StoryHolder::Member {
                    alias: "b".into(),
                    peer: None,
                }],
                VerdictOffer::DiscussOnly,
            ),
            (vec![StoryHolder::Unrecognized], VerdictOffer::DiscussOnly),
            (vec![], VerdictOffer::DiscussOnly),
        ] {
            let list = ReceivedStories::Listed {
                stories: holders
                    .into_iter()
                    .enumerate()
                    .map(|(n, holder)| ReceivedStory {
                        key: StoryKey::parse(&format!("S{n}")).unwrap(),
                        holder,
                    })
                    .collect(),
            };
            assert_eq!(list.verdict_offer(), expected);
        }
        for list in [
            None,
            Some(ReceivedStories::Unreadable),
            Some(ReceivedStories::Unrecognized),
        ] {
            assert_eq!(
                list.as_ref()
                    .map_or(VerdictOffer::None, ReceivedStories::verdict_offer),
                VerdictOffer::None
            );
        }
        assert_eq!(
            StoryRefusal::HolderNotAddressed {
                key: "S7-07".into(),
                alias: "b".into()
            }
            .to_string(),
            "story 'S7-07' was assigned to 'b' by your earlier send; address 'b' too, so the change is not silent"
        );
    }

    #[test]
    fn key_grammar_boundaries() {
        for key in ["A", "0", "S7-04", "a._-9", &"A".repeat(32)] {
            assert!(StoryKey::parse(key).is_ok(), "{key}");
        }
        for key in ["", ".A", "-a", "_a", "a=b", "a b", "é", &"a".repeat(33)] {
            assert!(StoryKey::parse(key).is_err(), "{key}");
        }
    }

    #[test]
    fn every_refusal_has_one_rail_neutral_text() {
        let kind = Some(SemanticMessageType::StoryAssignment);
        let manifest = StoryManifest::parse("S1,S2=b").unwrap();
        assert_eq!(
            manifest.check(None, &["b".into()]).unwrap_err().to_string(),
            "'--stories' needs '--type story_assignment' or '--type scope_change'"
        );
        assert_eq!(
            manifest
                .check(Some(SemanticMessageType::DesignUpdate), &["b".into()])
                .unwrap_err()
                .to_string(),
            "'--stories' needs '--type story_assignment' or '--type scope_change'"
        );
        assert!(manifest.check(kind, &["b".into()]).is_ok());
        assert_eq!(
            manifest.check(kind, &["c".into()]).unwrap_err().to_string(),
            "story 'S2' is assigned to 'b', who is not addressed by this send"
        );
        assert_eq!(
            StoryManifest::parse("").unwrap_err().to_string(),
            "Missing story list after '/team send --stories'"
        );
        assert_eq!(
            StoryManifest::parse("bad!").unwrap_err().to_string(),
            "story key 'bad!' is not valid: 1–32 characters from A–Z a–z 0–9 . _ -, starting with a letter or digit"
        );
        assert_eq!(
            StoryManifest::parse("S1,S1=b").unwrap_err().to_string(),
            "story key 'S1' is listed twice"
        );
        assert_eq!(
            StoryManifest::parse(
                &(0..65)
                    .map(|n| format!("S{n}"))
                    .collect::<Vec<_>>()
                    .join(",")
            )
            .unwrap_err()
            .to_string(),
            "a send lists at most 64 stories"
        );
        assert_eq!(
            StoryManifest::parse(&format!("S1={}", "b".repeat(257)))
                .unwrap_err()
                .to_string(),
            "story 'S1' is assigned to an alias longer than 256 bytes"
        );
        let alias = "b".repeat(256);
        assert!(
            StoryManifest::parse(&format!("S1={alias}"))
                .unwrap()
                .check(kind, &[alias])
                .is_ok()
        );
        let raw: StoryManifest =
            serde_json::from_value(serde_json::json!({"stories":[{"key":"bad!","to":null}]}))
                .unwrap();
        assert!(
            raw.check(kind, &[]).is_err(),
            "daemon must revalidate deserialized keys"
        );
    }

    #[test]
    fn summary_counts_you_only_and_pluralizes() {
        let key = |s| StoryKey::parse(s).unwrap();
        let yours = ReceivedStory {
            key: key("S1"),
            holder: StoryHolder::You,
        };
        let member = ReceivedStory {
            key: key("S2"),
            holder: StoryHolder::Member {
                alias: "b".into(),
                peer: None,
            },
        };
        assert_eq!(
            story_summary(&ReceivedStories::Listed {
                stories: vec![yours.clone()]
            }),
            "1 story, 1 assigned to you"
        );
        assert_eq!(
            story_summary(&ReceivedStories::Listed {
                stories: vec![yours, member.clone()]
            }),
            "2 stories, 1 assigned to you"
        );
        assert_eq!(
            story_summary(&ReceivedStories::Listed {
                stories: vec![member]
            }),
            "1 story, none assigned to you"
        );
        for list in [ReceivedStories::Unreadable, ReceivedStories::Unrecognized] {
            assert_eq!(
                story_summary(&list),
                "story list not readable by this build"
            );
        }
    }

    #[test]
    fn serde_round_trips_and_future_tags_are_readable() {
        let list = ReceivedStories::Listed {
            stories: vec![ReceivedStory {
                key: StoryKey::parse("S1").unwrap(),
                holder: StoryHolder::Member {
                    alias: "b".into(),
                    peer: Some(PeerId::from_public_key(&[1; 32]).unwrap()),
                },
            }],
        };
        assert_eq!(
            serde_json::from_str::<ReceivedStories>(&serde_json::to_string(&list).unwrap())
                .unwrap(),
            list
        );
        assert_eq!(
            serde_json::from_value::<ReceivedStories>(
                serde_json::json!({"state":"Future","extra":true})
            )
            .unwrap(),
            ReceivedStories::Unrecognized
        );
        assert_eq!(
            serde_json::from_value::<StoryHolder>(
                serde_json::json!({"kind":"Future","extra":true})
            )
            .unwrap(),
            StoryHolder::Unrecognized
        );
    }
}

#[cfg(test)]
mod record_tests {
    use crate::domain::models::RoomEvent;
    #[test]
    fn story_record_round_trip_and_legacy_byte_identity() {
        // A pre-19.21d line, in field order: re-serializing it must reproduce
        // the exact bytes, not merely an equal JSON value.
        let hash = "00".repeat(32);
        let legacy = format!(
            r#"{{"event":"team_send_action_recorded","action":"sa_test","recipients":[],"message_type":"design_update","subject":"sprint-7","body_hash":"{hash}","body_bytes":4}}"#
        );
        let event: RoomEvent = serde_json::from_str(&legacy).unwrap();
        assert_eq!(serde_json::to_string(&event).unwrap(), legacy);
        let listed = legacy.replace(
            r#""body_bytes":4}"#,
            r#""body_bytes":4,"stories":[{"key":"S1","to":null},{"key":"S2","to":0}]}"#,
        );
        let event: RoomEvent = serde_json::from_str(&listed).unwrap();
        assert_eq!(serde_json::to_string(&event).unwrap(), listed);
    }
}
