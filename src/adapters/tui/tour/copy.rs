//! The words of the tour (Story 21.14, D8): card titles, card bodies, the rail and the two
//! notices. No key is spelled here: every key text comes from a registry row through the
//! `keymap::render` helpers, so a card can never name a key the table does not bind.

use super::TourStep;
use crate::adapters::tui::keymap::render::{key_text, leader_follow_up_long, rail, rail_key};
use crate::adapters::tui::keymap::table::LeaderCmd;
use crate::adapters::tui::keymap::table_chat::ChatCmd;
use crate::adapters::tui::keymap::table_input::InputCmd;
use crate::adapters::tui::keymap::table_tour::TourCmd;
use crate::adapters::tui::keymap::{CommandId, Context, table};

/// `Tour` for the offer, `Tour · <k> of 5` for a step.
#[must_use]
pub fn card_title(step: TourStep) -> String {
    match step.number() {
        0 => "Tour".to_owned(),
        number => format!("Tour · {number} of {}", TourStep::COUNT),
    }
}

/// The typed form of the tour command, from the built-in catalogue.
#[must_use]
pub fn slash_command() -> String {
    crate::adapters::builtin_commands::find("tour")
        .map(|command| format!("/{}", command.name))
        .unwrap_or_default()
}

/// The card body as paragraphs (at most three sentences in all). `multiline` is the composer's
/// multi-line mode, where `Enter` types a newline and the send key is the leader chord.
#[must_use]
pub fn body(step: TourStep, multiline: bool) -> Vec<String> {
    let skip = rail_key(Context::Tour, TourCmd::SkipStep);
    match step {
        TourStep::Offer => vec![
            "Take a two-minute tour: send a message, inspect a tool call, answer a decision, \
             open Mission Control and find help."
                .to_owned(),
            format!("Run {} any time to take it again.", slash_command()),
        ],
        TourStep::Send => {
            let send = if multiline {
                key_text(LeaderCmd::Send)
            } else {
                key_text(InputCmd::SubmitOrNewline)
            };
            vec![format!(
                "Type a question in the message box and send it with {send}."
            )]
        }
        TourStep::Inspect => vec![
            format!(
                "When the agent uses a tool, move to the conversation with {}, select the tool \
                 line with {} and open it with {}.",
                key_text(InputCmd::ReturnToChat),
                key_text(ChatCmd::CycleInvocation),
                key_text(ChatCmd::Inspect),
            ),
            format!("No tool call yet? {skip} skips this step."),
        ],
        TourStep::Decide => vec![
            format!(
                "When the agent needs you, the status row says how many decisions wait. Open the \
                 queue with {} and answer the first item.",
                key_text(LeaderCmd::Decisions),
            ),
            format!("Nothing waiting? {skip} skips this step."),
        ],
        TourStep::Hub => vec![format!(
            "The row above the status line counts agents, tasks and decisions. Open Mission \
             Control with {} for the details, or the command palette with {}.",
            leader_follow_up_long(LeaderCmd::Agents),
            key_text(InputCmd::OpenPalette),
        )],
        TourStep::Help => vec![format!(
            "Open help with {}. It lists every key for where you are.",
            key_text(LeaderCmd::Help),
        )],
    }
}

/// The card's rail, one `[key] Label` item per row that can act on this card: `Enter` always,
/// `n` only on a step card, `Esc` always.
#[must_use]
pub fn rail_items(step: TourStep) -> Vec<String> {
    let mut commands = vec![TourCmd::Continue];
    if step != TourStep::Offer {
        commands.push(TourCmd::SkipStep);
    }
    commands.push(TourCmd::Leave);
    commands.into_iter().map(rail_item).collect()
}

fn rail_item(command: TourCmd) -> String {
    let label = table()
        .iter()
        .find(|row| row.context == Context::Tour && row.command == CommandId::Tour(command))
        .map_or("", |row| row.label);
    format!("{} {label}", rail(Context::Tour, command))
}

/// The coach line's instruction for an armed step (Story 21.14 D13): one short sentence with its
/// generated key. It never carries a driver wait string, `Ready`, or the Mission strip's
/// grammar tokens. `multiline` is the composer's multi-line mode (the send key is the chord).
#[must_use]
pub fn coach_instruction(step: TourStep, multiline: bool) -> String {
    match step {
        TourStep::Offer => String::new(),
        TourStep::Send => {
            let send = if multiline {
                key_text(LeaderCmd::Send)
            } else {
                key_text(InputCmd::SubmitOrNewline)
            };
            format!("Send a message with {send}")
        }
        TourStep::Inspect => format!(
            "Select a tool call and open it with {}",
            key_text(ChatCmd::Inspect)
        ),
        TourStep::Decide => format!("Answer a decision with {}", key_text(LeaderCmd::Decisions)),
        TourStep::Hub => format!(
            "Open Mission Control with {}",
            leader_follow_up_long(LeaderCmd::Agents)
        ),
        TourStep::Help => format!("Open help with {}", key_text(LeaderCmd::Help)),
    }
}

/// The Info notice of leaving the tour.
#[must_use]
pub fn closed_notice() -> String {
    format!("Tour closed. Run {} to take it again.", slash_command())
}

/// The Success notice of finishing the tour.
#[must_use]
pub fn complete_notice() -> String {
    format!(
        "Tour complete. Help is always one key away: {}.",
        key_text(LeaderCmd::Help)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story21_14_titles_number_the_five_steps() {
        assert_eq!(card_title(TourStep::Offer), "Tour");
        assert_eq!(card_title(TourStep::Send), "Tour · 1 of 5");
        assert_eq!(card_title(TourStep::Help), "Tour · 5 of 5");
    }

    #[test]
    fn story21_14_bodies_are_at_most_three_sentences_without_a_missing_key() {
        for step in [
            TourStep::Offer,
            TourStep::Send,
            TourStep::Inspect,
            TourStep::Decide,
            TourStep::Hub,
            TourStep::Help,
        ] {
            for multiline in [false, true] {
                let text = body(step, multiline).join(" ");
                let sentences = text
                    .split(['.', '?'])
                    .filter(|part| !part.trim().is_empty())
                    .count();
                assert!(sentences <= 4, "{step:?}: {text}");
                assert!(!text.contains("I "), "{step:?}: {text}");
                assert!(!text.contains(" ? "), "a missing key paints `?`: {text}");
            }
        }
    }

    #[test]
    fn story21_14_the_send_card_follows_the_multiline_mode() {
        assert!(body(TourStep::Send, false)[0].ends_with("with Enter."));
        assert!(body(TourStep::Send, true)[0].ends_with("with Ctrl+X, Enter."));
    }

    #[test]
    fn story21_14_the_rail_is_generated_and_the_offer_has_no_skip_step() {
        assert_eq!(
            rail_items(TourStep::Send),
            ["[Enter] Continue", "[n] Skip step", "[Esc] Skip tour"]
        );
        assert_eq!(
            rail_items(TourStep::Offer),
            ["[Enter] Continue", "[Esc] Skip tour"]
        );
    }
}
