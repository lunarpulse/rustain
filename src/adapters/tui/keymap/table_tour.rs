//! Tour card rows (Story 21.14, D3): the keys of the card the onboarding tour shows, and the
//! stack chords every frame declares. Every row is local: the attached host has no tour.
//!
//! The card never decides, sends or opens anything on its own: `Enter` arms the step the card
//! describes (or starts the tour from the offer), `n` skips a step, `Esc` and `Ctrl+C` leave the
//! tour. Whether `n` can act now (a step card, not the offer) is decided by the key executor and
//! the generated rail from the same fact, so the rail never paints a key that does nothing.

use super::model::{Binding, CommandId, Context, HelpCat, Key, bind};
use super::table::GlobalCmd;
use super::table_input::InputCmd;
use super::table_overlay::OverlayCmd;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TourCmd {
    /// `Enter`: begin the tour (the offer), or arm the step the card describes and close it.
    Continue,
    /// `n`: skip this step and show the next card (the last step finishes the tour).
    SkipStep,
    /// `Esc`, `Ctrl+C`: leave the tour for good.
    Leave,
}

const fn t(keys: &'static [Key], command: TourCmd, label: &'static str) -> Binding {
    bind(Context::Tour, keys, CommandId::Tour(command), label).local()
}

#[rustfmt::skip]
pub const ROWS: &[Binding] = &[
    t(&[Key::Enter], TourCmd::Continue, "Continue").help(HelpCat::General, "Enter (tour card)", "Start the tour, or begin the step the card describes"),
    t(&[Key::Char('n')], TourCmd::SkipStep, "Skip step").help(HelpCat::General, "n (tour card)", "Skip this tour step (not on the first offer card)"),
    t(&[Key::Esc], TourCmd::Leave, "Skip tour").help(HelpCat::General, "Esc (tour card)", "Leave the tour; /tour takes it again"),
    t(&[Key::Ctrl('c')], TourCmd::Leave, "").does("Leave the tour without cancelling work"),
    bind(Context::Tour, &[Key::Ctrl('x')], CommandId::Global(GlobalCmd::OpenLeader), "menu").local().does("Open the Ctrl+X menu above the tour card"),
    bind(Context::Tour, &[Key::Ctrl('p')], CommandId::Input(InputCmd::OpenPalette), "").local().does("Open the command palette above the tour card"),
    bind(Context::Tour, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "Quit").local().does("Ask to quit without closing the tour card"),
];
