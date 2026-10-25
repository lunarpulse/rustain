//! Pure grammar, send verdict and operator copy for the daemon-routed CLI.

use crate::domain::models::{
    RecipientOutcome, SemanticMessageType, StoryManifest, semantic_message_type_tokens,
};
use crate::domain::services::send_action_ledger::validate_subject;
use clap::{
    Args,
    builder::{PossibleValuesParser, TypedValueParser},
};

pub const INVALID_PEERS: &str = "Invalid or duplicate peer id in 'rustain team send'";
pub const NO_DAEMON: &str = "No daemon is running for this workspace, so nothing was sent. Start one with `rustain daemon start` — the daemon owns the A2A runtime and the send journal this verb uses.";
pub const WRITER_HELD: &str = "Another client holds this daemon's writer slot, so nothing was sent. Use /team send in that session, or detach it and retry.";
pub const NOT_EVERY_RECIPIENT: &str = "not every recipient received the message";
pub const OUTCOME_UNKNOWN: &str = "The daemon stopped answering before the send outcome was known; do not assume nothing was sent.";

/// Flags stop being flags as soon as the peer-list positional begins.
#[derive(Args, Debug, Clone)]
pub struct SendArgs {
    /// Semantic message type; flags must precede peers.
    #[arg(long = "type", value_parser = PossibleValuesParser::new(semantic_message_type_tokens()).map(|token| SemanticMessageType::parse(&token)))]
    pub message_type: Option<SemanticMessageType>,
    /// Optional sender-local, single-token label (at most 64 characters).
    #[arg(long, value_parser = parse_subject)]
    pub subject: Option<String>,
    /// Story keys and optional recipient aliases, for story_assignment only.
    #[arg(long, value_parser = parse_stories)]
    pub stories: Option<StoryManifest>,
    /// Comma-separated peers followed by message text; everything after peers is body.
    #[arg(value_name = "PEERS TEXT", trailing_var_arg = true, allow_hyphen_values = true, num_args = 1..)]
    pub values: Vec<String>,
}

fn parse_subject(value: &str) -> Result<String, String> {
    validate_subject(value).map_err(|error| error.to_string())?;
    Ok(value.to_owned())
}

fn parse_stories(value: &str) -> Result<StoryManifest, String> {
    StoryManifest::parse(value).map_err(|error| error.to_string())
}

#[derive(Debug)]
pub struct SendRequest {
    pub recipients: Vec<String>,
    pub text: String,
    pub message_type: Option<SemanticMessageType>,
    pub subject: Option<String>,
    pub stories: Option<StoryManifest>,
}

impl SendArgs {
    pub fn parse(&self) -> Result<SendRequest, String> {
        let peers = self
            .values
            .first()
            .ok_or_else(|| INVALID_PEERS.to_owned())?;
        let recipients: Vec<String> = peers.split(',').map(str::to_owned).collect();
        let mut seen = std::collections::HashSet::with_capacity(recipients.len());
        if recipients.iter().any(|peer| {
            peer.is_empty() || peer.chars().any(char::is_whitespace) || !seen.insert(peer)
        }) {
            return Err(INVALID_PEERS.to_owned());
        }
        let text = self.values[1..].join(" ");
        if text.trim().is_empty() {
            return Err(format!("Missing message text after peers `{peers}`"));
        }
        if let Some(stories) = &self.stories {
            stories
                .check(self.message_type, &recipients)
                .map_err(|error| error.to_string())?;
        }
        Ok(SendRequest {
            recipients,
            text,
            message_type: self.message_type,
            subject: self.subject.clone(),
            stories: self.stories.clone(),
        })
    }
}

pub fn reached(outcome: &RecipientOutcome) -> bool {
    match outcome {
        RecipientOutcome::Delivered
        | RecipientOutcome::AcceptedWithoutItem
        | RecipientOutcome::AwaitingApproval => true,
        RecipientOutcome::Declined
        | RecipientOutcome::AskedQuestion { .. }
        | RecipientOutcome::Unreachable { .. }
        | RecipientOutcome::NoUsableAnswer
        | RecipientOutcome::NotSent => false,
    }
}

pub fn started(count: usize, action: &crate::domain::models::SendActionId) -> String {
    let action = action.as_str();
    format!(
        "Sending to {count} recipient(s) as action {action}. Interrupting stops only this wait — the daemon finishes the send; '/team board --action {action}' shows it."
    )
}

pub fn version_mismatch(daemon: u32, client: u32) -> String {
    format!(
        "This daemon runs a different rustain build (attach protocol v{daemon}; this command speaks v{client}), so nothing was sent. Restart it with this build: `rustain daemon stop`, then `rustain daemon start`."
    )
}

pub fn request_refused(reason: &str) -> String {
    format!("Nothing was sent: {reason}")
}

pub fn attach_failed(reason: &impl std::fmt::Display) -> String {
    format!("Could not attach to the daemon: {reason}. Nothing was sent.")
}

pub fn incomplete(action: Option<&crate::domain::models::SendActionId>) -> String {
    action.map_or_else(|| OUTCOME_UNKNOWN.to_owned(), |action| {
        let action = action.as_str();
        format!("The daemon stopped answering before every recipient settled; the send is daemon-owned — '/team board --action {action}' shows the rest.")
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::cli::commands::{Cli, Command};
    use crate::adapters::cli::team::TeamAction;
    use clap::Parser;

    fn parse(tail: &[&str]) -> Result<SendRequest, String> {
        let cli = Cli::try_parse_from(
            ["rustain", "team", "send"]
                .into_iter()
                .chain(tail.iter().copied()),
        )
        .map_err(|error| error.to_string())?;
        let Some(Command::Team {
            action: TeamAction::Send(args),
        }) = cli.command
        else {
            panic!("send action");
        };
        args.parse()
    }

    #[test]
    fn send_grammar_preserves_body_and_flag_shaped_tokens() {
        for tail in [
            vec!["alice", "--type", "is", "wrong"],
            vec!["alice", "--subject", "x"],
            vec!["alice", "--stories", "S1"],
            vec!["alice", "--help"],
        ] {
            let request = parse(&tail).unwrap();
            assert_eq!(request.text, tail[1..].join(" "));
            assert_eq!(request.message_type, None);
        }
        let body = "two  spaces\nline two — ünï";
        assert_eq!(parse(&["alice,bob", body]).unwrap().text, body);
        assert_eq!(
            parse(&["alice,bob", "one", "two"]).unwrap().recipients,
            ["alice", "bob"]
        );
        assert_eq!(
            parse(&["alice"]).unwrap_err(),
            "Missing message text after peers `alice`"
        );
        assert_eq!(
            parse(&["alice", " \n\t"]).unwrap_err(),
            "Missing message text after peers `alice`"
        );
    }

    #[test]
    fn send_grammar_refuses_invalid_recipients_subject_type_and_stories() {
        for peers in [
            "",
            ",alice",
            "alice,",
            "alice,,bob",
            "alice,alice",
            "alice, bob",
            "alice,al\nice",
        ] {
            assert_eq!(parse(&[peers, "hello"]).unwrap_err(), INVALID_PEERS);
        }
        assert!(parse(&["--type", "typo", "alice", "hello"]).is_err());
        assert!(parse(&["--subject", "two words", "alice", "hello"]).is_err());
        assert!(parse(&["--subject", &"x".repeat(65), "alice", "hello"]).is_err());
        assert!(
            parse(&["--stories", "S1", "alice", "hello"])
                .unwrap_err()
                .contains("needs '--type story_assignment'")
        );
        assert!(
            parse(&[
                "--type",
                "story_assignment",
                "--stories",
                "S1=bob",
                "alice",
                "hello"
            ])
            .is_err()
        );
        let request = parse(&[
            "--type",
            "story_assignment",
            "--subject",
            "sprint-7",
            "--stories",
            "S1=alice",
            "alice",
            "Sprint",
            "stories",
        ])
        .unwrap();
        assert_eq!(
            request.message_type,
            Some(SemanticMessageType::StoryAssignment)
        );
        assert_eq!(request.subject.as_deref(), Some("sprint-7"));
        assert_eq!(request.stories.unwrap().stories[0].key.as_str(), "S1");
    }

    #[test]
    fn send_reached_truth_table_is_exhaustive() {
        for (outcome, expected) in [
            (RecipientOutcome::Delivered, true),
            (RecipientOutcome::AwaitingApproval, true),
            (RecipientOutcome::AcceptedWithoutItem, true),
            (RecipientOutcome::Declined, false),
            (
                RecipientOutcome::AskedQuestion {
                    task_id: "task".into(),
                },
                false,
            ),
            (RecipientOutcome::Unreachable { cause: None }, false),
            (RecipientOutcome::NoUsableAnswer, false),
            (RecipientOutcome::NotSent, false),
        ] {
            assert_eq!(reached(&outcome), expected, "{outcome:?}");
        }
    }
}
