//! Bounded, tolerant ingest of the sending host's story statement.

use std::collections::HashSet;

use serde_json::Value;

use crate::domain::models::story_manifest::{MAX_STORIES, MAX_STORY_ALIAS_BYTES};
use crate::domain::models::{
    PeerId, ReceivedStories, ReceivedStory, SemanticMessageType, StoryHolder, StoryKey,
    carries_story_list,
};

/// Missing and unrelated carriers are absent; malformed story statements retain
/// only their unreadable state, never the raw metadata or a refusal reason.
pub(super) fn parse_story_carrier(
    kind: SemanticMessageType,
    raw: Option<&Value>,
) -> Option<ReceivedStories> {
    if !carries_story_list(kind) {
        return None;
    }
    let raw = raw?;
    Some(match parse_story_list(raw) {
        Some(stories) => ReceivedStories::Listed { stories },
        None => ReceivedStories::Unreadable,
    })
}

fn parse_story_list(raw: &Value) -> Option<Vec<ReceivedStory>> {
    let entries = raw.as_object()?.get("stories")?.as_array()?;
    if entries.is_empty() || entries.len() > MAX_STORIES {
        return None;
    }
    let mut keys = HashSet::with_capacity(entries.len());
    let mut stories = Vec::with_capacity(entries.len());
    for entry in entries {
        let entry = entry.as_object()?;
        let key_text = entry.get("key")?.as_str()?;
        let key = StoryKey::parse(key_text).ok()?;
        if !keys.insert(key_text) {
            return None;
        }
        let you = match entry.get("you") {
            Some(value) => value.as_bool()?,
            None => false,
        };
        let alias = match entry.get("alias") {
            Some(value) => {
                let alias = value.as_str()?;
                if alias.is_empty() || alias.len() > MAX_STORY_ALIAS_BYTES {
                    return None;
                }
                Some(alias)
            }
            None => None,
        };
        let holder = match (you, alias, entry.get("peer")) {
            (true, None, None) => StoryHolder::You,
            (false, None, None) => StoryHolder::NoHolder,
            (false, Some(alias), peer) => StoryHolder::Member {
                alias: alias.to_owned(),
                peer: match peer {
                    Some(value) => Some(PeerId::parse(value.as_str()?).ok()?),
                    None => None,
                },
            },
            _ => return None,
        };
        stories.push(ReceivedStory { key, holder });
    }
    Some(stories)
}

/// A selection is not a story list. Every message type may carry it; malformed
/// metadata is admitted as unreadable without retaining the raw value.
pub(super) fn parse_selection_carrier(
    raw: Option<&Value>,
) -> Option<crate::domain::models::ReceivedSelection> {
    use crate::domain::models::ReceivedSelection;
    let raw = raw?;
    let parse = || {
        let entries = raw.as_object()?.get("stories")?.as_array()?;
        if entries.is_empty() || entries.len() > MAX_STORIES {
            return None;
        }
        let mut seen = HashSet::with_capacity(entries.len());
        let mut stories = Vec::with_capacity(entries.len());
        for entry in entries {
            let token = entry.as_str()?;
            let key = StoryKey::parse(token).ok()?;
            if !seen.insert(token) {
                return None;
            }
            stories.push(key);
        }
        Some(stories)
    };
    Some(match parse() {
        Some(stories) => ReceivedSelection::Listed { stories },
        None => ReceivedSelection::Unreadable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{PeerId, ReceivedStories, SemanticMessageType, StoryHolder};
    use serde_json::json;

    #[test]
    fn story_carrier_rejects_every_malformed_shape_without_raw_retention() {
        let pin = PeerId::from_public_key(&[2; 32]).unwrap();
        let malformed = vec![
            json!(null),
            json!([]),
            json!({}),
            json!({"stories":null}),
            json!({"stories":[]}),
            json!({"stories":(0..65).map(|n|json!({"key":format!("S{n}")})).collect::<Vec<_>>()}),
            json!({"stories":[0]}),
            json!({"stories":[{}]}),
            json!({"stories":[{"key":3}]}),
            json!({"stories":[{"key":"bad!"}]}),
            json!({"stories":[{"key":"S1"},{"key":"S1"}]}),
            json!({"stories":[{"key":"S1","you":1}]}),
            json!({"stories":[{"key":"S1","you":true,"alias":"b"}]}),
            json!({"stories":[{"key":"S1","you":true,"peer":pin}]}),
            json!({"stories":[{"key":"S1","peer":pin}]}),
            json!({"stories":[{"key":"S1","alias":null}]}),
            json!({"stories":[{"key":"S1","alias":""}]}),
            json!({"stories":[{"key":"S1","alias":"b".repeat(257)}]}),
            json!({"stories":[{"key":"S1","alias":"b","peer":null}]}),
            json!({"stories":[{"key":"S1","alias":"b","peer":"invalid"}]}),
            json!({"stories":[{"key":""}]}),
            json!({"stories":[{"key":"S".repeat(33)}]}),
            json!({"stories":[{"key":"S1","you":null}]}),
            json!({"stories":[{"key":"S1","you":"false"}]}),
            json!({"stories":[{"key":"S1","you":false,"peer":pin}]}),
            json!({"stories":[{"key":"S1","alias":1}]}),
            json!({"stories":[{"key":"S1","alias":"é".repeat(128) + "x"}]}),
            json!({"stories":[{"key":"S1","alias":"b","peer":1}]}),
        ];
        for value in malformed {
            assert_eq!(
                parse_story_carrier(SemanticMessageType::StoryAssignment, Some(&value)),
                Some(ReceivedStories::Unreadable),
                "{value}"
            );
            assert_eq!(
                parse_story_carrier(SemanticMessageType::DesignUpdate, Some(&value)),
                None
            );
        }
        assert_eq!(
            parse_story_carrier(SemanticMessageType::StoryAssignment, None),
            None
        );
    }

    #[test]
    fn story_carrier_keeps_order_false_and_unknown_fields_and_alias_byte_boundary() {
        let value = json!({"future":true,"stories":[{"key":"S1","you":false,"future":"x"},{"key":"S2","you":true},{"key":"S3","you":false,"alias":"é".repeat(128)}]});
        let Some(ReceivedStories::Listed { stories }) =
            parse_story_carrier(SemanticMessageType::StoryAssignment, Some(&value))
        else {
            panic!("readable list")
        };
        assert_eq!(stories.len(), 3);
        assert_eq!(stories[0].holder, StoryHolder::NoHolder);
        assert_eq!(stories[1].holder, StoryHolder::You);
        assert!(
            matches!(&stories[2].holder, StoryHolder::Member { alias, peer:None } if alias.len()==256)
        );
    }

    #[test]
    fn story_carrier_keeps_pinned_member_and_maximum_list() {
        let pin = PeerId::from_public_key(&[3; 32]).unwrap();
        let value = json!({"stories":[{"key":"S1","you":false,"alias":"c","peer":pin,"future":{"anything":true}}]});
        assert_eq!(
            parse_story_carrier(SemanticMessageType::DesignUpdate, Some(&value)),
            None
        );
        let Some(ReceivedStories::Listed { stories }) =
            parse_story_carrier(SemanticMessageType::StoryAssignment, Some(&value))
        else {
            panic!("readable pinned member");
        };
        assert_eq!(
            stories[0].holder,
            StoryHolder::Member {
                alias: "c".to_owned(),
                peer: Some(pin),
            }
        );

        let value =
            json!({"stories":(0..64).map(|n| json!({"key":format!("S{n}")})).collect::<Vec<_>>()});
        let Some(ReceivedStories::Listed { stories }) =
            parse_story_carrier(SemanticMessageType::StoryAssignment, Some(&value))
        else {
            panic!("readable maximum list");
        };
        assert_eq!(stories.len(), 64);
        assert_eq!(stories[0].key.as_str(), "S0");
        assert_eq!(stories[63].key.as_str(), "S63");
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use crate::domain::models::ReceivedSelection;
    use serde_json::json;

    #[test]
    fn story19_23_selection_ingest_table_is_tolerant_and_bounded() {
        assert_eq!(parse_selection_carrier(None), None);
        for raw in [
            json!(null),
            json!([]),
            json!("raw-secret"),
            json!(1),
            json!({}),
            json!({"stories":null}),
            json!({"stories":"S1"}),
            json!({"stories":[]}),
            json!({"stories":(0..65).map(|n|format!("S{n}")).collect::<Vec<_>>()}),
            json!({"stories":[1]}),
            json!({"stories":[null]}),
            json!({"stories":[{}]}),
            json!({"stories":["bad!"]}),
            json!({"stories":["S1","S1"]}),
            json!({"stories":[""]}),
            json!({"stories":["S".repeat(33)]}),
        ] {
            assert_eq!(
                parse_selection_carrier(Some(&raw)),
                Some(ReceivedSelection::Unreadable),
                "{raw}"
            );
        }
        let list = json!({"stories":["S7-06","S7-04"]});
        assert_eq!(
            parse_selection_carrier(Some(&list)),
            Some(ReceivedSelection::Listed {
                stories: vec![
                    StoryKey::parse("S7-06").unwrap(),
                    StoryKey::parse("S7-04").unwrap()
                ]
            })
        );
        assert!(
            matches!(parse_selection_carrier(Some(&json!({"stories":(0..64).map(|n|format!("S{n}")).collect::<Vec<_>>()}))),Some(ReceivedSelection::Listed {stories}) if stories.len()==64)
        );
    }
}
