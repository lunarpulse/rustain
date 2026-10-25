//! Sending-host story selection; a selection grants no authority.

use std::collections::BTreeMap;

use super::driver::A2aDelegationRuntime;
use super::send::SendError;
use crate::adapters::policy::recipient_item::JournalRecipientItemProjection;
use crate::domain::models::{
    A2aPeerSpec, RecipientItemView, SelectedRecipient, SelectionRefusal, SendActionRecipient,
    StoryHolder, StoryKey, StorySelection, StorySelector, story_statements,
};

/// Read exactly one host-owned journal snapshot and fold both sources from it.
/// Selection is only a statement about where this message should go.
pub async fn resolve(
    runtime: &A2aDelegationRuntime,
    selector: &StorySelector,
) -> Result<StorySelection, SendError> {
    selector.check().map_err(SendError::InvalidStories)?;
    let entries = runtime
        .story_selection_entries()
        .await
        .map_err(|_| SendError::InvalidSelection(SelectionRefusal::JournalUnreadable))?;
    let received = JournalRecipientItemProjection::from_entries(&entries).snapshot();
    let own = crate::domain::services::send_action_ledger::story_holders(&entries);
    resolve_core(
        selector,
        received.items.values(),
        &own,
        runtime.story_peer_specs(),
        |holder| runtime.eligible_binding(holder).map(|(spec, _)| spec.id),
    )
    .map_err(SendError::InvalidSelection)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Candidate {
    Alias(String),
    NoHolder,
    Here,
    Unpinned(String),
    NoMatch,
    Several(Vec<String>),
    Unreadable,
}

fn received_candidate<'r>(
    holder: &StoryHolder,
    roster: impl Iterator<Item = &'r A2aPeerSpec>,
) -> Candidate {
    match holder {
        StoryHolder::NoHolder => Candidate::NoHolder,
        StoryHolder::You => Candidate::Here,
        StoryHolder::Member { alias, peer: None } => Candidate::Unpinned(alias.clone()),
        StoryHolder::Unrecognized => Candidate::Unreadable,
        StoryHolder::Member {
            peer: Some(pin), ..
        } => {
            let mut matches = roster.filter(|spec| spec.pinned_identity().as_ref() == Some(pin));
            let Some(first) = matches.next() else {
                return Candidate::NoMatch;
            };
            let Some(second) = matches.next() else {
                return Candidate::Alias(first.id.clone());
            };
            let mut aliases = vec![first.id.clone(), second.id.clone()];
            aliases.extend(matches.map(|spec| spec.id.clone()));
            aliases.sort_unstable();
            Candidate::Several(aliases)
        }
    }
}

fn resolve_candidates(
    key: &StoryKey,
    candidates: impl IntoIterator<Item = Candidate>,
) -> Result<String, SelectionRefusal> {
    let mut candidates = candidates.into_iter();
    let first = candidates
        .next()
        .ok_or_else(|| SelectionRefusal::NotListed(key.clone()))?;
    if candidates.any(|candidate| candidate != first) {
        return Err(SelectionRefusal::ConflictingHolders(key.clone()));
    }
    Err(match first {
        Candidate::Alias(alias) => return Ok(alias),
        Candidate::NoHolder => SelectionRefusal::NoHolder(key.clone()),
        Candidate::Here => SelectionRefusal::HeldHere(key.clone()),
        Candidate::Unpinned(alias) => SelectionRefusal::UnpinnedHolder {
            key: key.clone(),
            alias,
        },
        Candidate::NoMatch => SelectionRefusal::NoRosterMatch(key.clone()),
        Candidate::Several(aliases) => SelectionRefusal::SeveralRosterMatches {
            key: key.clone(),
            aliases,
        },
        Candidate::Unreadable => SelectionRefusal::UnreadableHolder(key.clone()),
    })
}

fn resolve_core<'a, 'r>(
    selector: &StorySelector,
    views: impl IntoIterator<Item = &'a RecipientItemView>,
    own: &BTreeMap<StoryKey, Option<SendActionRecipient>>,
    roster: impl Iterator<Item = &'r A2aPeerSpec> + Clone,
    eligible: impl Fn(&SendActionRecipient) -> Option<String>,
) -> Result<StorySelection, SelectionRefusal> {
    let statements = story_statements(views);
    let mut selection = StorySelection {
        recipients: Vec::new(),
    };
    for key in &selector.stories {
        let received = statements
            .iter()
            .filter(|statement| statement.story.key == *key)
            .map(|statement| received_candidate(&statement.story.holder, roster.clone()));
        let own = own.get(key).map(|holder| match holder {
            None => Candidate::NoHolder,
            Some(holder) => eligible(holder).map_or(Candidate::NoMatch, Candidate::Alias),
        });
        let alias = resolve_candidates(key, received.chain(own))?;
        if let Some(recipient) = selection
            .recipients
            .iter_mut()
            .find(|recipient| recipient.alias == alias)
        {
            recipient.stories.push(key.clone());
        } else {
            selection.recipients.push(SelectedRecipient {
                alias,
                stories: vec![key.clone()],
            });
        }
    }
    Ok(selection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{
        A2aPeerSource, Authorship, ItemAddress, ItemId, PeerId, ReceivedStories, ReceivedStory,
        RecipientItemState, RedactedUrl, SemanticMessageType, SendAuthorization,
    };

    fn key(value: &str) -> StoryKey {
        StoryKey::parse(value).unwrap()
    }
    fn item(sender: u8, order: u64, key_text: &str, holder: StoryHolder) -> RecipientItemView {
        RecipientItemView {
            address: ItemAddress::from_a2a_ingress(
                PeerId::from_public_key(&[sender; 32]).unwrap(),
                ItemId::from_replay(format!("ri-{order}")),
            ),
            task: format!("task-{order}"),
            alias: None,
            message_type: SemanticMessageType::StoryAssignment,
            stories: Some(ReceivedStories::Listed {
                stories: vec![ReceivedStory {
                    key: key(key_text),
                    holder,
                }],
            }),
            selected_for: None,
            authorship: Authorship::HumanWritten,
            send_authorization: SendAuthorization::HumanDecided,
            state: RecipientItemState::Received {
                content: "body".into(),
            },
            journal_order: order,
            retracted_at_ms: None,
            in_reply_to: None,
            capability_hash: None,
            reply_capability_matched: false,
        }
    }
    fn roster() -> Vec<A2aPeerSpec> {
        vec![A2aPeerSpec::new(
            "jun",
            RedactedUrl::from("http://127.0.0.1:9"),
            A2aPeerSource::Workspace,
        )]
    }

    #[test]
    fn story19_23_core_refuses_each_non_alias_candidate_and_disagreement() {
        let k = key("S1");
        for (candidate, expected) in [
            (Candidate::NoHolder, SelectionRefusal::NoHolder(k.clone())),
            (Candidate::Here, SelectionRefusal::HeldHere(k.clone())),
            (
                Candidate::Unpinned("Jun".into()),
                SelectionRefusal::UnpinnedHolder {
                    key: k.clone(),
                    alias: "Jun".into(),
                },
            ),
            (
                Candidate::NoMatch,
                SelectionRefusal::NoRosterMatch(k.clone()),
            ),
            (
                Candidate::Several(vec!["a".into(), "b".into()]),
                SelectionRefusal::SeveralRosterMatches {
                    key: k.clone(),
                    aliases: vec!["a".into(), "b".into()],
                },
            ),
            (
                Candidate::Unreadable,
                SelectionRefusal::UnreadableHolder(k.clone()),
            ),
        ] {
            assert_eq!(
                resolve_candidates(&k, [candidate.clone(), candidate]),
                Err(expected)
            );
        }
        assert_eq!(
            resolve_candidates(&k, []),
            Err(SelectionRefusal::NotListed(k.clone()))
        );
        assert_eq!(
            resolve_candidates(
                &k,
                [
                    Candidate::Alias("jun".into()),
                    Candidate::Alias("jun".into())
                ]
            ),
            Ok("jun".into())
        );
        assert_eq!(
            resolve_candidates(&k, [Candidate::Alias("jun".into()), Candidate::Here]),
            Err(SelectionRefusal::ConflictingHolders(k))
        );
    }

    #[test]
    fn story19_23_core_uses_one_uncapped_d6_walk_and_own_statements() {
        let mut peers = roster();
        let mut views = (0..70)
            .map(|n| {
                item(
                    (n % 2 + 1) as u8,
                    n,
                    &format!("S{n}"),
                    StoryHolder::NoHolder,
                )
            })
            .collect::<Vec<_>>();
        let selector = StorySelector::parse("S69").unwrap();
        let own = std::collections::BTreeMap::new();
        assert_eq!(
            resolve_core(&selector, views.iter(), &own, peers.iter(), |_| None),
            Err(SelectionRefusal::NoHolder(key("S69")))
        );
        peers[0].pinned_key = Some(crate::domain::models::PinnedKey::new(
            crate::domain::models::PinnedKeyAlgorithm::EdDsa,
            "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc".into(),
            None,
        ));
        if let Some(ReceivedStories::Listed { stories }) = &mut views.last_mut().unwrap().stories {
            stories[0].holder = StoryHolder::Member {
                alias: "display-only".into(),
                peer: peers[0].pinned_identity(),
            };
        }
        let selected = resolve_core(&selector, views.iter(), &own, peers.iter(), |_| None).unwrap();
        assert_eq!(selected.recipients[0].alias, "jun");
        assert_eq!(selected.recipients[0].stories, [key("S69")]);
        let mut own = std::collections::BTreeMap::new();
        own.insert(
            key("S71"),
            Some(SendActionRecipient {
                alias: "jun".into(),
                peer: PeerId::from_public_key(&[4; 32]).unwrap(),
                endpoint: None,
            }),
        );
        let result = resolve_core(
            &StorySelector::parse("S71").unwrap(),
            views.iter(),
            &own,
            peers.iter(),
            |holder| Some(holder.alias.clone()),
        )
        .unwrap();
        assert_eq!(result.recipients[0].alias, "jun");
        assert_eq!(result.recipients[0].stories, [key("S71")]);
        let unpinned = item(
            1,
            71,
            "S71",
            StoryHolder::Member {
                alias: "jun".into(),
                peer: None,
            },
        );
        assert_eq!(
            resolve_core(
                &StorySelector::parse("S71").unwrap(),
                [&unpinned],
                &own,
                peers.iter(),
                |holder| Some(holder.alias.clone())
            ),
            Err(SelectionRefusal::ConflictingHolders(key("S71")))
        );
        own.clear();
        assert!(matches!(
            resolve_core(
                &StorySelector::parse("S71").unwrap(),
                [&unpinned],
                &own,
                peers.iter(),
                |_| None
            ),
            Err(SelectionRefusal::UnpinnedHolder { .. })
        ));
    }
}

#[cfg(test)]
mod binding_tests {
    use super::*;
    use crate::domain::models::{
        A2aPeerSource, PeerId, PinnedKey, PinnedKeyAlgorithm, RedactedUrl,
    };

    #[test]
    fn story19_23_received_candidates_require_exactly_one_pin_match() {
        let unpinned = A2aPeerSpec::new(
            "jun",
            RedactedUrl::from("http://127.0.0.1:9"),
            A2aPeerSource::Workspace,
        );
        let pin = PinnedKey::new(
            PinnedKeyAlgorithm::EdDsa,
            "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc".into(),
            None,
        );
        let mut first = unpinned.clone().with_pinned_key(Some(pin));
        let holder = StoryHolder::Member {
            alias: "not-jun".into(),
            peer: first.pinned_identity(),
        };
        assert_eq!(
            received_candidate(&holder, [&unpinned].into_iter()),
            Candidate::NoMatch
        );
        assert_eq!(
            received_candidate(&holder, [&first].into_iter()),
            Candidate::Alias("jun".into())
        );
        let mut second = first.clone();
        second.id = "aisha".into();
        assert_eq!(
            received_candidate(&holder, [&first, &second].into_iter()),
            Candidate::Several(vec!["aisha".into(), "jun".into()])
        );
        first.id = "renamed".into();
        assert_eq!(
            received_candidate(&holder, [&first].into_iter()),
            Candidate::Alias("renamed".into())
        );
        assert_eq!(
            received_candidate(
                &StoryHolder::Member {
                    alias: "jun".into(),
                    peer: None
                },
                [&unpinned].into_iter()
            ),
            Candidate::Unpinned("jun".into())
        );
        assert_eq!(
            received_candidate(&StoryHolder::You, [].into_iter()),
            Candidate::Here
        );
        assert_eq!(
            received_candidate(&StoryHolder::NoHolder, [].into_iter()),
            Candidate::NoHolder
        );
        assert_eq!(
            received_candidate(&StoryHolder::Unrecognized, [].into_iter()),
            Candidate::Unreadable
        );
    }

    #[test]
    fn story19_23_recipients_and_keys_keep_selector_order() {
        let selector = StorySelector::parse("S2,S1,S3").unwrap();
        let own = [("S1", "jun"), ("S2", "aisha"), ("S3", "jun")]
            .into_iter()
            .map(|(key, alias)| {
                (
                    StoryKey::parse(key).unwrap(),
                    Some(SendActionRecipient {
                        alias: alias.into(),
                        peer: PeerId::from_public_key(&[1; 32]).unwrap(),
                        endpoint: None,
                    }),
                )
            })
            .collect();
        let result = resolve_core(&selector, [], &own, [].into_iter(), |holder| {
            Some(holder.alias.clone())
        })
        .unwrap();
        assert_eq!(
            result
                .recipients
                .iter()
                .map(|r| r.alias.as_str())
                .collect::<Vec<_>>(),
            ["aisha", "jun"]
        );
        assert_eq!(
            result.recipients[1]
                .stories
                .iter()
                .map(StoryKey::as_str)
                .collect::<Vec<_>>(),
            ["S1", "S3"]
        );
        assert!(matches!(
            resolve_core(&selector, [], &own, [].into_iter(), |_| None),
            Err(SelectionRefusal::NoRosterMatch(_))
        ));
    }

    #[tokio::test]
    async fn story19_23_resolution_reads_one_snapshot_and_refuses_unreadable_journal() {
        use crate::domain::models::RoomEvent;
        use crate::domain::ports::{RoomJournal, RoomJournalError, RoomJournalReader};
        use crate::infrastructure::subagent::NodeTree;
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Journal(AtomicUsize);
        #[async_trait::async_trait]
        impl RoomJournal for Journal {
            async fn record_event(&self, _: RoomEvent) -> Result<(), RoomJournalError> {
                panic!("resolution must never write")
            }
        }
        #[async_trait::async_trait]
        impl RoomJournalReader for Journal {
            async fn load_entries(
                &self,
            ) -> Result<Vec<crate::domain::models::JournalEntry>, RoomJournalError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(vec![])
            }
        }
        let journal = Arc::new(Journal(AtomicUsize::new(0)));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = A2aDelegationRuntime::new(NodeTree::new(), journal.clone(), tx);
        let selector = StorySelector::parse("S1").unwrap();
        assert!(matches!(
            resolve(&runtime, &selector).await,
            Err(SendError::InvalidSelection(
                SelectionRefusal::JournalUnreadable
            ))
        ));
        let runtime = runtime.with_journal_reader(journal.clone());
        assert!(matches!(
            resolve(&runtime, &selector).await,
            Err(SendError::InvalidSelection(SelectionRefusal::NotListed(_)))
        ));
        assert_eq!(journal.0.load(Ordering::SeqCst), 1);
    }
}
