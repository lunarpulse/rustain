//! `/room` and `/room role grant|revoke|list` — Story 18.3a, AC1 / AC4.
//!
//! Pure parser and renderers: data in, data out. Every effect (the journal
//! read, the projection refresh, the durable append) happens in
//! [`crate::infrastructure::runtime::room_bridge`], because the handler
//! contract forbids `crate::infrastructure::*` imports here.
//!
//! Named `room_command`, not `handle_room_command`: `tests/conformance.rs`
//! pins `EXPECTED_HANDLE_COUNT` with an exact `assert_eq!` over
//! `^\s*pub(\(crate\))?\s+(async\s+)?fn\s+handle_[a-z_]+\(`, and bumping it is
//! a governance decision requiring a `RATCHET-SIGNOFF` trailer. Nothing here
//! needs a `HandlerOutcome`, so the counter stays untouched — the same choice
//! `handlers/team_command.rs` made.

use crate::adapters::tui::state::TuiState;
use crate::domain::models::RoomRole;
use crate::domain::ports::RoomRoleState;

/// The valid sub-verb set, named verbatim in every parser refusal.
pub const USAGE: &str = "/room | /room role list | /room role grant <alias-or-peer-id> \
                         <owner|editor|viewer> | /room role revoke <alias-or-peer-id>";

/// Stable id for the in-chat `/room role` result block. A role listing is a
/// **view**, not an event stream: re-running it replaces the block rather than
/// stacking another copy under the old one.
pub const ROOM_BLOCK_ID: &str = "room-role";

#[derive(Debug, PartialEq, Eq)]
pub enum RoomCommandArgs {
    /// Bare `/room` — open the durable-room viewer panel.
    View,
    /// `/room role list`.
    RoleList,
    /// `/room role grant <target> <role>`.
    RoleGrant { target: String, role: RoomRole },
    /// `/room role revoke <target>`.
    RoleRevoke { target: String },
}

/// Parse one `/room` subcommand. Bare `/room` remains the viewer.
pub fn parse_room_command(cmd_arg: Option<&str>) -> Result<RoomCommandArgs, String> {
    let arg = cmd_arg.map(str::trim).unwrap_or("");
    let mut tokens = arg.split_whitespace();
    let Some(verb) = tokens.next() else {
        return Ok(RoomCommandArgs::View);
    };
    if verb != "role" {
        return Err(format!("Unknown /room subcommand '{verb}'. Use: {USAGE}"));
    }
    let action = tokens
        .next()
        .ok_or_else(|| format!("Missing /room role action. Use: {USAGE}"))?;
    match action {
        "list" => {
            if tokens.next().is_some() {
                return Err(format!(
                    "'/room role list' takes no arguments. Use: {USAGE}"
                ));
            }
            Ok(RoomCommandArgs::RoleList)
        }
        "grant" => {
            let target = tokens
                .next()
                .ok_or_else(|| format!("Missing peer target. Use: {USAGE}"))?;
            let role = tokens
                .next()
                .ok_or_else(|| format!("Missing role. Use: {USAGE}"))?;
            // An unrecognised word typed at the prompt is an operator error to
            // report, never a silent fall to `RoomRole::Unknown` — that
            // fallback exists for durable wire values, not for the keyboard.
            let role = RoomRole::parse_grantable(role).ok_or_else(|| {
                format!("Unknown role '{role}' — valid: owner, editor, viewer. Use: {USAGE}")
            })?;
            if tokens.next().is_some() {
                return Err(format!(
                    "Expected exactly '<alias-or-peer-id> <role>' after 'grant'. Use: {USAGE}"
                ));
            }
            Ok(RoomCommandArgs::RoleGrant {
                target: target.to_owned(),
                role,
            })
        }
        "revoke" => {
            let target = tokens
                .next()
                .ok_or_else(|| format!("Missing peer target. Use: {USAGE}"))?;
            if tokens.next().is_some() {
                return Err(format!(
                    "Expected one alias or PeerId after 'revoke'. Use: {USAGE}"
                ));
            }
            Ok(RoomCommandArgs::RoleRevoke {
                target: target.to_owned(),
            })
        }
        other => Err(format!("Unknown /room role action '{other}'. Use: {USAGE}")),
    }
}

/// Render `/room role list`.
///
/// ⛔ **Journaled fact, not enforcement claim.** Every line states what the
/// journal knows — "granted role R" / "role revoked" — and never that a peer
/// can or cannot now read, send, or decrypt anything. Nothing here implies
/// rekeying or cryptographic exclusion (`DF-18-CRYPTO-CLUSTER` C4).
#[must_use]
pub fn render_role_list(
    roles: &[(String, crate::domain::models::PeerId, RoomRoleState)],
    read_error: Option<&str>,
) -> String {
    let mut out = String::from("Room roles (journaled)");
    if let Some(error) = read_error {
        out.push_str(&format!(
            "\n⚠ could not read the room journal: {error}\nEvery role reads as the least \
             privileged value until the journal is readable again."
        ));
        return out;
    }
    if roles.is_empty() {
        out.push_str(
            "\n· no room roles recorded. The local operator holds room ownership implicitly.",
        );
        return out;
    }
    for (label, peer, state) in roles {
        let line = match state {
            RoomRoleState::Granted(role) => format!("granted role '{}'", role.label()),
            _ => "role revoked".to_owned(),
        };
        out.push_str(&format!("\n- {label} ({peer}): {line}"));
    }
    out
}

/// Put one `/room role` result in the transcript as a replaceable block.
pub(crate) fn show_room_message(state: &mut TuiState, message: String) {
    state.feedback_blocks.insert(
        ROOM_BLOCK_ID.to_owned(),
        crate::domain::models::FeedbackBlock {
            id: ROOM_BLOCK_ID.to_owned(),
            level: crate::domain::models::FeedbackLevel::Info,
            message,
            actions: Vec::new(),
        },
    );
    state.active_feedback_id = Some(ROOM_BLOCK_ID.to_owned());
    state.needs_redraw = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_room_opens_the_viewer_and_role_verbs_parse() {
        assert_eq!(parse_room_command(None).unwrap(), RoomCommandArgs::View);
        assert_eq!(
            parse_room_command(Some("   ")).unwrap(),
            RoomCommandArgs::View
        );
        assert_eq!(
            parse_room_command(Some("role list")).unwrap(),
            RoomCommandArgs::RoleList
        );
        assert_eq!(
            parse_room_command(Some("role grant alice editor")).unwrap(),
            RoomCommandArgs::RoleGrant {
                target: "alice".to_owned(),
                role: RoomRole::Editor,
            }
        );
        assert_eq!(
            parse_room_command(Some("role revoke alice")).unwrap(),
            RoomCommandArgs::RoleRevoke {
                target: "alice".to_owned(),
            }
        );
    }

    #[test]
    fn a_mistyped_role_is_an_operator_error_not_a_silent_unknown() {
        let error = parse_room_command(Some("role grant alice ownr")).unwrap_err();
        assert!(error.contains("Unknown role 'ownr'"), "{error}");
        assert!(error.contains("owner, editor, viewer"), "{error}");
        // The wire-level `Unknown` fallback must never be reachable by typing.
        for spec in ["role grant alice unknown", "role grant alice archivist"] {
            assert!(parse_room_command(Some(spec)).is_err(), "{spec}");
        }
    }

    #[test]
    fn malformed_invocations_name_the_usage_line() {
        for spec in [
            "roles",
            "role",
            "role grant",
            "role grant alice",
            "role grant alice editor extra",
            "role revoke",
            "role revoke alice extra",
            "role list extra",
            "role frobnicate alice",
        ] {
            let error = parse_room_command(Some(spec)).unwrap_err();
            assert!(error.contains(USAGE), "{spec} → {error}");
        }
    }

    #[test]
    fn the_role_list_states_emptiness_and_read_failure_out_loud() {
        assert!(render_role_list(&[], None).contains("no room roles recorded"));
        let failed = render_role_list(&[], Some("disk went away"));
        assert!(failed.contains("could not read the room journal"));
        assert!(failed.contains("least privileged"));
    }

    #[test]
    fn the_role_list_reports_journaled_facts_without_enforcement_claims() {
        let peer = crate::domain::models::PeerId::from_public_key(&[5u8; 32]).unwrap();
        let rendered = render_role_list(
            &[
                (
                    "alice".to_owned(),
                    peer.clone(),
                    RoomRoleState::Granted(RoomRole::Editor),
                ),
                ("bob".to_owned(), peer.clone(), RoomRoleState::Revoked),
            ],
            None,
        );
        assert!(rendered.contains("alice"));
        assert!(rendered.contains("granted role 'editor'"));
        assert!(rendered.contains("bob"));
        assert!(rendered.contains("role revoked"));
        let lowered = rendered.to_ascii_lowercase();
        for forbidden in [
            "rekey",
            "key rotation",
            "can no longer",
            "excluded",
            "recalled",
        ] {
            assert!(
                !lowered.contains(forbidden),
                "forbidden wording: {rendered}"
            );
        }
    }
}
