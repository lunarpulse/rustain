//! A2A JSON-RPC task-state vocabulary and its explicit, total, bidirectional
//! mapping onto the domain [`RapTaskState`] FSM.
//!
//! Ruling 1 (Story 17.4b): `RapTaskState`'s `serde` is **not** the A2A wire
//! codec. `RapTaskState` derives `camelCase` and emits `"inputRequired"`; the
//! A2A JSON-RPC wire uses `"input-required"` — hyphenated. A `serde_json`
//! passthrough compiles, looks right, and silently talks a dialect no real agent
//! speaks. This module is the explicit mapping that Ruling 1 mandates.
//!
//! Ruling 1b (post-spike): the JSON-RPC binding is a **single** dialect
//! (lowercase-hyphen) on both v0.3 and v1.0 agents — there is no two-arm codec.
//! An unrecognized spelling (camelCase, or the HTTP+JSON/gRPC `TASK_STATE_*`
//! proto encoding) is a **typed refusal**, never a silent mis-parse.

use crate::domain::models::RapTaskState;

use super::error::A2aError;

/// The A2A JSON-RPC task-state vocabulary. Nine values: the eight that map onto
/// [`RapTaskState`] plus `Unknown`, which the spec defines and `RapTaskState`
/// deliberately has no variant for (Ruling 4: hold state, log, keep polling).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum A2aTaskState {
    Submitted,
    Working,
    InputRequired,
    AuthRequired,
    Completed,
    Failed,
    Canceled,
    Rejected,
    Unknown,
}

impl A2aTaskState {
    /// Every JSON-RPC wire value, in canonical order. Pinned by the table-driven
    /// test so a dropped variant is caught loudly.
    pub const WIRE_VALUES: [&'static str; 9] = [
        "submitted",
        "working",
        "input-required",
        "auth-required",
        "completed",
        "failed",
        "canceled",
        "rejected",
        "unknown",
    ];

    /// Parse a JSON-RPC wire task-state string. The binding is lowercase-hyphen;
    /// any other spelling — camelCase (`"inputRequired"`), the proto-JSON
    /// `TASK_STATE_*` family, or anything unknown — is a typed refusal.
    pub fn from_wire(raw: &str) -> Result<Self, A2aError> {
        match raw {
            "submitted" => Ok(Self::Submitted),
            "working" => Ok(Self::Working),
            "input-required" => Ok(Self::InputRequired),
            "auth-required" => Ok(Self::AuthRequired),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "canceled" => Ok(Self::Canceled),
            "rejected" => Ok(Self::Rejected),
            "unknown" => Ok(Self::Unknown),
            other => Err(A2aError::UnknownTaskState {
                raw: other.to_owned(),
            }),
        }
    }

    /// The lowercase-hyphen JSON-RPC wire spelling.
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Submitted => "submitted",
            Self::Working => "working",
            Self::InputRequired => "input-required",
            Self::AuthRequired => "auth-required",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::Rejected => "rejected",
            Self::Unknown => "unknown",
        }
    }

    /// Project onto the domain FSM. `Unknown` has no [`RapTaskState`] — the caller
    /// holds current state and keeps polling (Ruling 4).
    pub const fn to_rap(self) -> Option<RapTaskState> {
        match self {
            Self::Submitted => Some(RapTaskState::Submitted),
            Self::Working => Some(RapTaskState::Working),
            Self::InputRequired => Some(RapTaskState::InputRequired),
            Self::AuthRequired => Some(RapTaskState::AuthRequired),
            Self::Completed => Some(RapTaskState::Completed),
            Self::Failed => Some(RapTaskState::Failed),
            Self::Canceled => Some(RapTaskState::Canceled),
            Self::Rejected => Some(RapTaskState::Rejected),
            Self::Unknown => None,
        }
    }
}

/// Domain → wire. The **only** correct way to put a [`RapTaskState`] onto the
/// A2A JSON-RPC wire — never `serde_json::to_value(rap_state)`, which emits
/// camelCase (Ruling 1). Total and infallible.
pub const fn rap_to_wire(state: RapTaskState) -> &'static str {
    match state {
        RapTaskState::Submitted => "submitted",
        RapTaskState::Working => "working",
        RapTaskState::InputRequired => "input-required",
        RapTaskState::AuthRequired => "auth-required",
        RapTaskState::Completed => "completed",
        RapTaskState::Failed => "failed",
        RapTaskState::Canceled => "canceled",
        RapTaskState::Rejected => "rejected",
    }
}

/// Wire → domain. `Ok(None)` for the spec's `"unknown"` (hold state); `Err` for
/// any unrecognized spelling.
pub fn wire_to_rap(raw: &str) -> Result<Option<RapTaskState>, A2aError> {
    Ok(A2aTaskState::from_wire(raw)?.to_rap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_wire_value_maps_and_round_trips() {
        // Table-driven over all nine JSON-RPC wire values (AC1).
        let table: [(&str, A2aTaskState, Option<RapTaskState>); 9] = [
            (
                "submitted",
                A2aTaskState::Submitted,
                Some(RapTaskState::Submitted),
            ),
            (
                "working",
                A2aTaskState::Working,
                Some(RapTaskState::Working),
            ),
            (
                "input-required",
                A2aTaskState::InputRequired,
                Some(RapTaskState::InputRequired),
            ),
            (
                "auth-required",
                A2aTaskState::AuthRequired,
                Some(RapTaskState::AuthRequired),
            ),
            (
                "completed",
                A2aTaskState::Completed,
                Some(RapTaskState::Completed),
            ),
            ("failed", A2aTaskState::Failed, Some(RapTaskState::Failed)),
            (
                "canceled",
                A2aTaskState::Canceled,
                Some(RapTaskState::Canceled),
            ),
            (
                "rejected",
                A2aTaskState::Rejected,
                Some(RapTaskState::Rejected),
            ),
            ("unknown", A2aTaskState::Unknown, None),
        ];
        assert_eq!(table.len(), A2aTaskState::WIRE_VALUES.len());
        for (wire, state, rap) in table {
            assert_eq!(
                A2aTaskState::from_wire(wire).unwrap(),
                state,
                "parse {wire}"
            );
            assert_eq!(state.as_wire(), wire, "as_wire {wire}");
            assert_eq!(state.to_rap(), rap, "to_rap {wire}");
            assert_eq!(wire_to_rap(wire).unwrap(), rap, "wire_to_rap {wire}");
        }
    }

    #[test]
    fn rap_states_emit_hyphenated_wire_never_camelcase() {
        // The Ruling-1 trap: RapTaskState::as_str() emits camelCase. rap_to_wire
        // must emit hyphenated. This is the whole reason the mapping exists.
        assert_eq!(rap_to_wire(RapTaskState::InputRequired), "input-required");
        assert_eq!(rap_to_wire(RapTaskState::AuthRequired), "auth-required");
        assert_ne!(
            rap_to_wire(RapTaskState::InputRequired),
            RapTaskState::InputRequired.as_str(),
            "as_str() is camelCase and must never reach the wire"
        );
    }

    #[test]
    fn camelcase_spelling_is_refused_not_silently_parsed() {
        // A serde passthrough would accept camelCase; the mapping refuses it.
        for camel in ["inputRequired", "authRequired"] {
            let error = A2aTaskState::from_wire(camel).expect_err("camelCase must be refused");
            assert!(matches!(error, A2aError::UnknownTaskState { ref raw } if raw == camel));
        }
    }

    #[test]
    fn proto_json_task_state_encoding_is_refused_loudly() {
        // A peer emitting the HTTP+JSON/gRPC `TASK_STATE_*` encoding over the
        // JSON-RPC binding must be refused, never mis-parsed (Ruling 1b post-spike).
        let error = A2aTaskState::from_wire("TASK_STATE_SUBMITTED")
            .expect_err("proto-JSON encoding must be refused on the JSON-RPC binding");
        assert!(matches!(error, A2aError::UnknownTaskState { .. }));
    }

    #[test]
    fn serde_passthrough_of_rap_state_would_talk_the_wrong_dialect() {
        // Documents the trap: serializing RapTaskState directly emits camelCase,
        // which from_wire then refuses. Proof that the passthrough is broken.
        let json = serde_json::to_string(&RapTaskState::InputRequired).unwrap();
        assert_eq!(json, "\"inputRequired\"");
        let unquoted = json.trim_matches('"');
        assert!(A2aTaskState::from_wire(unquoted).is_err());
    }
}
