//! RU-25 navigation, isolated from Chat and Global commands.

use super::model::{Binding, CommandId, Context, Key, bind};
use crate::adapters::tui::presentation::viewer::ViewerCmd;

const fn v(keys: &'static [Key], command: ViewerCmd, label: &'static str) -> Binding {
    bind(
        Context::InspectorBody,
        keys,
        CommandId::Viewer(command),
        label,
    )
    .local()
}

#[rustfmt::skip]
pub const ROWS: &[Binding] = &[
    v(&[Key::Char('j')], ViewerCmd::LineDown, "Scroll").hint().does("Scroll the inspector down one line"),
    v(&[Key::Down], ViewerCmd::LineDown, "").does("Scroll the inspector down one line"),
    v(&[Key::Char('k')], ViewerCmd::LineUp, "Scroll").hint().does("Scroll the inspector up one line"),
    v(&[Key::Up], ViewerCmd::LineUp, "").does("Scroll the inspector up one line"),
    v(&[Key::Ctrl('d')], ViewerCmd::HalfDown, "Half page").hint().does("Scroll the inspector down half a page"),
    v(&[Key::Ctrl('u')], ViewerCmd::HalfUp, "Half page").hint().does("Scroll the inspector up half a page"),
    v(&[Key::PageDown], ViewerCmd::PageDown, "Page").hint().does("Scroll the inspector down one page"),
    v(&[Key::PageUp], ViewerCmd::PageUp, "Page").hint().does("Scroll the inspector up one page"),
    v(&[Key::Char('g')], ViewerCmd::Top, "Top").hint().does("Jump to the first inspector row"),
    v(&[Key::Char('G')], ViewerCmd::Tail, "Bottom").hint().does("Jump to the last inspector row"),
];
