use async_trait::async_trait;
use std::sync::Arc;
use thiserror::Error;

use crate::domain::models::{
    AgentId, DeliveryDisposition, DeliveryOutcome, EffectivePolicy, MessageHeader, OwnershipKind,
    PolicyPrincipal, RefuseReason, ResponseMode, SemanticMessageType, relationship_disposition,
};
use crate::domain::services::team_policy::{resolve_message_type_policy, sender_policy_for};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DeliveryError {
    #[error("recipient not found: {0:?}")]
    NotFound(AgentId),
    #[error("remote recipient unsupported in R1: {0:?}")]
    RemoteUnsupported(AgentId),
    #[error("recipient cannot receive messages in current state: {0:?}")]
    Refused(RefuseReason),
    #[error("recipient inbox is full: {0:?}")]
    Full(AgentId),
    #[error("recipient channel is closed: {0:?}")]
    Closed(AgentId),
}

#[async_trait]
pub trait AgentMessageBus: Send + Sync {
    async fn deliver(
        &self,
        to: &AgentId,
        env: crate::domain::models::Envelope<crate::domain::models::AgentMessage>,
    ) -> Result<DeliveryOutcome, DeliveryError>;
}
/// Response automation selected independently from relationship consent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerResponsePolicy {
    pub mode: ResponseMode,
    pub auto_response: Option<String>,
    pub notification: crate::domain::models::NotificationUrgency,
    pub provenance: crate::domain::models::InteractionPolicySnapshot,
}

impl Default for PeerResponsePolicy {
    fn default() -> Self {
        Self {
            mode: ResponseMode::NotifyAndWait,
            auto_response: None,
            notification: crate::domain::models::NotificationUrgency::Queue,
            provenance: crate::domain::models::InteractionPolicySnapshot::default(),
        }
    }
}

pub trait DeliveryPolicy: Send + Sync {
    fn decide(
        &self,
        header: &MessageHeader,
        recipient_ownership: OwnershipKind,
    ) -> DeliveryDisposition;

    fn response_policy(&self, header: &MessageHeader) -> PeerResponsePolicy {
        header
            .verified_peer_id
            .as_ref()
            .map_or_else(PeerResponsePolicy::default, |peer_id| {
                self.response_policy_for_peer(
                    &PolicyPrincipal::rap(peer_id.clone()),
                    header.message_type,
                )
            })
    }

    fn response_policy_for_peer(
        &self,
        principal: &PolicyPrincipal,
        message_type: SemanticMessageType,
    ) -> PeerResponsePolicy {
        let _ = (principal, message_type);
        PeerResponsePolicy::default()
    }
}

#[derive(Clone, Debug, Default)]
pub struct RelationshipDeliveryPolicy;

impl DeliveryPolicy for RelationshipDeliveryPolicy {
    fn decide(
        &self,
        header: &MessageHeader,
        recipient_ownership: OwnershipKind,
    ) -> DeliveryDisposition {
        let _ = header;
        relationship_disposition(recipient_ownership)
    }
}

/// Consent-only delivery policy backed by the startup-resolved workspace policy.
/// Response modes are queried separately through [`DeliveryPolicy::response_policy`]
/// and [`DeliveryPolicy::response_policy_for_peer`].
#[derive(Clone, Debug)]
pub struct EffectiveDeliveryPolicy {
    policy: Arc<EffectivePolicy>,
}

impl EffectiveDeliveryPolicy {
    #[must_use]
    pub fn new(policy: Arc<EffectivePolicy>) -> Self {
        Self { policy }
    }
}

impl DeliveryPolicy for EffectiveDeliveryPolicy {
    fn decide(
        &self,
        _header: &MessageHeader,
        recipient_ownership: OwnershipKind,
    ) -> DeliveryDisposition {
        relationship_disposition(recipient_ownership)
    }

    fn response_policy_for_peer(
        &self,
        principal: &PolicyPrincipal,
        message_type: SemanticMessageType,
    ) -> PeerResponsePolicy {
        let sender = sender_policy_for(&self.policy, principal, message_type);
        let (response, notification) =
            resolve_message_type_policy(&self.policy, sender, message_type);
        PeerResponsePolicy {
            mode: response.value,
            auto_response: sender.and_then(|matched| {
                matched
                    .type_override
                    .and_then(|override_| override_.auto_response.clone())
                    .or_else(|| matched.auto_response.clone())
            }),
            notification: notification.value,
            provenance: crate::domain::models::InteractionPolicySnapshot {
                sender_label: sender.map(|matched| matched.alias.clone()),
                message_type,
                response,
                notification,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{CorrelationId, MessageKind, PeerId};

    fn header() -> MessageHeader {
        MessageHeader {
            message_type: crate::domain::models::SemanticMessageType::Unknown,
            sender: AgentId::from_validated("parent"),
            recipient: AgentId::from_validated("child"),
            correlation_id: CorrelationId::new("c"),
            kind: MessageKind::PeerMessage,
            sequence: None,
            verified_peer_id: None,
        }
    }

    #[test]
    fn ac4_relationship_policy_routes_owned_and_peer_through_same_seam() {
        let policy = RelationshipDeliveryPolicy;
        assert_eq!(
            policy.decide(&header(), OwnershipKind::Owned),
            DeliveryDisposition::MustReport
        );
        assert_eq!(
            policy.decide(&header(), OwnershipKind::Peer),
            DeliveryDisposition::MayRefuse
        );
    }

    #[test]
    fn effective_policy_uses_verified_sender_and_keeps_relationship_separate() {
        use crate::domain::models::{
            IndividualPolicy, PeerId, PolicySource, Resolved, SenderBinding, SenderIdentity,
            SenderPolicy,
        };

        let sender = PeerId::from_public_key(&[7_u8; 32]).expect("valid peer id");
        let mut effective = crate::domain::services::team_policy::resolve_effective_policy(
            &IndividualPolicy::default(),
            None,
            &[],
        );
        effective.sender_overrides.push(SenderPolicy {
            alias: "trusted-peer".to_owned(),
            identity: SenderIdentity::Pinned {
                peer_id: sender.clone(),
                binding: SenderBinding::DeclaredPeerId,
            },
            response_mode: Some(Resolved {
                value: ResponseMode::NotifyAndAuto,
                source: PolicySource::Default,
                individual: ResponseMode::NotifyAndAuto,
                team: None,
            }),
            notification: Some(Resolved {
                value: crate::domain::models::NotificationUrgency::Immediate,
                source: PolicySource::TeamRaised {
                    file: ".rustain/team-policy.toml".to_owned(),
                },
                individual: crate::domain::models::NotificationUrgency::Queue,
                team: Some(crate::domain::models::NotificationUrgency::Immediate),
            }),
            auto_response: Some("acknowledged".to_owned()),
            per_type: std::collections::BTreeMap::new(),
        });
        let policy = EffectiveDeliveryPolicy::new(Arc::new(effective));

        let mut verified = header();
        verified.verified_peer_id = Some(sender);
        assert_eq!(
            policy.response_policy(&verified),
            PeerResponsePolicy {
                mode: ResponseMode::NotifyAndAuto,
                auto_response: Some("acknowledged".to_owned()),
                notification: crate::domain::models::NotificationUrgency::Immediate,
                provenance: crate::domain::models::InteractionPolicySnapshot {
                    sender_label: Some("trusted-peer".to_owned()),
                    message_type: SemanticMessageType::Unknown,
                    response: Resolved {
                        value: ResponseMode::NotifyAndAuto,
                        source: PolicySource::Default,
                        individual: ResponseMode::NotifyAndAuto,
                        team: None,
                    },
                    notification: Resolved {
                        value: crate::domain::models::NotificationUrgency::Immediate,
                        source: PolicySource::TeamRaised {
                            file: ".rustain/team-policy.toml".to_owned(),
                        },
                        individual: crate::domain::models::NotificationUrgency::Queue,
                        team: Some(crate::domain::models::NotificationUrgency::Immediate),
                    },
                },
            }
        );
        assert_eq!(
            policy.decide(&verified, OwnershipKind::Peer),
            DeliveryDisposition::MayRefuse,
            "response automation must not mutate relationship consent"
        );

        let mut claimed_only = header();
        claimed_only.sender = AgentId::from_validated("trusted-peer");
        assert_eq!(
            policy.response_policy(&claimed_only).mode,
            ResponseMode::NotifyAndWait,
            "a claimed AgentId must not impersonate a verified peer identity"
        );
    }

    #[test]
    fn response_policy_resolves_team_and_sender_overrides_by_closed_type() {
        use std::collections::BTreeMap;

        use crate::domain::models::{
            IndividualDefaults, IndividualPolicy, MessageTypeOverride, NotificationUrgency, PeerId,
            SemanticMessageType, SenderBinding, SenderIdentity, SenderOverride, TeamPolicy,
            TeamTypeOverride,
        };

        let sender = PeerId::from_public_key(&[9_u8; 32]).expect("valid peer id");
        let mut individual = IndividualPolicy {
            defaults: IndividualDefaults {
                response_mode: Some(ResponseMode::NotifyAndAuto),
                notification: Some(NotificationUrgency::Digest),
                ..IndividualDefaults::default()
            },
            ..IndividualPolicy::default()
        };
        individual.overrides.insert(
            "pinned".to_owned(),
            SenderOverride {
                peer_id: Some(sender.to_string()),
                per_type: BTreeMap::from([
                    (
                        "consultation".to_owned(),
                        MessageTypeOverride {
                            response_mode: Some(ResponseMode::NotifyAndWait),
                            notification: Some(NotificationUrgency::Immediate),
                            auto_response: Some("consultation reply".to_owned()),
                        },
                    ),
                    (
                        "bug_report".to_owned(),
                        MessageTypeOverride {
                            response_mode: Some(ResponseMode::NotifyAndDraft),
                            notification: Some(NotificationUrgency::Queue),
                            auto_response: Some("bug reply".to_owned()),
                        },
                    ),
                ]),
                ..SenderOverride::default()
            },
        );
        let team = TeamPolicy {
            overrides: crate::domain::models::TeamOverrides {
                per_type: BTreeMap::from([
                    (
                        "consultation".to_owned(),
                        toml::Value::try_from(TeamTypeOverride {
                            response_mode: Some(ResponseMode::NotifyAndDraft),
                            notification: Some(NotificationUrgency::Queue),
                        })
                        .unwrap(),
                    ),
                    (
                        "bug_report".to_owned(),
                        toml::Value::try_from(TeamTypeOverride {
                            response_mode: Some(ResponseMode::NotifyAndAuto),
                            notification: Some(NotificationUrgency::Digest),
                        })
                        .unwrap(),
                    ),
                ]),
                ..crate::domain::models::TeamOverrides::default()
            },
            ..TeamPolicy::default()
        };
        let mut effective = crate::domain::services::team_policy::resolve_effective_policy(
            &individual,
            Some(&team),
            &[],
        );
        effective.sender_overrides[0].identity = SenderIdentity::Pinned {
            peer_id: sender.clone(),
            binding: SenderBinding::DeclaredPeerId,
        };
        let policy = EffectiveDeliveryPolicy::new(Arc::new(effective));

        let consultation = policy.response_policy_for_peer(
            &PolicyPrincipal::rap(sender.clone()),
            SemanticMessageType::Consultation,
        );
        let bug = policy.response_policy_for_peer(
            &PolicyPrincipal::rap(sender),
            SemanticMessageType::BugReport,
        );
        assert_eq!(consultation.mode, ResponseMode::NotifyAndWait);
        assert_eq!(consultation.notification, NotificationUrgency::Immediate);
        assert_eq!(
            consultation.auto_response.as_deref(),
            Some("consultation reply")
        );
        assert_eq!(bug.mode, ResponseMode::NotifyAndDraft);
        assert_eq!(bug.notification, NotificationUrgency::Queue);
        assert_eq!(bug.auto_response.as_deref(), Some("bug reply"));

        let a2a_submitter = PeerId::from_public_key(&[10_u8; 32]).expect("valid peer id");
        let a2a_consultation = policy.response_policy_for_peer(
            &PolicyPrincipal::Pseudonymous(a2a_submitter.clone()),
            SemanticMessageType::Consultation,
        );
        let a2a_bug = policy.response_policy_for_peer(
            &PolicyPrincipal::Pseudonymous(a2a_submitter),
            SemanticMessageType::BugReport,
        );
        assert_eq!(a2a_consultation.mode, ResponseMode::NotifyAndDraft);
        assert_eq!(a2a_consultation.notification, NotificationUrgency::Queue);
        assert_eq!(a2a_bug.mode, ResponseMode::NotifyAndAuto);
        assert_eq!(a2a_bug.notification, NotificationUrgency::Digest);
        assert_eq!(
            a2a_consultation.auto_response, None,
            "A2A has no pinned roster sender, so individual per-alias policy cannot bind"
        );
    }

    #[test]
    fn every_semantic_type_preserves_the_urgency_floor_and_automation_ceiling() {
        use std::collections::BTreeMap;

        use crate::domain::models::{
            IndividualDefaults, IndividualPolicy, MessageTypeOverride, NotificationUrgency, PeerId,
            ResponseMode, SenderBinding, SenderIdentity, SenderOverride, TeamPolicy,
            TeamTypeOverride, semantic_message_type_tokens,
        };

        let sender = PeerId::from_public_key(&[12_u8; 32]).expect("valid peer id");
        let individual_type_overrides = semantic_message_type_tokens()
            .map(|token| {
                (
                    token.to_owned(),
                    MessageTypeOverride {
                        response_mode: Some(ResponseMode::NotifyAndAuto),
                        notification: Some(NotificationUrgency::Digest),
                        auto_response: None,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let team_type_overrides = semantic_message_type_tokens()
            .map(|token| {
                (
                    token.to_owned(),
                    toml::Value::try_from(TeamTypeOverride {
                        response_mode: Some(ResponseMode::NotifyAndAuto),
                        notification: Some(NotificationUrgency::Digest),
                    })
                    .unwrap(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut individual = IndividualPolicy {
            defaults: IndividualDefaults {
                response_mode: Some(ResponseMode::NotifyAndWait),
                notification: Some(NotificationUrgency::Immediate),
                ..Default::default()
            },
            ..Default::default()
        };
        individual.overrides.insert(
            "pinned".to_owned(),
            SenderOverride {
                peer_id: Some(sender.to_string()),
                per_type: individual_type_overrides,
                ..Default::default()
            },
        );
        let team = TeamPolicy {
            overrides: crate::domain::models::TeamOverrides {
                per_type: team_type_overrides,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut effective = crate::domain::services::team_policy::resolve_effective_policy(
            &individual,
            Some(&team),
            &[],
        );
        effective.sender_overrides[0].identity = SenderIdentity::Pinned {
            peer_id: sender.clone(),
            binding: SenderBinding::DeclaredPeerId,
        };
        let policy = EffectiveDeliveryPolicy::new(Arc::new(effective));

        for token in semantic_message_type_tokens() {
            let resolved = policy.response_policy_for_peer(
                &PolicyPrincipal::rap(sender.clone()),
                crate::domain::models::SemanticMessageType::parse(token),
            );
            assert_eq!(
                resolved.mode,
                ResponseMode::NotifyAndWait,
                "{token} loosened the automation ceiling"
            );
            assert_eq!(
                resolved.notification,
                NotificationUrgency::Immediate,
                "{token} lowered the urgency floor"
            );
        }
    }
    #[test]
    fn unknown_wire_type_uses_strictest_type_agnostic_policy_and_is_disclosable() {
        use std::collections::BTreeMap;

        use crate::domain::models::{
            IndividualDefaults, IndividualPolicy, NotificationUrgency, PeerId,
            SemanticMessageRecognition, SemanticMessageType, TeamDefaults, TeamOverrides,
            TeamPolicy, TeamTypeOverride, semantic_message_type_metadata,
        };

        let individual = IndividualPolicy {
            defaults: IndividualDefaults {
                response_mode: Some(ResponseMode::NotifyAndAuto),
                notification: Some(NotificationUrgency::Digest),
                ..IndividualDefaults::default()
            },
            ..IndividualPolicy::default()
        };
        let team = TeamPolicy {
            defaults: TeamDefaults {
                response_mode: Some(ResponseMode::NotifyAndWait),
                notification: Some(NotificationUrgency::Immediate),
            },
            overrides: TeamOverrides {
                per_type: BTreeMap::from([(
                    "future_message".to_owned(),
                    toml::Value::try_from(TeamTypeOverride {
                        response_mode: Some(ResponseMode::NotifyAndAuto),
                        notification: Some(NotificationUrgency::Digest),
                    })
                    .unwrap(),
                )]),
                ..TeamOverrides::default()
            },
            ..TeamPolicy::default()
        };
        let effective = crate::domain::services::team_policy::resolve_effective_policy(
            &individual,
            Some(&team),
            &[],
        );
        let policy = EffectiveDeliveryPolicy::new(Arc::new(effective));
        let submitter = PeerId::from_public_key(&[11_u8; 32]).expect("valid peer id");

        let unknown = policy.response_policy_for_peer(
            &PolicyPrincipal::Pseudonymous(submitter),
            SemanticMessageType::Unknown,
        );
        assert_eq!(unknown.mode, ResponseMode::NotifyAndWait);
        assert_eq!(unknown.notification, NotificationUrgency::Immediate);
        let metadata = semantic_message_type_metadata(SemanticMessageType::Unknown);
        assert_eq!(metadata.recognition, SemanticMessageRecognition::Unknown);
        assert_eq!(metadata.operator_noun, "unknown type");
    }

    /// K09c: the RAP default `response_policy` wraps a verified header as `rap`, so a
    /// pinned override written in TOML reaches it; the same `PeerId` presented as a
    /// `Pseudonymous` principal gets the workspace default.
    #[test]
    fn default_response_policy_wraps_header_peer_as_rap_and_pseudonym_gets_defaults() {
        use sha2::Digest as _;

        use crate::domain::models::{
            A2aPeerSource, A2aPeerSpec, IndividualPolicy, NotificationUrgency, PinnedKey,
            PinnedKeyAlgorithm, RedactedUrl,
        };

        #[derive(serde::Deserialize)]
        struct Root {
            interaction: IndividualPolicy,
        }

        // The override's pin equals the A2A loopback pseudonym, `sha256("loopback")`.
        let key: [u8; 32] = sha2::Sha256::digest(b"loopback").into();
        let pin = PeerId::from_public_key(&key).expect("valid peer id");
        let written = format!(
            "[interaction.overrides.loopback-pin]\n\
             peer_id = \"{}\"\n\
             response_mode = \"notify-and-auto\"\n\
             notification = \"immediate\"\n\
             auto_response = \"pinned reply\"\n",
            pin.as_str()
        );
        let individual = toml::from_str::<Root>(&written)
            .expect("written interaction policy parses")
            .interaction;
        let roster = [A2aPeerSpec::new(
            "loopback-pin",
            RedactedUrl::new("https://peer.example/a2a".to_owned()),
            A2aPeerSource::Workspace,
        )
        .with_pinned_key(Some(PinnedKey::new(
            PinnedKeyAlgorithm::EdDsa,
            base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, key),
            None,
        )))];
        let effective = crate::domain::services::team_policy::resolve_effective_policy(
            &individual,
            None,
            &roster,
        );
        assert_eq!(effective.sender_overrides.len(), 1);
        let policy = EffectiveDeliveryPolicy::new(Arc::new(effective));

        let mut with_peer = header();
        with_peer.verified_peer_id = Some(pin.clone());
        let from_header = policy.response_policy(&with_peer);
        assert_eq!(from_header.mode, ResponseMode::NotifyAndAuto);
        assert_eq!(from_header.notification, NotificationUrgency::Immediate);
        assert_eq!(from_header.auto_response.as_deref(), Some("pinned reply"));
        assert_eq!(
            from_header.provenance.sender_label.as_deref(),
            Some("loopback-pin")
        );
        assert_eq!(
            from_header,
            policy.response_policy_for_peer(
                &PolicyPrincipal::rap(pin.clone()),
                SemanticMessageType::Unknown
            ),
            "the header path is the rap(verified peer) path"
        );

        let pseudonymous = policy.response_policy_for_peer(
            &PolicyPrincipal::Pseudonymous(pin),
            SemanticMessageType::Unknown,
        );
        assert_eq!(pseudonymous.mode, ResponseMode::NotifyAndWait);
        assert_eq!(pseudonymous.notification, NotificationUrgency::Queue);
        assert_eq!(pseudonymous.auto_response, None);
        assert_eq!(pseudonymous.provenance.sender_label, None);

        assert_eq!(
            policy.response_policy(&header()),
            PeerResponsePolicy::default(),
            "a header without a verified peer still gets the default"
        );
    }
}
