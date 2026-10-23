use std::collections::BTreeSet;

use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::domain::models::{AgentToolRestriction, PermissionMode, ResponseMode, ToolPolicy};
use serde_json::{Value, json};

use super::observed_host::{HttpCounts, ObservedHost};
use super::proposal_harness::ProposalFixture;

const TOOL: &str = "propose_team_message";
const BODY: &str = "ADR-047 finds a race.\nKeep the owner lock around publication.\nRésumé: preserve naïve input. 🧭";
const R1: &str = "Proposal shown to the operator. Nothing has been sent; the operator decides whether to send it to lena-po, jun-dev.";
const R3: &str = "Not proposed: an earlier proposal is still awaiting the operator's decision. Nothing was sent.";

fn valid_input() -> Value {
    json!({"recipients":["lena-po", "jun-dev"], "type":"architecture_update", "body":BODY})
}

async fn fixture() -> (ProposalFixture, ObservedHost, ObservedHost) {
    let lena = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let jun = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut fixture = ProposalFixture::start(vec![lena.peer("lena-po"), jun.peer("jun-dev")]).await;
    fixture.set_mode(PermissionMode::Yolo);
    assert!(
        lena.counts().get > 0 && jun.counts().get > 0,
        "real signed AgentCard GETs must cross the observer"
    );
    lena.reset_counts();
    jun.reset_counts();
    (fixture, lena, jun)
}

#[tokio::test]
async fn k01_valid_raise_returns_r1_for_aliases_type_and_exact_body() {
    let (mut fixture, lena, jun) = fixture().await;
    assert_eq!(
        fixture.submit(valid_input()).await,
        R1,
        "K01 valid raise must return R1"
    );
    assert!(
        fixture.last_turn_completed,
        "the proposal invocation must not park the turn"
    );
    let snapshot = fixture
        .state
        .pending_team_proposals
        .get(&fixture.conversation.id)
        .expect("production ready event must retain a proposal");
    assert_eq!(snapshot.recipients, ["lena-po", "jun-dev"]);
    assert_eq!(
        snapshot.message_type,
        rustain::domain::models::SemanticMessageType::ArchitectureUpdate
    );
    assert_eq!(snapshot.body.as_bytes(), BODY.as_bytes());
    assert_eq!(snapshot.conversation_id, fixture.conversation.id);
    assert!(snapshot.id.starts_with("tp_"));
    assert_eq!(fixture.last_tool_errors, [false]);
    assert!(
        lena.journal().entries().await.is_empty(),
        "raising cannot deliver to Lena"
    );
    assert!(
        jun.journal().entries().await.is_empty(),
        "raising cannot deliver to Jun"
    );
    assert_eq!(lena.counts(), HttpCounts { get: 0, post: 0 });
    assert_eq!(jun.counts(), HttpCounts { get: 0, post: 0 });
    lena.stop().await;
    jun.stop().await;
}

#[tokio::test]
async fn k01_catalogue_exposes_proposal_only_with_egress_not_describe() {
    let (fixture, lena, jun) = fixture().await;
    assert!(
        fixture.available_tools().iter().any(|name| name == TOOL),
        "K01 egress catalogue must offer proposal"
    );
    assert!(
        !fixture.describe().iter().any(|tool| tool.name == TOOL),
        "proposal is not registry discovery"
    );
    lena.stop().await;
    jun.stop().await;
}

#[tokio::test]
async fn k01_invalid_alias_type_recipient_body_and_unicode_fields_are_refused() {
    let (mut fixture, lena, jun) = fixture().await;
    for (label, input, diagnostic) in [
        (
            "unknown alias",
            json!({"recipients":["unknown-agent"],"type":"architecture_update","body":"memo"}),
            "unknown-agent",
        ),
        (
            "missing type",
            json!({"recipients":["lena-po"],"body":"memo"}),
            "type",
        ),
        (
            "duplicate",
            json!({"recipients":["lena-po","lena-po"],"type":"architecture_update","body":"memo"}),
            "duplicate",
        ),
        (
            "empty recipients",
            json!({"recipients":[],"type":"architecture_update","body":"memo"}),
            "recipients",
        ),
        (
            "unknown type",
            json!({"recipients":["lena-po"],"type":"future_kind","body":"memo"}),
            "type",
        ),
        (
            "missing body",
            json!({"recipients":["lena-po"],"type":"architecture_update"}),
            "body",
        ),
        (
            "empty body",
            json!({"recipients":["lena-po"],"type":"architecture_update","body":""}),
            "body",
        ),
        (
            "C0",
            json!({"recipients":["lena-po"],"type":"architecture_update","body":"a\u{0007}b"}),
            "control",
        ),
        (
            "C1",
            json!({"recipients":["lena-po"],"type":"architecture_update","body":"a\u{0085}b"}),
            "control",
        ),
        (
            "DEL",
            json!({"recipients":["lena-po"],"type":"architecture_update","body":"a\u{007f}b"}),
            "control",
        ),
        (
            "bidi",
            json!({"recipients":["lena-po"],"type":"architecture_update","body":"left\u{202e}right"}),
            "control",
        ),
        (
            "bytes",
            json!({"recipients":["lena-po"],"type":"architecture_update","body":"x".repeat(131_073)}),
            "131072",
        ),
    ] {
        let result = fixture.submit(input).await;
        assert!(
            result.contains(diagnostic),
            "K01 {label}: field diagnostic absent: {result:?}"
        );
        assert_eq!(
            fixture.last_tool_errors,
            [true],
            "K01 {label}: R4 must be an error, not a successful refusal string"
        );
        assert!(
            fixture.state.pending_team_proposals.is_empty(),
            "K01 {label}: malformed input left a card"
        );
        assert!(
            !result.starts_with("Proposal shown"),
            "K01 {label}: invalid proposal raised"
        );
    }
    assert!(lena.journal().entries().await.is_empty());
    assert!(jun.journal().entries().await.is_empty());
    assert_eq!(lena.counts(), HttpCounts { get: 0, post: 0 });
    assert_eq!(jun.counts(), HttpCounts { get: 0, post: 0 });
    lena.stop().await;
    jun.stop().await;
}

#[tokio::test]
async fn k01_second_raise_returns_r3_without_replacing_first_pending() {
    let (mut fixture, lena, jun) = fixture().await;
    assert_eq!(fixture.submit(valid_input()).await, R1);
    let original = fixture
        .state
        .pending_team_proposals
        .get(&fixture.conversation.id)
        .expect("first proposal")
        .clone();
    assert_eq!(
        fixture
            .submit(
                json!({"recipients":["jun-dev"],"type":"architecture_update","body":"replacement"})
            )
            .await,
        R3,
        "K01 second raise must not supersede"
    );
    let retained = fixture
        .state
        .pending_team_proposals
        .get(&fixture.conversation.id)
        .expect("pending original");
    assert!(
        std::sync::Arc::ptr_eq(&original, retained),
        "R3 must leave the original immutable snapshot untouched"
    );
    assert_eq!(lena.counts(), HttpCounts { get: 0, post: 0 });
    assert_eq!(jun.counts(), HttpCounts { get: 0, post: 0 });
    lena.stop().await;
    jun.stop().await;
}

#[tokio::test]
async fn k01_allowlist_omission_and_exclusion_deny_in_every_mode() {
    for mode in [
        PermissionMode::Normal,
        PermissionMode::Yolo,
        PermissionMode::AutoEdit,
        PermissionMode::Plan,
    ] {
        for policy in [
            ToolPolicy::Allowlist {
                tools: BTreeSet::from(["Read".to_owned()]),
            },
            ToolPolicy::Denylist {
                tools: BTreeSet::from([TOOL.to_owned()]),
            },
        ] {
            let (mut fixture, lena, jun) = fixture().await;
            fixture.set_mode(mode);
            fixture.set_restriction(Some(AgentToolRestriction {
                agent_name: "restricted".into(),
                policy,
                declared_items: BTreeSet::new(),
            }));
            let result = fixture.submit(valid_input()).await;
            assert!(
                !result.starts_with("Proposal shown"),
                "K01 restriction ignored in {mode:?}"
            );
            assert!(
                result.contains("agent") || result.contains("Plan mode"),
                "K01 restriction must be an explicit denial: {result}"
            );
            assert_eq!(lena.counts(), HttpCounts { get: 0, post: 0 });
            assert_eq!(jun.counts(), HttpCounts { get: 0, post: 0 });
            lena.stop().await;
            jun.stop().await;
        }
    }
}

#[tokio::test]
async fn k01_real_skill_allowlist_binds_in_every_mode_with_allowed_positive_control() {
    for mode in [
        PermissionMode::Normal,
        PermissionMode::Yolo,
        PermissionMode::AutoEdit,
        PermissionMode::Plan,
    ] {
        for allowed in [false, true] {
            let (mut fixture, lena, jun) = fixture().await;
            fixture.set_mode(mode);
            fixture
                .activate_policy_skill(if allowed {
                    "Read propose_team_message"
                } else {
                    "Read"
                })
                .await;
            let result = fixture.submit(valid_input()).await;
            if allowed && mode != PermissionMode::Plan {
                assert_eq!(
                    result, R1,
                    "explicit skill allowlist must permit a valid raise in {mode:?}"
                );
                assert_eq!(fixture.last_tool_errors, [false]);
                assert!(
                    fixture
                        .state
                        .pending_team_proposals
                        .contains_key(&fixture.conversation.id)
                );
            } else {
                assert_eq!(
                    fixture.last_tool_errors,
                    [true],
                    "skill restriction or Plan must deny in {mode:?}: {result}"
                );
                assert!(fixture.state.pending_team_proposals.is_empty());
                assert!(
                    result.contains("skill") || result.contains("Plan mode"),
                    "expected policy denial, not a missing tool: {result}"
                );
            }
            assert_eq!(lena.counts(), HttpCounts { get: 0, post: 0 });
            assert_eq!(jun.counts(), HttpCounts { get: 0, post: 0 });
            assert!(lena.journal().entries().await.is_empty());
            assert!(jun.journal().entries().await.is_empty());
            lena.stop().await;
            jun.stop().await;
        }
    }
}
