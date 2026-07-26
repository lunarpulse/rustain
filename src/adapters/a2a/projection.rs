//! `RoomProjection<Viewer>` — the codebase's single redaction boundary.
//!
//! Story 18.1b, AC4b (R7). This story **originates** the type; Story 18.3a
//! parameterizes it by room role (owner / editor / viewer). The alternative —
//! a one-off `A2aResultProjection` here and a `RoomProjection<Viewer>` there —
//! would be two types carrying one invariant, and the one that drifts is the one
//! nobody is looking at.
//!
//! # Redaction is the TYPE, not a pass
//!
//! There is no field on [`RoomProjection`] that can hold a `PathBuf`, a tool
//! argv vector, a system prompt, a `NodeState`, an `AgentId`, a `ChatMessage`, a
//! room event, or a journal handle. Opacity therefore survives a future field
//! addition by someone who never read this module: adding such a field is a
//! visible change to a struct whose every member is asserted by
//! `the_projection_admits_no_host_state`.
//!
//! A redaction *pass* over a permissive struct would give the same served bytes
//! today and none of that guarantee.

use std::marker::PhantomData;
use std::path::Path;

use crate::domain::models::RapTaskState;

use super::task::rap_to_wire;

/// Who a projection is for. Story 18.3a adds room roles; this story ships the
/// one viewer that exists — a remote A2A submitter.
pub trait ProjectionViewer: Send + Sync + 'static {
    /// Stable label, disclosed in the served payload so a reader can tell which
    /// disclosure policy produced it.
    const LABEL: &'static str;
}

/// A remote agent that submitted a task over A2A. Sees capability-scoped
/// results and nothing else — "even if the requesting agent's software is
/// modified", because the boundary is enforced locally, by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemotePeerViewer;

impl ProjectionViewer for RemotePeerViewer {
    const LABEL: &'static str = "remote-peer";
}

/// What a projection is allowed to say. A closed, three-way vocabulary: text we
/// deliberately disclosed, an honest "not available from here", or nothing yet.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disclosure {
    /// Capability-scoped agent output.
    Text(String),
    /// The referenced work is bound to a host that is not this one. Reuses the
    /// existing spelling (`OrchestrationError::HostBoundUnavailable`,
    /// `RoomEvent::HostBoundUnavailable`) rather than minting a second.
    HostBoundUnavailable,
    /// Nothing to disclose yet (the task has not produced output).
    None,
}

/// A capability-scoped disclosure of locally-held state.
///
/// Construct through [`RoomProjection::disclose`]; the fields are private so a
/// caller cannot assemble one around unredacted state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomProjection<V: ProjectionViewer> {
    /// The task id — a value the submitter itself supplied, or one we minted for
    /// it. Never a node id, which encodes our internal addressing.
    task_id: String,
    /// The task's wire-facing lifecycle state.
    state: RapTaskState,
    /// The disclosure itself.
    disclosure: Disclosure,
    /// Additive, ignorable ownership metadata (`x-rustain-ownership`).
    ownership_peer_id: String,
    _viewer: PhantomData<V>,
}

impl<V: ProjectionViewer> RoomProjection<V> {
    /// Project internal state for `V`.
    ///
    /// `text` is the agent's answer; `host_root` and `forbidden` are host-local
    /// scrub inputs, taken by reference **purely to scrub** and never stored.
    /// A nonempty result is downgraded when it quotes the workspace root,
    /// contains any forbidden fragment, or contains an absolute-path-looking
    /// token. This is defence in depth on top of the structural guarantee: the
    /// type cannot carry a path, and suspect text is never served.
    #[must_use]
    pub fn disclose(
        task_id: impl Into<String>,
        state: RapTaskState,
        text: Option<&str>,
        host_root: &Path,
        forbidden: &[String],
        ownership_peer_id: impl Into<String>,
    ) -> Self {
        let disclosure = match text.map(str::trim).filter(|text| !text.is_empty()) {
            None => Disclosure::None,
            Some(text)
                if mentions_host_root(text, host_root)
                    || contains_forbidden_fragment(text, forbidden)
                    || contains_absolute_path_token(text) =>
            {
                Disclosure::HostBoundUnavailable
            }
            Some(text) => Disclosure::Text(text.to_owned()),
        };
        Self {
            task_id: task_id.into(),
            state,
            disclosure,
            ownership_peer_id: ownership_peer_id.into(),
            _viewer: PhantomData,
        }
    }

    #[must_use]
    pub fn state(&self) -> RapTaskState {
        self.state
    }

    #[must_use]
    pub fn disclosure(&self) -> &Disclosure {
        &self.disclosure
    }

    /// Render the A2A `Task` object this projection is allowed to produce.
    ///
    /// The wire state comes from [`rap_to_wire`] — never
    /// `serde_json::to_value(RapTaskState)`, which emits camelCase and talks a
    /// dialect no A2A agent speaks.
    #[must_use]
    pub fn to_task_json(&self) -> serde_json::Value {
        let mut status = serde_json::json!({ "state": rap_to_wire(self.state) });

        let text = match &self.disclosure {
            Disclosure::Text(text) => Some(text.clone()),
            Disclosure::HostBoundUnavailable => Some(
                "host-bound-unavailable: the result references state bound to a host that is not \
                 reachable from this disclosure"
                    .to_owned(),
            ),
            Disclosure::None => None,
        };

        if let Some(text) = text {
            status["message"] = serde_json::json!({
                "kind": "message",
                "role": "agent",
                "parts": [{ "kind": "text", "text": text }],
                // Additive and ignorable: a vanilla A2A client drops unknown
                // `metadata` keys, so declaring our ownership model here cannot
                // break interop (AC4b mutant c).
                "metadata": {
                    "x-rustain-ownership": {
                        "kind": "peer",
                        "viewer": V::LABEL,
                        "hostPeerId": self.ownership_peer_id,
                    }
                },
            });
        }

        serde_json::json!({
            "kind": "task",
            "id": self.task_id,
            "status": status,
        })
    }
}

/// Does `text` quote the workspace root (or a path under it)?
///
/// Compared case-sensitively on the raw string: this is a containment check
/// against one known literal, not a path-traversal decision.
fn mentions_host_root(text: &str, host_root: &Path) -> bool {
    let root = host_root.to_string_lossy();
    // A one-component root ("/" or "") would match everything; refuse to treat
    // it as a marker rather than blanking every disclosure.
    if root.len() < 2 {
        return false;
    }
    text.contains(root.as_ref())
}

/// Does `text` contain a host-sensitive fragment supplied by the runtime?
///
/// An empty fragment matches every string and deliberately fails closed. The
/// runtime owns fragment selection; this projection only enforces it.
fn contains_forbidden_fragment(text: &str, forbidden: &[String]) -> bool {
    forbidden.iter().any(|fragment| text.contains(fragment))
}

/// Does `text` contain a simple Unix-style absolute-path token?
///
/// Tokens start after whitespace or common prose delimiters. `/tmp/result` and
/// `~/project/result` are rejected; a bare `/` or `~/`, relative `docs/a.md`,
/// and an `https://` URL are not. This is deliberately a deterministic
/// disclosure heuristic, not a filesystem parser.
fn contains_absolute_path_token(text: &str) -> bool {
    text.split(|character: char| {
        character.is_whitespace()
            || matches!(
                character,
                '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | '"' | '\'' | '`' | ',' | ';' | '='
            )
    })
    .any(|token| {
        token
            .strip_prefix("~/")
            .is_some_and(|path| !path.is_empty())
            || token.strip_prefix('/').is_some_and(|path| !path.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "peer-abc";

    fn root() -> &'static Path {
        Path::new("/home/dev/secret-workspace")
    }

    #[test]
    fn a_disclosed_result_carries_text_and_the_wire_state() {
        let projection = RoomProjection::<RemotePeerViewer>::disclose(
            "task-1",
            RapTaskState::Completed,
            Some("The corpus contains 141 parseable agent cards."),
            root(),
            &[],
            HOST,
        );
        let json = projection.to_task_json();
        assert_eq!(json["kind"], "task");
        assert_eq!(json["id"], "task-1");
        assert_eq!(json["status"]["state"], "completed");
        assert_eq!(
            json["status"]["message"]["parts"][0]["text"],
            "The corpus contains 141 parseable agent cards."
        );
    }

    #[test]
    fn multiword_states_render_hyphenated_never_camelcase() {
        // The camelCase trap is only detectable on a multiword state.
        let projection = RoomProjection::<RemotePeerViewer>::disclose(
            "task-1",
            RapTaskState::AuthRequired,
            None,
            root(),
            &[],
            HOST,
        );
        assert_eq!(
            projection.to_task_json()["status"]["state"],
            "auth-required"
        );
        assert_ne!(
            projection.to_task_json()["status"]["state"],
            serde_json::to_value(RapTaskState::AuthRequired).unwrap()
        );
    }

    #[test]
    fn text_quoting_the_workspace_root_is_downgraded_not_served() {
        let projection = RoomProjection::<RemotePeerViewer>::disclose(
            "task-1",
            RapTaskState::Completed,
            Some("I read /home/dev/secret-workspace/notes.md"),
            root(),
            &[],
            HOST,
        );
        assert_eq!(projection.disclosure(), &Disclosure::HostBoundUnavailable);
        let rendered = projection.to_task_json().to_string();
        assert!(!rendered.contains("secret-workspace"), "{rendered}");
        assert!(rendered.contains("host-bound-unavailable"), "{rendered}");
    }

    #[test]
    fn forbidden_fragments_are_downgraded_not_served() {
        let forbidden =
            vec!["System instruction: preserve this host-local execution detail.".to_owned()];
        let projection = RoomProjection::<RemotePeerViewer>::disclose(
            "task-1",
            RapTaskState::Completed,
            Some(
                "Completed. System instruction: preserve this host-local execution detail. \
                 Summary follows.",
            ),
            root(),
            &forbidden,
            HOST,
        );

        assert_eq!(projection.disclosure(), &Disclosure::HostBoundUnavailable);
        let rendered = projection.to_task_json().to_string();
        assert!(!rendered.contains(&forbidden[0]), "{rendered}");
        assert!(rendered.contains("host-bound-unavailable"), "{rendered}");
    }

    #[test]
    fn absolute_path_tokens_are_downgraded() {
        for text in [
            "The review completed; report is /tmp/rustain/report.txt.",
            "The review completed; report is ~/private/report.txt.",
        ] {
            let projection = RoomProjection::<RemotePeerViewer>::disclose(
                "task-1",
                RapTaskState::Completed,
                Some(text),
                root(),
                &[],
                HOST,
            );
            assert_eq!(
                projection.disclosure(),
                &Disclosure::HostBoundUnavailable,
                "{text}"
            );
        }
    }

    #[test]
    fn relative_paths_and_normal_prose_remain_disclosable() {
        for text in [
            "The review completed with no findings.",
            "Read docs/review-summary.md and returned its title.",
            "See https://a2a.example.test/card for the public protocol details.",
        ] {
            let projection = RoomProjection::<RemotePeerViewer>::disclose(
                "task-1",
                RapTaskState::Completed,
                Some(text),
                root(),
                &[],
                HOST,
            );
            assert_eq!(
                projection.disclosure(),
                &Disclosure::Text(text.to_owned()),
                "{text}"
            );
        }
    }

    #[test]
    fn a_degenerate_root_does_not_blank_every_disclosure() {
        // Positive control for the scrub: "/" must not be treated as a marker.
        let projection = RoomProjection::<RemotePeerViewer>::disclose(
            "task-1",
            RapTaskState::Completed,
            Some("done"),
            Path::new("/"),
            &[],
            HOST,
        );
        assert_eq!(
            projection.disclosure(),
            &Disclosure::Text("done".to_owned())
        );
    }

    #[test]
    fn ownership_metadata_is_additive_and_nested_under_metadata() {
        let json = RoomProjection::<RemotePeerViewer>::disclose(
            "task-1",
            RapTaskState::Completed,
            Some("ok"),
            root(),
            &[],
            HOST,
        )
        .to_task_json();
        // A vanilla A2A parse reads kind/id/status/state/message/parts and never
        // trips over the extension: it lives under `metadata`, and no required
        // field was renamed or removed to make room for it.
        assert!(json["status"]["message"]["metadata"]["x-rustain-ownership"].is_object());
        assert_eq!(
            json["status"]["message"]["metadata"]["x-rustain-ownership"]["viewer"],
            RemotePeerViewer::LABEL
        );
        assert_eq!(json["status"]["message"]["role"], "agent");
    }

    /// AC4b, K4b-structural (Rule 4). Redaction is the type: this test pins the
    /// projection's *complete* field list, so a future field that could carry a
    /// workspace root, tool argv, or a system prompt cannot be added silently.
    #[test]
    fn the_projection_admits_no_host_state() {
        let source = include_str!("projection.rs");
        let start = source
            .find("pub struct RoomProjection<V: ProjectionViewer> {")
            .expect("struct declaration");
        let body = &source[start..];
        let end = body.find("\n}").expect("struct terminator");
        let body = &body[..end];

        let fields: Vec<&str> = body
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.starts_with("//") || line.starts_with("///") || !line.contains(':') {
                    return None;
                }
                line.split(':').next().map(str::trim)
            })
            .filter(|name| !name.is_empty() && *name != "pub struct RoomProjection<V")
            .collect();

        assert_eq!(
            fields,
            vec![
                "task_id",
                "state",
                "disclosure",
                "ownership_peer_id",
                "_viewer"
            ],
            "RoomProjection gained or lost a field. Every member must be a value \
             the remote submitter is entitled to see: no PathBuf, no OsString, no \
             argv vector, no system prompt, no AgentId/NodeState/RoomEvent handle. \
             If the new field is genuinely capability-scoped, add it here."
        );

        for forbidden in [
            "PathBuf",
            "OsString",
            "AgentId",
            "NodeState",
            "RoomEvent",
            "ChatMessage",
            "Conversation",
            "argv",
            "system_prompt",
            "workspace",
        ] {
            assert!(
                !body.contains(forbidden),
                "RoomProjection must not hold {forbidden}: redaction is the type, not a pass"
            );
        }
    }
}
