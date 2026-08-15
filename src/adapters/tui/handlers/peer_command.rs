//! Pure `/peer` parser (Story 18.4b).
//!
//! Effect-free and adapter-local: this module MUST NOT import anything from
//! `crate::infrastructure::*`, the invariant `tests/conformance.rs` and
//! `tools/ci/check-handler-extraction-scope.sh` enforce for every handler.
//!
//! Every sub-verb here has the same name and the same meaning as the CLI verb
//! (`rustain peer …`); the shell that runs them is
//! `infrastructure::runtime::peer_bridge`, shared with the CLI.

use crate::adapters::tui::state::TuiState;

const USAGE: &str = "/peer list [--json] | /peer show <alias> | /peer invite [--ttl=<dur>] \
                     [--name=<name>] | /peer add <alias> <ticket> [--allow-local-addresses] | \
                     /peer revoke <alias-or-peer-id>";

/// One parsed `/peer` sub-verb.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerCommandArgs {
    /// Bare `/peer` and `/peer list` both open the roster.
    List {
        json: bool,
    },
    Show {
        alias: String,
    },
    Invite {
        ttl: Option<String>,
        name: Option<String>,
    },
    Add {
        alias: String,
        ticket: String,
        /// Story 18.4d (D15). Opt in, **per import**, to a ticket whose claimed
        /// address points into this machine or this local network — which two
        /// hosts on one machine legitimately need. ⛔ Not a confirm bypass: the
        /// fingerprint confirm still happens and still has no flag.
        allow_local_addresses: bool,
    },
    Revoke {
        target: String,
    },
}

/// Parse one `/peer` subcommand. Bare `/peer` is the roster.
///
/// ⛔ There is no `--yes` and no `--force` on `add`: an unknown flag is an error
/// rather than something ignored, because a flag the operator believes skipped
/// the confirm must never appear to be accepted.
pub fn parse_peer_command(cmd_arg: Option<&str>) -> Result<PeerCommandArgs, String> {
    let arg = cmd_arg.map(str::trim).unwrap_or("");
    let mut tokens = arg.split_whitespace();
    let Some(verb) = tokens.next() else {
        return Ok(PeerCommandArgs::List { json: false });
    };
    match verb {
        "list" => {
            let mut json = false;
            for token in tokens {
                match token {
                    "--json" => json = true,
                    other => {
                        return Err(format!("Unknown /peer list flag '{other}'. Use: {USAGE}"));
                    }
                }
            }
            Ok(PeerCommandArgs::List { json })
        }
        "show" => {
            let alias = tokens
                .next()
                .ok_or_else(|| format!("Missing alias. Use: {USAGE}"))?;
            if tokens.next().is_some() {
                return Err(format!("Expected one alias after 'show'. Use: {USAGE}"));
            }
            Ok(PeerCommandArgs::Show {
                alias: alias.to_owned(),
            })
        }
        "invite" => {
            let mut ttl = None;
            let mut name = None;
            for token in tokens {
                if let Some(value) = token.strip_prefix("--ttl=") {
                    if value.is_empty() {
                        return Err(format!("'--ttl=' needs a duration. Use: {USAGE}"));
                    }
                    ttl = Some(value.to_owned());
                } else if let Some(value) = token.strip_prefix("--name=") {
                    if value.is_empty() {
                        return Err(format!("'--name=' needs a name. Use: {USAGE}"));
                    }
                    name = Some(value.to_owned());
                } else {
                    return Err(format!("Unknown /peer invite flag '{token}'. Use: {USAGE}"));
                }
            }
            Ok(PeerCommandArgs::Invite { ttl, name })
        }
        "add" => {
            let alias = tokens.next().ok_or_else(|| {
                format!("Missing alias. You choose it, not the ticket. Use: {USAGE}")
            })?;
            let ticket = tokens
                .next()
                .ok_or_else(|| format!("Missing ticket. Use: {USAGE}"))?;
            let mut allow_local_addresses = false;
            for extra in tokens {
                // Naming the forbidden flags explicitly: an operator who tries
                // one must be told it does not exist, not have it ignored.
                if extra.starts_with("--yes") || extra.starts_with("--force") {
                    return Err(
                        "There is no way to skip the confirm: it is the trust gate, not a \
                         formality. Compare the fingerprint, then answer [y]."
                            .to_owned(),
                    );
                }
                if extra == "--allow-local-addresses" {
                    allow_local_addresses = true;
                    continue;
                }
                return Err(format!(
                    "Expected '<alias> <ticket> [--allow-local-addresses]' after 'add'. Use: \
                     {USAGE}"
                ));
            }
            Ok(PeerCommandArgs::Add {
                alias: alias.to_owned(),
                ticket: ticket.to_owned(),
                allow_local_addresses,
            })
        }
        "revoke" => {
            let target = tokens
                .next()
                .ok_or_else(|| format!("Missing peer target. Use: {USAGE}"))?;
            for token in tokens {
                // `--now` is accepted for compatibility with the planning text
                // and changes nothing; admission is already checked per frame.
                if token != "--now" {
                    return Err(format!(
                        "Expected one alias or peer id after 'revoke'. Use: {USAGE}"
                    ));
                }
            }
            Ok(PeerCommandArgs::Revoke {
                target: target.to_owned(),
            })
        }
        other => Err(format!("Unknown /peer subcommand '{other}'. Use: {USAGE}")),
    }
}

/// Push one block of `peer` output into the transcript.
pub fn show_peer_message(state: &mut TuiState, message: String) {
    let block = crate::domain::models::FeedbackBlock {
        id: format!("peer-{}", state.feedback_blocks.len() + 1),
        level: crate::domain::models::FeedbackLevel::Info,
        message,
        actions: vec![crate::domain::models::FeedbackAction::Dismiss],
    };
    state.feedback_blocks.insert(block.id.clone(), block);
    state.needs_redraw = true;
}

/// Push the key-mismatch alarm as a never-truncated error block.
///
/// `FeedbackLevel::Error` is exempt from the three-line cap that bounds
/// `Warning`, which is exactly why the alarm uses it: both fingerprints and the
/// two-verb remedy must survive at the 60×16 layout floor. ⛔ It carries **no**
/// actions, so no keystroke resolves it into an accept.
pub fn show_peer_alarm(state: &mut TuiState, message: String) {
    let block = crate::domain::models::FeedbackBlock {
        id: format!("peer-alarm-{}", state.feedback_blocks.len() + 1),
        level: crate::domain::models::FeedbackLevel::Error,
        message,
        actions: vec![],
    };
    state.feedback_blocks.insert(block.id.clone(), block);
    state.needs_redraw = true;
}

/// Take the pending confirm and restore focus. Returns it only on accept.
pub fn resolve_peer_add_card(
    state: &mut TuiState,
    accept: bool,
) -> Option<crate::adapters::tui::state::PendingPeerAdd> {
    let pending = state.pending_peer_add.take()?;
    state.focus = pending.prior_focus.clone();
    state.needs_redraw = true;
    accept.then_some(pending)
}
