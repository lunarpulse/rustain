//! K6 / A19: partial 4-A1 ratchet. Behaviour belongs to K2–K4, not this scanner.
//! Every retained occurrence has an exact named-site pin and a migration owner.
//! Removed/changed pins fail stale; new hits never inherit a file exemption.
use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const PRESENTATION: &str = "src/adapters/tui/presentation/";
const HOSTS: &[(&str, &str)] = &[
    ("src/infrastructure/runtime/event_loop.rs", "render"),
    ("src/infrastructure/runtime/attach_loop.rs", "run_attached"),
];
const TEST_FILES: &[&str] = &[
    "src/adapters/auth/flows/api_key/tests.rs",
    "src/adapters/daemon/approval_lifecycle_tests.rs",
    "src/adapters/daemon/server/turn_interrupt/tests.rs",
    "src/adapters/responses/tests.rs",
    "src/adapters/tui/auth_outcome_tests.rs",
    "src/adapters/tui/story21_2_slash_tests.rs",
    "src/infrastructure/runtime/state_extraction_tests.rs",
];
type Pin<'a> = (&'a str, &'a str, &'a [&'a str]);
fn re(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap()
}
fn compact(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn blank(bytes: &mut [u8]) {
    for b in bytes {
        if *b != b'\n' {
            *b = b' ';
        }
    }
}

// Lexical masking preserves byte offsets and physical lines. Nested comments, escaped
// strings, raw strings and char literals cannot invent a brace or a banned identifier.
fn code_only(source: &str) -> String {
    let mut b = source.as_bytes().to_vec();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i..].starts_with(b"/*") {
            i += 2;
            let mut depth = 1;
            while i < b.len() && depth > 0 {
                if b[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if b[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else if b[i] == b'r' && b.get(i + 1).is_some_and(|c| matches!(*c, b'#' | b'"')) {
            let mut j = i + 1;
            while b.get(j) == Some(&b'#') {
                j += 1;
            }
            if b.get(j) != Some(&b'"') {
                i += 1;
                continue;
            }
            let hashes = j - i - 1;
            i = j + 1;
            while i < b.len() {
                if b[i] == b'"'
                    && b.get(i + 1..i + 1 + hashes)
                        .is_some_and(|s| s.iter().all(|c| *c == b'#'))
                {
                    i += 1 + hashes;
                    break;
                }
                i += 1;
            }
        } else if b[i] == b'"' {
            i += 1;
            while i < b.len() {
                if b[i] == b'\\' {
                    i = (i + 2).min(b.len());
                } else if b[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
        } else if b[i] == b'\'' {
            // A lifetime is not a char. A char has exactly one scalar or escape.
            let rest = &source[i + 1..];
            let length = if rest.starts_with('\\') {
                2
            } else {
                rest.chars().next().map_or(0, char::len_utf8)
            };
            if b.get(i + 1 + length) != Some(&b'\'') {
                i += 1;
                continue;
            }
            i += length + 2;
        } else {
            i += 1;
            continue;
        }
        blank(&mut b[start..i]);
    }
    String::from_utf8(b).unwrap()
}
fn item_end(code: &str, start: usize) -> usize {
    let mut depth = 0;
    for (off, c) in code[start..].char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 && c == '}' {
                    return start + off + 1;
                }
            }
            ';' if depth == 0 => return start + off + 1,
            _ => {}
        }
    }
    panic!("unterminated source item at byte {start}")
}
fn production_part(source: &str) -> String {
    let mut code = code_only(source);
    static CFG: LazyLock<Regex> = LazyLock::new(|| re(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]"));
    let mut cursor = 0;
    while let Some(m) = CFG.find_at(&code, cursor) {
        let (start, mut at) = (m.start(), m.end());
        loop {
            at += code[at..].len() - code[at..].trim_start().len();
            if !code[at..].starts_with("#[") {
                break;
            }
            let mut depth = 0;
            for (off, c) in code[at + 1..].char_indices() {
                if c == '[' {
                    depth += 1;
                }
                if c == ']' {
                    depth -= 1;
                    if depth == 0 {
                        at += off + 2;
                        break;
                    }
                }
            }
        }
        cursor = item_end(&code, at);
        let mut bytes = code.into_bytes();
        blank(&mut bytes[start..cursor]);
        code = String::from_utf8(bytes).unwrap();
    }
    code
}
fn scope(code: &str, at: usize) -> String {
    static NAMES: LazyLock<Regex> = LazyLock::new(|| re(r"\b(fn|struct|enum)\s+(\w+)"));
    static IMPL: LazyLock<Regex> = LazyLock::new(|| re(r"\bimpl\b[^{};]*\{"));
    let Some(m) = NAMES.captures_iter(&code[..at]).last() else {
        return "<module>".into();
    };
    let mut name = m[2].to_owned();
    if &m[1] == "fn" {
        if let Some(im) = IMPL.find_iter(&code[..m.get(0).unwrap().start()]).last() {
            let mut depth = 1;
            for c in code[im.end()..m.get(0).unwrap().start()].chars() {
                if c == '{' {
                    depth += 1;
                } else if c == '}' {
                    depth -= 1;
                }
                if depth == 0 {
                    break;
                }
            }
            if depth > 0 {
                name = format!("{}::{name}", compact(im.as_str().trim_end_matches('{')));
            }
        }
    }
    name
}

// Exact named-site pins from da41ddae, minus A1 cutovers; whitespace is not ownership.
#[rustfmt::skip]
const PINS: &[Pin<'_>] = &[
    ("src/adapters/tui/agent_inspector_keys.rs", "4-C", &[
        r###"focus-write|on_key|state.focus = FocusState::Chat;"###,
    ]),
    ("src/adapters/tui/app.rs", "4-C", &[
        r###"focus-write|advance_deferred_permission|state.focus = FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Permission));"###, r###"focus-write|advance_deferred_permission|state.focus = if state.recipient_surface.is_some() {"###, r###"focus-write|handle_leader_key|state.focus = origin.clone();"###, r###"focus-write|handle_leader_key|state.focus = FocusState::Overlay(OverlayType::Help);"###, r###"focus-write|dispatch_input|state.focus = FocusState::Chat;"###, r###"focus-write|open_leader|state.focus = FocusState::Overlay(OverlayType::WhichKey);"###,
        r###"focus-write|open_palette|state.focus = FocusState::Overlay(OverlayType::CommandPalette);"###, r###"focus-write|card_special_key|state.focus ="###, r###"focus-write|card_special_key|state.focus = FocusState::Input;"###, r###"focus-write|exec_input|state.focus = FocusState::Chat;"###, r###"focus-write|exec_input|state.focus = FocusState::Overlay(OverlayType::ReverseSearch);"###, r###"focus-write|exec_input|state.focus = FocusState::Overlay(OverlayType::Search);"###,
        r###"focus-write|exec_chat|state.focus = FocusState::Input;"###, r###"focus-write|exec_chat|state.focus = FocusState::Overlay(OverlayType::Help);"###, r###"focus-write|exec_sidebar|state.focus = FocusState::Chat;"###, r###"focus-write|exec_sidebar|state.focus = FocusState::Input;"###, r###"focus-write|exec_sidebar|state.focus = FocusState::Overlay(OverlayType::CrossSearch);"###, r###"focus-write|sidebar_move|state.focus = FocusState::Sidebar {"###,
        r###"focus-write|exec_panel|state.focus = FocusState::Sidebar {"###, r###"focus-write|exec_panel|state.focus = FocusState::Overlay(OverlayType::WaveOverlay);"###, r###"focus-write|exec_panel|state.focus = FocusState::Chat;"###, r###"focus-write|exec_recipient|state.focus = surface.prior_focus;"###, r###"focus-write|exec_help|OverlayCmd::HelpClose => state.focus = state.help_overlay.close(),"###,
        r###"focus-write|exec_profile|state.focus = prev;"###, r###"focus-write|exec_model_selector|state.focus = state.model_selector.dismiss().unwrap_or(FocusState::Input);"###, r###"focus-write|handle_char|state.focus = FocusState::Input;"###, r###"focus-write|exec_palette|state.focus = focus;"###, r###"focus-write|exec_palette|state.focus = state.command_palette.dismiss().unwrap_or(FocusState::Input);"###,
        r###"focus-write|exec_usage|state.focus = state.usage_panel.dismiss().unwrap_or(FocusState::Input);"###, r###"focus-write|dispatch_palette_action|state.focus = FocusState::Input;"###, r###"focus-write|exec_reverse_search_keys|state.focus = FocusState::Input;"###, r###"focus-write|exec_reverse_search_keys|state.focus = FocusState::Input;"###,
    ]),
    ("src/adapters/tui/app.rs", "4-A2", &[
        r###"prior|open_leader|let prior_focus = match &state.focus {"###, r###"prior|open_leader|state.which_key.open(prior_focus);"###, r###"prior|open_palette|let prior_focus = match &state.focus {"###, r###"prior|open_palette|state.command_palette.open(prior_focus);"###, r###"overlay|ctrl_x_opens_leader|OverlayType::CommandPalette"###, r###"overlay|ctrl_x_opens_leader|| OverlayType::WhichKey"###,
        r###"overlay|ctrl_x_opens_leader|| OverlayType::Help"###, r###"overlay|handle_leader_key|state.focus = FocusState::Overlay(OverlayType::Help);"###, r###"overlay|overlay_focus_context|OverlayType::Help => Context::Help,"###, r###"overlay|open_leader|FocusState::Overlay(OverlayType::ReverseSearch | OverlayType::Autocomplete(_)) => {"###, r###"overlay|open_leader|FocusState::Overlay(OverlayType::ReverseSearch | OverlayType::Autocomplete(_)) => {"###, r###"overlay|open_leader|state.focus = FocusState::Overlay(OverlayType::WhichKey);"###,
        r###"overlay|open_palette|FocusState::Overlay(OverlayType::ReverseSearch | OverlayType::Autocomplete(_)) => {"###, r###"overlay|open_palette|FocusState::Overlay(OverlayType::ReverseSearch | OverlayType::Autocomplete(_)) => {"###, r###"overlay|open_palette|state.focus = FocusState::Overlay(OverlayType::CommandPalette);"###, r###"overlay|exec_chat|state.focus = FocusState::Overlay(OverlayType::Help);"###,
    ]),
    ("src/adapters/tui/app.rs", "4-B", &[
        r###"overlay|overlay_focus_context|OverlayType::Login => Context::Login,"###,
        r###"prior|exec_input|state.search_state.prior_focus = Some(prior);"###, r###"overlay|handle_input_inner|if state.focus == FocusState::Overlay(OverlayType::Login)"###, r###"overlay|ctrl_x_opens_leader|| OverlayType::ModelSelector"###, r###"overlay|ctrl_x_opens_leader|| OverlayType::ProfileSwitcher"###, r###"overlay|ctrl_x_opens_leader|| OverlayType::Search"###, r###"overlay|ctrl_x_opens_leader|| OverlayType::CrossSearch"###,
        r###"overlay|ctrl_x_opens_leader|| OverlayType::BookmarkList"###, r###"overlay|ctrl_x_opens_leader|| OverlayType::UsagePanel"###, r###"overlay|ctrl_x_opens_leader|| OverlayType::WaveOverlay"###, r###"overlay|ctrl_x_opens_leader|| OverlayType::Login"###, r###"overlay|overlay_focus_context|OverlayType::ModelSelector => Context::ModelSelector,"###, r###"overlay|overlay_focus_context|OverlayType::UsagePanel => Context::UsagePanel,"###,
        r###"overlay|overlay_focus_context|OverlayType::ProfileSwitcher => Context::ProfileSwitcher,"###, r###"overlay|overlay_focus_context|OverlayType::Search => match state.search_state.substate {"###, r###"overlay|overlay_focus_context|OverlayType::CrossSearch => Context::CrossSearch,"###, r###"overlay|overlay_focus_context|OverlayType::BookmarkList => Context::BookmarkList,"###, r###"overlay|overlay_focus_context|OverlayType::WaveOverlay => Context::WaveOverlay,"###, r###"overlay|exec_input|state.focus = FocusState::Overlay(OverlayType::ReverseSearch);"###,
        r###"overlay|exec_input|state.focus = FocusState::Overlay(OverlayType::Search);"###, r###"overlay|exec_sidebar|state.focus = FocusState::Overlay(OverlayType::CrossSearch);"###, r###"overlay|exec_panel|state.focus = FocusState::Overlay(OverlayType::WaveOverlay);"###,
    ]),
    ("src/adapters/tui/app.rs", "21-8", &[
        r###"prior|exec_recipient|state.focus = surface.prior_focus;"###,
    ]),
    ("src/adapters/tui/decision_pane.rs", "4-C", &[
        r###"focus-write|settle_borrowed_card_focus|state.focus = state.card_focus_borrowed_from.take().unwrap();"###, r###"focus-borrow|take_chat_focus_for_local_card|&mut state.focus,"###,
    ]),
    ("src/adapters/tui/handlers/accounts.rs", "4-C", &[
        r###"focus-write|command|state.focus = LOGIN;"###, r###"focus-write|command|state.focus = LOGIN;"###,
    ]),
    ("src/adapters/tui/handlers/artifact_command.rs", "4-C", &[
        r###"focus-write|resolve_apply_card|state.focus = card.prior_focus.clone();"###,
    ]),
    ("src/adapters/tui/handlers/artifact_command.rs", "21-6", &[
        r###"prior|resolve_apply_card|state.focus = card.prior_focus.clone();"###,
    ]),
    ("src/adapters/tui/handlers/bookmark.rs", "4-C", &[
        r###"focus-write|handle_apply_open_bookmark_list|state.focus = FocusState::Overlay(crate::domain::models::visual::OverlayType::BookmarkList);"###, r###"focus-write|handle_apply_jump_bookmark|state.focus = FocusState::Chat;"###, r###"focus-write|handle_apply_jump_bookmark|state.focus = FocusState::Chat;"###, r###"focus-write|handle_apply_jump_bookmark|state.focus = FocusState::Chat;"###, r###"focus-write|handle_apply_delete_bookmark|state.focus = FocusState::Chat;"###,
    ]),
    ("src/adapters/tui/handlers/bookmark.rs", "4-B", &[
        r###"overlay|handle_apply_open_bookmark_list|state.focus = FocusState::Overlay(crate::domain::models::visual::OverlayType::BookmarkList);"###,
    ]),
    ("src/adapters/tui/handlers/export.rs", "4-C", &[
        r###"focus-write|apply_confirm_export_overwrite|state.focus = FocusState::Chat;"###, r###"focus-write|apply_cancel_export_overwrite|state.focus = FocusState::Chat;"###, r###"focus-write|apply_export_command|state.focus ="###,
    ]),
    ("src/adapters/tui/handlers/login.rs", "4-C", &[
        r###"focus-write|take_focus|state.focus = state.model_selector.dismiss().unwrap_or(FocusState::Input);"###, r###"focus-write|open|state.focus = LOGIN;"###, r###"focus-write|open|state.focus = LOGIN;"###, r###"focus-write|begin|state.focus = LOGIN;"###, r###"focus-write|close|state.focus = previous.unwrap_or(FocusState::Input);"###,
    ]),
    ("src/adapters/tui/handlers/login.rs", "4-B", &[
        r###"previous|begin|state.login.previous_focus.get_or_insert(previous);"###, r###"overlay|display_text|pub(crate) const LOGIN: FocusState = FocusState::Overlay(OverlayType::Login);"###, r###"overlay|take_focus|if state.focus == FocusState::Overlay(OverlayType::ModelSelector) {"###,
    ]),
    ("src/adapters/tui/handlers/model_switch.rs", "4-C", &[
        r###"focus-write|open_selector|state.focus = crate::domain::models::FocusState::Overlay("###, r###"focus-write|handle_apply_model_switch|state.focus = crate::domain::models::FocusState::Overlay("###, r###"focus-write|handle_complete_model_switch|state.focus = state"###,
    ]),
    ("src/adapters/tui/handlers/model_switch.rs", "4-B", &[
        r###"overlay|open_selector|crate::domain::models::visual::OverlayType::ModelSelector,"###, r###"overlay|handle_apply_model_switch|crate::domain::models::visual::OverlayType::ModelSelector,"###,
    ]),
    ("src/adapters/tui/handlers/notice.rs", "4-C", &[
        r###"focus-write|apply_density_transition|state.focus = crate::domain::models::FocusState::Sidebar {"###, r###"focus-write|apply_density_transition|state.focus = crate::domain::models::FocusState::Chat;"###,
    ]),
    ("src/adapters/tui/handlers/peer_command.rs", "4-C", &[
        r###"focus-write|resolve_peer_add_card|state.focus = pending.prior_focus.clone();"###,
    ]),
    ("src/adapters/tui/handlers/peer_command.rs", "21-6", &[
        r###"prior|resolve_peer_add_card|state.focus = pending.prior_focus.clone();"###,
    ]),
    ("src/adapters/tui/handlers/team_command.rs", "4-C", &[
        r###"focus-write|open_team_retract_card|state.focus = crate::domain::models::FocusState::Overlay(OverlayType::Confirmation("###, r###"focus-write|resolve_team_retract_card|state.focus = pending.prior_focus.clone();"###,
    ]),
    ("src/adapters/tui/handlers/team_command.rs", "21-6", &[
        r###"prior|open_team_retract_card|prior_focus: state.focus.clone(),"###, r###"prior|resolve_team_retract_card|state.focus = pending.prior_focus.clone();"###,
    ]),
    ("src/adapters/tui/handlers/transparency.rs", "4-C", &[
        r###"focus-borrow|synchronize_sidebar_selection|} = &mut state.focus"###,
    ]),
    ("src/adapters/tui/handlers/usage_panel.rs", "4-C", &[
        r###"focus-write|handle_open_usage_panel|state.focus = crate::domain::models::FocusState::Overlay("###,
    ]),
    ("src/adapters/tui/handlers/usage_panel.rs", "4-B", &[
        r###"overlay|handle_open_usage_panel|crate::domain::models::visual::OverlayType::UsagePanel,"###,
    ]),
    ("src/adapters/tui/hints.rs", "4-A2", &[
        r###"overlay|contextual_hint|FocusState::Overlay(OverlayType::WhichKey) => {"###, r###"overlay|contextual_hint|FocusState::Overlay(OverlayType::CommandPalette) => Some(render_tips::palette_tip()),"###,
    ]),
    ("src/adapters/tui/layout.rs", "4-B", &[
        r###"reservation|AppLayout|pub search_bar: Option<Rect>,"###, r###"reservation|AppLayout|pub bookmark_panel: Option<Rect>,"###, r###"reservation|AppLayout|pub fn reserve_search_bar(&mut self, active: bool) {"###, r###"reservation|impl AppLayout::reserve_search_bar|self.search_bar = Some(bar);"###, r###"reservation|impl AppLayout::reserve_search_bar|pub fn reserve_bookmark_panel(&mut self, requested_height: u16) {"###, r###"reservation|impl AppLayout::reserve_bookmark_panel|self.bookmark_panel = Some(panel);"###,
        r###"reservation|compute_layout|search_bar: None,"###, r###"reservation|compute_layout|bookmark_panel: None,"###, r###"reservation|compute_layout|search_bar: None,"###, r###"reservation|compute_layout|bookmark_panel: None,"###, r###"reservation|compute_layout|search_bar: None,"###, r###"reservation|compute_layout|bookmark_panel: None,"###,
        r###"reservation|compute_layout|search_bar: None,"###, r###"reservation|compute_layout|bookmark_panel: None,"###, r###"reservation|compute_layout|search_bar: None,"###, r###"reservation|compute_layout|bookmark_panel: None,"###,
    ]),
    ("src/adapters/tui/state.rs", "4-B", &[
        r###"previous|ModelSelectorState|pub previous_focus: Option<FocusState>,"###, r###"previous|impl ModelSelectorState::new|previous_focus: None,"###, r###"previous|impl ModelSelectorState::open|self.previous_focus = Some(current_focus);"###, r###"previous|impl ModelSelectorState::dismiss|self.previous_focus.take()"###, r###"previous|ProfileSwitcherState|pub previous_focus: Option<FocusState>,"###, r###"previous|impl ProfileSwitcherState::new|previous_focus: None,"###,
        r###"previous|impl ProfileSwitcherState::open|self.previous_focus = Some(current_focus);"###, r###"previous|impl ProfileSwitcherState::dismiss|self.previous_focus.take()"###, r###"previous|UsagePanelState|pub previous_focus: Option<FocusState>,"###, r###"previous|impl UsagePanelState::new|previous_focus: None,"###, r###"previous|impl UsagePanelState::open|self.previous_focus = Some(current_focus);"###, r###"previous|impl UsagePanelState::dismiss|self.previous_focus.take()"###,
        r###"prior|SearchState|pub prior_focus: Option<FocusState>,"###, r###"prior|impl SearchState::new|prior_focus: None,"###, r###"active|ReverseSearchState|pub active: bool,"###, r###"active|SearchState|pub active: bool,"###, r###"active|CrossSearchState|pub active: bool,"###, r###"active|ModelSelectorState|pub active: bool,"###,
        r###"active|ProfileSwitcherState|pub active: bool,"###, r###"active|UsagePanelState|pub active: bool,"###,
    ]),
    ("src/adapters/tui/state.rs", "4-A2", &[
        r###"previous|CommandPaletteState|pub previous_focus: Option<FocusState>,"###, r###"previous|impl CommandPaletteState::new|previous_focus: None,"###, r###"previous|impl CommandPaletteState::open|self.previous_focus = Some(current_focus);"###, r###"previous|impl CommandPaletteState::dismiss|self.previous_focus.take()"###, r###"previous|WhichKeyState|pub previous_focus: Option<FocusState>,"###, r###"previous|impl WhichKeyState::new|previous_focus: None,"###,
        r###"previous|impl WhichKeyState::open|self.previous_focus = Some(current_focus);"###, r###"previous|impl WhichKeyState::dismiss|self.previous_focus.take()"###, r###"prior|HelpOverlayState|pub prior_focus: FocusState,"###, r###"prior|impl HelpOverlayState::new|prior_focus: FocusState::Input,"###, r###"prior|impl HelpOverlayState::open|self.prior_focus = prior;"###, r###"prior|impl HelpOverlayState::close|self.prior_focus.clone()"###,
        r###"active|AutocompleteState|pub active: bool,"###, r###"active|CommandPaletteState|pub active: bool,"###, r###"active|HelpOverlayState|pub active: bool,"###, r###"active|WhichKeyState|pub active: bool,"###,
    ]),
    ("src/adapters/tui/state.rs", "21-6", &[
        r###"prior|PendingArtifactCard|pub prior_focus: crate::domain::models::FocusState,"###, r###"prior|PendingPeerAdd|pub prior_focus: crate::domain::models::FocusState,"###, r###"prior|PendingTeamRetract|pub prior_focus: crate::domain::models::FocusState,"###,
    ]),
    ("src/adapters/tui/state.rs", "21-8", &[
        r###"prior|RecipientSurface|pub prior_focus: FocusState,"###,
    ]),
    ("src/adapters/tui/state/login_overlay.rs", "4-B", &[
        r###"previous|LoginOverlayState|pub previous_focus: Option<FocusState>,"###, r###"previous|impl Default for LoginOverlayState::default|previous_focus: None,"###, r###"previous|impl LoginOverlayState::open_picker|self.previous_focus.get_or_insert(previous);"###, r###"previous|impl LoginOverlayState::open_accounts|self.previous_focus.get_or_insert(previous);"###, r###"previous|impl LoginOverlayState::open_methods|self.previous_focus.get_or_insert(previous);"###, r###"previous|impl LoginOverlayState::close|self.previous_focus.take()"###,
        r###"active|LoginOverlayState|pub active: bool,"###,
    ]),
    ("src/adapters/tui/widgets/mod.rs", "4-B", &[
        r###"reservation|<module>|pub mod search_bar;"###,
    ]),
    ("src/domain/models/visual.rs", "4-A2", &[
        r###"variant|OverlayType|CommandPalette,"###, r###"variant|OverlayType|WhichKey,"###, r###"variant|OverlayType|Help,"###, r###"variant|OverlayType|Autocomplete(super::autocomplete::AutocompleteKind),"###,
    ]),
    ("src/domain/models/visual.rs", "4-B", &[
        r###"variant|OverlayType|ModelSelector,"###, r###"variant|OverlayType|ProfileSwitcher,"###, r###"variant|OverlayType|ReverseSearch,"###, r###"variant|OverlayType|Search,"###, r###"variant|OverlayType|CrossSearch,"###, r###"variant|OverlayType|BookmarkList,"###,
        r###"variant|OverlayType|UsagePanel,"###, r###"variant|OverlayType|WaveOverlay,"###, r###"variant|OverlayType|Login,"###,
    ]),
    ("src/infrastructure/runtime/approval_bridge.rs", "4-C", &[
        r###"focus-write|apply|state.focus ="###, r###"focus-write|handle_action|state.focus = FocusState::Overlay(OverlayType::Confirmation("###, r###"focus-write|handle_action|state.focus ="###,
    ]),
    ("src/infrastructure/runtime/artifact_bridge.rs", "4-C", &[
        r###"focus-write|activate_artifacts_sidebar|state.focus = crate::domain::models::FocusState::Sidebar {"###, r###"focus-write|finalize_artifacts_panel|state.focus = crate::domain::models::FocusState::Sidebar {"###, r###"focus-write|open_apply_card|state.focus = crate::domain::models::FocusState::Overlay("###, r###"focus-write|open_resolve_card|state.focus = crate::domain::models::FocusState::Overlay("###,
    ]),
    ("src/infrastructure/runtime/artifact_bridge.rs", "21-6", &[
        r###"prior|open_apply_card|let prior_focus = state.focus.clone();"###, r###"prior|open_apply_card|prior_focus,"###, r###"prior|open_resolve_card|let prior_focus = state.focus.clone();"###, r###"prior|open_resolve_card|prior_focus,"###,
    ]),
    ("src/infrastructure/runtime/attach_keys.rs", "4-C", &[
        r###"focus-write|execute_attached|ctx.state.focus = FocusState::Overlay(OverlayType::WhichKey);"###, r###"focus-write|execute_attached|ctx.state.focus = FocusState::Input;"###, r###"focus-write|execute_attached|ctx.state.focus = FocusState::Overlay(OverlayType::Help);"###,
        r###"focus-write|execute_attached|FocusState::Input => ctx.state.focus = FocusState::Chat,"###, r###"focus-write|execute_attached|FocusState::Chat => ctx.state.focus = FocusState::Input,"###, r###"focus-write|execute_attached|ctx.state.focus = ctx.state.help_overlay.close();"###, r###"focus-write|execute_attached|ctx.state.focus = ctx"###, r###"focus-write|leader_key|ctx.state.focus = origin.clone();"###, r###"focus-write|leader_key|ctx.state.focus = FocusState::Overlay(OverlayType::Help);"###,
        r###"focus-write|open_palette|ctx.state.focus = FocusState::Overlay(OverlayType::CommandPalette);"###, r###"focus-write|begin_retract|ctx.state.focus = FocusState::Chat;"###, r###"focus-borrow|begin_retract|&mut ctx.state.focus,"###, r###"attached|attached_card_context|fn with_lent_input<R>("###, r###"attached|execute_attached|with_lent_input(ctx.state, ctx.input, |state| {"###, r###"attached|execute_attached|with_lent_input(ctx.state, ctx.input, cancel_autocomplete_popup);"###,
        r###"attached|popup_delegate|with_lent_input(ctx.state, ctx.input, |state| {"###, r###"attached|palette_delegate|let action = with_lent_input(ctx.state, ctx.input, |state| handle_input(state, &event));"###,
    ]),
    ("src/infrastructure/runtime/attach_keys.rs", "4-A2", &[
        r###"overlay|execute_attached|ctx.state.focus = FocusState::Overlay(OverlayType::WhichKey);"###, r###"overlay|execute_attached|ctx.state.focus = FocusState::Overlay(OverlayType::Help);"###, r###"overlay|leader_key|ctx.state.focus = FocusState::Overlay(OverlayType::Help);"###, r###"overlay|open_palette|ctx.state.focus = FocusState::Overlay(OverlayType::CommandPalette);"###, r###"attached|log_facts|pub(super) fn paint_overlays("###, r###"render|paint_overlays|help_overlay::render("###,
    ]),
    ("src/infrastructure/runtime/attach_loop.rs", "4-C", &[
        r###"focus-write|open_response_surface|state.focus = FocusState::Chat;"###, r###"focus-write|recipient_surface_key|state.focus = surface.prior_focus;"###, r###"focus-write|recipient_surface_key|state.focus = FocusState::Input;"###, r###"focus-write|attached_proposal_key|state.focus = surface.prior_focus;"###, r###"focus-write|run_attached|state.focus = FocusState::Chat;"###, r###"focus-borrow|open_response_surface|None => (std::mem::replace(&mut state.focus, FocusState::Chat), None),"###,
        r###"focus-borrow|attached_card_arrived|&mut state.focus,"###, r###"focus-mem|open_response_surface|None => (std::mem::replace(&mut state.focus, FocusState::Chat), None),"###, r###"attached|apply_attached_consolidation_frame|fn render_attached_composer("###, r###"attached|run_attached|let mut scroll_offset = 0usize;"###, r###"attached|run_attached|let mut auto_scroll = true;"###, r###"attached|run_attached|let mut pending_consolidation_card: Option<"###,
        r###"attached|run_attached|let mut retract_confirm: Option<(String, String)> = None;"###, r###"attached|run_attached|let mut pending_consent_approvals = std::collections::VecDeque::<PendingConsentApproval>::new();"###, r###"attached|run_attached|let mut approvals = AttachedApprovals::default();"###, r###"attached|run_attached|render_attached_composer("###,
    ]),
    ("src/infrastructure/runtime/attach_loop.rs", "21-8", &[
        r###"prior|open_response_surface|let (prior_focus, root_id) = match state.recipient_surface.take() {"###, r###"prior|open_response_surface|surface.prior_focus,"###, r###"prior|open_response_surface|prior_focus,"###, r###"prior|recipient_surface_key|state.focus = surface.prior_focus;"###, r###"prior|attached_proposal_key|state.focus = surface.prior_focus;"###,
    ]),
    ("src/infrastructure/runtime/attach_loop.rs", "4-A2", &[
        r###"attached|run_attached|attach_keys::paint_overlays(f, area, layout.input_area, layout.status_bar, &state)"###,
    ]),
    ("src/infrastructure/runtime/event_loop.rs", "4-C", &[
        r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Input;"###, r###"focus-write|run|state.focus = FocusState::Input;"###, r###"focus-write|run|state.focus = FocusState::Input;"###, r###"focus-write|run|state.focus = FocusState::Input;"###, r###"focus-write|run|state.focus = FocusState::Input;"###,
        r###"focus-write|run|state.focus = FocusState::Overlay(OverlayType::Confirmation("###, r###"focus-write|run|state.focus = FocusState::Input;"###, r###"focus-write|run|state.focus = FocusState::Input;"###, r###"focus-write|run|state.focus = FocusState::Input;"###, r###"focus-write|run|state.focus = FocusState::Input;"###, r###"focus-write|run|state.focus = handlers::login::LOGIN;"###,
        r###"focus-write|run|state.focus = FocusState::Input;"###, r###"focus-write|run|state.focus = FocusState::Input;"###, r###"focus-write|run|state.focus = FocusState::Input;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###,
        r###"focus-write|run|state.focus = FocusState::Input;"###, r###"focus-write|run|state.focus = if state.sidebar_visible {"###, r###"focus-write|run|state.focus = FocusState::Overlay("###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###,
        r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Overlay("###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###,
        r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###,
        r###"focus-write|run|state.focus = prior;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Sidebar {"###, r###"focus-write|run|state.focus = FocusState::Sidebar {"###, r###"focus-write|run|state.focus = handlers::login::LOGIN;"###, r###"focus-write|run|state.focus = crate::domain::models::FocusState::Overlay("###,
        r###"focus-write|run|state.focus = crate::domain::models::FocusState::Overlay("###, r###"focus-write|run|state.focus = FocusState::Overlay(OverlayType::WaveOverlay);"###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Overlay(OverlayType::Confirmation("###,
        r###"focus-write|run|state.focus = FocusState::Input;"###, r###"focus-write|run|state.focus = FocusState::Input;"###, r###"focus-write|run|state.focus = FocusState::Overlay(OverlayType::Confirmation("###, r###"focus-write|run|state.focus = FocusState::Overlay(OverlayType::Confirmation("###, r###"focus-write|run|state.focus = FocusState::Overlay(OverlayType::Confirmation("###, r###"focus-write|run|state.focus = FocusState::Overlay(OverlayType::Confirmation("###,
        r###"focus-write|run|state.focus = prev;"###, r###"focus-write|run|state.focus = prev;"###, r###"focus-write|apply_open_cross_search_result|state.focus = FocusState::Chat;"###, r###"focus-write|restore_focus_after_modal|state.focus = if state.pending_feedback_input.is_some() {"###, r###"focus-write|surface_deferred_modal|state.focus ="###, r###"focus-write|advance_skill_trust_queue|state.focus = FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::SkillTrust));"###,
    ]),
    ("src/infrastructure/runtime/event_loop.rs", "4-B", &[
        r###"prior|run|.prior_focus"###, r###"reservation|render|app_layout.reserve_search_bar(search_state.active);"###, r###"reservation|render|app_layout.reserve_bookmark_panel(requested_height);"###, r###"reservation|render|if let Some(search_bar_area) = app_layout.search_bar {"###, r###"reservation|render|crate::adapters::tui::widgets::search_bar::render("###, r###"reservation|render|if let Some(bookmark_panel_area) = app_layout.bookmark_panel {"###,
        r###"overlay|run|crate::domain::models::visual::OverlayType::ProfileSwitcher,"###, r###"overlay|run|crate::domain::models::visual::OverlayType::ProfileSwitcher,"###, r###"overlay|run|state.focus = FocusState::Overlay(OverlayType::WaveOverlay);"###, r###"overlay|run|crate::domain::models::visual::OverlayType::ProfileSwitcher,"###, r###"overlay|run|crate::domain::models::visual::OverlayType::ProfileSwitcher,"###, r###"overlay|render|crate::domain::models::visual::OverlayType::BookmarkList,"###,
        r###"overlay|render|} else if state.focus == FocusState::Overlay(OverlayType::WaveOverlay) {"###, r###"render|render|let lines = crate::adapters::tui::widgets::wave_overlay::render_wave_overlay("###, r###"render|render|crate::adapters::tui::widgets::search_bar::render(frame, search_bar_area, search_state, theme, state.modal_stack.is_empty());"###, r###"render|render|crate::adapters::tui::widgets::cross_search::render(frame, sidebar_area, &state.cross_search, theme, state.modal_stack.is_empty());"###, r###"render|render|crate::adapters::tui::widgets::bookmark_list::render("###, r###"render|render|model_selector::render(frame, area, model_selector, theme);"###,
        r###"render|render|crate::adapters::tui::widgets::login_dialog::render("###, r###"render|render|usage_panel::render(frame, area, usage_panel, theme);"###, r###"render|render|profile_switcher::render(frame, area, profile_switcher, theme);"###,
        r###"render|render|reverse_search::render(frame, app_layout.input_area, reverse_search, theme, state.modal_stack.is_empty());"###,
    ]),
    ("src/infrastructure/runtime/event_loop.rs", "4-A2", &[
        r###"render|render|help_overlay::render("###, r###"render|render|decision_painted = crate::adapters::tui::decision_pane::render(frame, app_layout.chat_pane, &state.decisions, &log_facts, theme);"###,
    ]),
    ("src/infrastructure/runtime/panel_bridge.rs", "4-C", &[
        r###"focus-write|open_panel|state.focus = FocusState::Sidebar {"###, r###"focus-write|open_panel|state.focus = FocusState::Chat;"###, r###"focus-write|open_panel|state.focus = FocusState::Chat;"###, r###"focus-write|open_panel|state.focus = FocusState::Sidebar {"###,
    ]),
    ("src/infrastructure/runtime/peer_bridge.rs", "4-C", &[
        r###"focus-write|raise_peer_add_card|state.focus = crate::domain::models::FocusState::Overlay("###,
    ]),
    ("src/infrastructure/runtime/peer_bridge.rs", "21-6", &[
        r###"prior|raise_peer_add_card|prior_focus: state.focus.clone(),"###,
    ]),
    ("src/infrastructure/runtime/recipient_surface.rs", "4-C", &[
        r###"focus-write|open_received_surface|state.focus = FocusState::Chat;"###, r###"focus-borrow|open_received_surface|std::mem::replace(&mut state.focus, FocusState::Chat),"###, r###"focus-mem|open_received_surface|std::mem::replace(&mut state.focus, FocusState::Chat),"###,
    ]),
    ("src/infrastructure/runtime/recipient_surface.rs", "21-8", &[
        r###"prior|open_received_surface|let (prior_focus, results) = match old {"###, r###"prior|open_received_surface|surface.prior_focus,"###, r###"prior|open_received_surface|prior_focus,"###,
    ]),
    ("src/infrastructure/runtime/room_bridge.rs", "4-C", &[
        r###"focus-write|activate_room_sidebar|state.focus = crate::domain::models::FocusState::Sidebar {"###, r###"focus-write|finalize_room_panel|state.focus = crate::domain::models::FocusState::Sidebar {"###,
    ]),
    ("src/infrastructure/runtime/team_proposal_bridge.rs", "4-C", &[
        r###"focus-write|proposal_action|state.focus = crate::domain::models::FocusState::Chat;"###,
    ]),
    ("src/infrastructure/runtime/transparency_bridge.rs", "21-6", &[
        r###"prior|retract_card|prior_focus: crate::domain::models::FocusState::Chat,"###,
    ]),
];

const RULES: &[(&str, &str)] = &[
    ("focus-write", r###"\.\s*focus\s*="###),
    ("focus-borrow", r###"&mut\s+[\w.]*\.focus\b"###),
    (
        "focus-mem",
        r###"mem\s*::\s*(?:replace|swap|take)\s*\(\s*&mut\s+[\w.]*\.focus\b"###,
    ),
    ("previous", r###"\bprevious_focus\b"###),
    ("prior", r###"\bprior_focus\b"###),
    (
        "reservation",
        r###"\b(?:reserve_search_bar|reserve_bookmark_panel|search_bar|bookmark_panel)\b"###,
    ),
    (
        "overlay",
        r###"OverlayType\s*::\s*(?:CommandPalette|ModelSelector|ProfileSwitcher|WhichKey|Help|ReverseSearch|Search|CrossSearch|BookmarkList|UsagePanel|Autocomplete|WaveOverlay|Login)\b"###,
    ),
    (
        "variant",
        r###"(?m)^\s*(?:CommandPalette|ModelSelector|ProfileSwitcher|WhichKey|Help|ReverseSearch|Search|CrossSearch|BookmarkList|UsagePanel|Autocomplete|WaveOverlay|Login)\s*(?:,|\()"###,
    ),
    ("active", r###"\b(?:pub\s+)?active\s*:\s*bool\b"###),
    (
        "attached",
        r###"\blet\s+mut\s+(?:scroll_offset|auto_scroll|pending_consent_approvals|retract_confirm|pending_consolidation_card)\b|\bAttachedApprovals\s*::\s*default\s*\(|\b(?:with_lent_input|render_attached_composer|paint_overlays)\b"###,
    ),
    (
        "render",
        r###"\b(?:notification_log|quit_confirm|help_overlay|command_palette|decision_pane|model_selector|profile_switcher|usage_panel|login_dialog|wave_overlay|search_bar|cross_search|bookmark_list|reverse_search)\s*::\s*render\w*\s*\("###,
    ),
    // Payload presence cannot become a second open authority, regardless of read syntax.
    ("option", r"\.\s*(?:notification_log|quit_confirm)\b"),
    (
        "interceptor",
        r"\b(?:notification_log|quit_confirm)\s*::\s*(?:handle_\w+|execute)\s*\(",
    ),
    ("close-surfaces", r"\bclose_surfaces\b"),
];

// Canonical bytes retain an offset back into the masked source. Rustfmt may wrap
// tokens or add trailing commas, but a pin must still cover this exact hit in
// this exact named scope; it never grants the rest of a function an exemption.
fn tokens(code: &str) -> (String, Vec<usize>) {
    let bytes = code.as_bytes();
    let mut text = Vec::with_capacity(bytes.len());
    let mut offsets = Vec::with_capacity(bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        if b.is_ascii_whitespace() {
            continue;
        }
        if b == b','
            && bytes[i + 1..]
                .iter()
                .find(|b| !b.is_ascii_whitespace())
                .is_none_or(|b| *b == b')')
        {
            continue;
        }
        text.push(b);
        offsets.push(i);
    }
    (String::from_utf8(text).unwrap(), offsets)
}

fn scan(files: &[(String, String)], pins: &[Pin<'_>]) -> Vec<String> {
    static PATTERNS: LazyLock<Vec<(&str, Regex)>> = LazyLock::new(|| {
        RULES
            .iter()
            .map(|(rule, pattern)| (*rule, re(pattern)))
            .collect()
    });
    static ACTIVE_TYPES: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r"NotificationLog|QuitConfirm|CommandPalette|HelpOverlay|WhichKey|Autocomplete|ReverseSearch|SearchState|CrossSearch|ModelSelector|ProfileSwitcher|UsagePanel|LoginOverlay",
        )
    });
    let mut errors = Vec::new();
    let mut unused: Vec<_> = pins
        .iter()
        .flat_map(|(p, o, ss)| {
            ss.iter().map(move |s| {
                let mut parts = s.splitn(3, '|');
                let rule = parts.next().unwrap();
                let name = parts.next().expect("pin names its scope");
                let text = parts.next().expect("pin names its tokens");
                (*p, *o, *s, rule, name, tokens(text).0)
            })
        })
        .collect();
    for (_, owner, _, _, _, _) in &unused {
        assert!(["4-A2", "4-B", "4-C", "21-6", "21-8"].contains(owner));
    }
    for (path, source) in files {
        let code = production_part(source);
        let (normalized, offsets) = tokens(&code);
        let own_payload = path.starts_with(PRESENTATION)
            || [
                "src/adapters/tui/notification_log.rs",
                "src/adapters/tui/quit_confirm.rs",
            ]
            .contains(&path.as_str());
        for (rule, pattern) in PATTERNS.iter() {
            if (rule.starts_with("focus-") || *rule == "render") && path.starts_with(PRESENTATION) {
                continue;
            }
            if *rule == "option" && own_payload {
                continue;
            }
            if *rule == "interceptor" && path.starts_with(PRESENTATION) {
                continue;
            }
            if *rule == "variant" && path != "src/domain/models/visual.rs" {
                continue;
            }
            if *rule == "attached"
                && !path.ends_with("/attach_keys.rs")
                && !path.ends_with("/attach_loop.rs")
            {
                continue;
            }
            for m in pattern.find_iter(&code) {
                if *rule == "focus-write" && code.as_bytes().get(m.end()) == Some(&b'=') {
                    continue;
                }
                if *rule == "active" && !ACTIVE_TYPES.is_match(&scope(&code, m.start())) {
                    continue;
                }
                let start = code[..m.start()].rfind('\n').map_or(0, |n| n + 1);
                let end = code[m.end()..]
                    .find('\n')
                    .map_or(code.len(), |n| m.end() + n);
                let name = scope(&code, m.start());
                let site = format!("{rule}|{name}|{}", compact(&code[start..end]));
                // A1 ownership bans are not eligible for a future-slice pin.
                let hard = ["option", "interceptor", "close-surfaces"].contains(rule)
                    || (*rule == "render"
                        && (m.as_str().starts_with("notification_log")
                            || m.as_str().starts_with("quit_confirm")));
                let hit = offsets.partition_point(|&offset| offset < m.start());
                if let Some(n) = unused.iter().position(|(p, _, _, r, n, text)| {
                    !hard
                        && *p == path.as_str()
                        && *r == *rule
                        && *n == name
                        && normalized.match_indices(text.as_str()).any(|(at, _)| {
                            let end = at + text.len();
                            at <= hit
                                && hit < end
                                && !(text
                                    .as_bytes()
                                    .last()
                                    .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
                                    && normalized
                                        .as_bytes()
                                        .get(end)
                                        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_'))
                        })
                }) {
                    unused.remove(n);
                } else {
                    errors.push(format!(
                        "{path}:{}: {site}",
                        code[..m.start()].matches('\n').count() + 1
                    ));
                }
            }
        }
        if let Some((_, name)) = HOSTS.iter().find(|(p, _)| *p == path.as_str()) {
            let definitions: Vec<_> = re(&format!(r"\bfn\s+{name}\s*\("))
                .find_iter(&code)
                .map(|m| m.start())
                .collect();
            if definitions.len() != 1 {
                errors.push(format!("{path}: expected unique fn {name}"));
                continue;
            }
            let begin = definitions[0];
            let body = &code[begin..item_end(&code, begin)];
            let calls = re(r"\bpresentation\s*::\s*compose\s*\(");
            if calls.find_iter(body).count() != 1 || calls.find_iter(&code).count() != 1 {
                errors.push(format!(
                    "{path}: {name} must call presentation::compose exactly once"
                ));
            }
        }
    }
    for (path, owner, site, _, _, _) in unused {
        errors.push(format!("stale pin {path} ({owner}): {site}"));
    }
    errors
}
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}
#[test]
fn k6_every_production_site_is_owned_and_both_draws_compose_once() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut paths = Vec::new();
    rust_files(&root.join("src"), &mut paths);
    paths.sort();
    let total = paths.len();
    let mut skipped = Vec::new();
    let files: Vec<_> = paths
        .iter()
        .filter_map(|p| {
            let name = p
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if p.file_name()
                .is_some_and(|n| n == "tests.rs" || n.to_string_lossy().ends_with("_tests.rs"))
            {
                skipped.push(name);
                None
            } else {
                Some((name, fs::read_to_string(p).unwrap()))
            }
        })
        .collect();
    assert_eq!(skipped, TEST_FILES);
    assert_eq!(files.len() + skipped.len(), total, "checked == total");
    assert!(files.len() > 500);
    for (host, _) in HOSTS {
        assert_eq!(files.iter().filter(|(p, _)| p.as_str() == *host).count(), 1);
    }
    for module in ["mod", "stack", "reducer", "route", "compose", "geometry"] {
        assert_eq!(
            files
                .iter()
                .filter(|(p, _)| p == &format!("{PRESENTATION}{module}.rs"))
                .count(),
            1
        );
    }
    let errors = scan(&files, PINS);
    assert!(errors.is_empty(), "{errors:#?}");
}
const PATTERN_FIELD_READS: [&str; 12] = [
    "matches!(s.notification_log, Some(_));",
    "matches!(&s.notification_log, Some(_));",
    "matches!(&mut s.notification_log, Some(_));",
    "match s.notification_log { Some(_) => (), None => () }",
    "match &s.notification_log { Some(_) => (), None => () }",
    "match &mut s.notification_log { Some(_) => (), None => () }",
    "matches!(s.quit_confirm, Some(_));",
    "matches!(&s.quit_confirm, Some(_));",
    "matches!(&mut s.quit_confirm, Some(_));",
    "match s.quit_confirm { Some(_) => (), None => () }",
    "match &s.quit_confirm { Some(_) => (), None => () }",
    "match &mut s.quit_confirm { Some(_) => (), None => () }",
];
#[test]
fn k6_same_scanner_rejects_each_forbidden_mechanism() {
    let cases = [
        ("focus-write", "s.focus = next;"),
        ("focus-borrow", "let f = &mut s.focus;"),
        ("focus-mem", "mem::replace(&mut s.focus, next);"),
        ("previous", "s.previous_focus.take();"),
        ("prior", "s.prior_focus.take();"),
        ("reservation", "reserve_search_bar(&mut layout);"),
        ("overlay", "let f = OverlayType::Help;"),
        ("active", "struct HelpOverlayState { pub active: bool }"),
        ("attached", "let mut scroll_offset = 0;"),
        ("render", "help_overlay::render(f);"),
        ("option", "if s.notification_log.is_some() {}"),
        ("option", "let Some(q) = &s.quit_confirm else { return; };"),
        ("interceptor", "quit_confirm::handle_domain_event(s,e);"),
        ("close-surfaces", "close_surfaces(s);"),
        ("render", "notification_log::render(f);"),
        ("render", "quit_confirm::render(f);"),
        ("render", "reverse_search::render(f);"),
        ("option", "s\n.quit_confirm\n.as_ref();"),
    ];
    for (rule, text) in cases
        .into_iter()
        .chain(PATTERN_FIELD_READS.map(|text| ("option", text)))
    {
        let path = if rule == "attached" {
            "src/infrastructure/runtime/attach_keys.rs"
        } else {
            "src/adapters/tui/state.rs"
        };
        let errors = scan(&[(path.into(), format!("fn witness() {{ {text} }}"))], &[]);
        assert!(
            errors.iter().any(|s| s.contains(&format!("{rule}|"))),
            "{rule}: {errors:?}"
        );
    }
    let errors = scan(
        &[(
            "src/domain/models/visual.rs".into(),
            "enum OverlayType {\n Help,\n}".into(),
        )],
        &[],
    );
    assert!(errors.iter().any(|s| s.contains("variant|")));
    for field in ["notification_log", "quit_confirm"] {
        for query in [
            "is_some",
            "is_none",
            "as_ref",
            "as_mut",
            "is_some_and",
            "is_none_or",
        ] {
            assert!(
                !scan(
                    &[(
                        "src/x.rs".into(),
                        format!("fn f() {{ s.{field}.{query}(); }}")
                    )],
                    &[]
                )
                .is_empty()
            );
        }
    }
    for token in [
        "reserve_search_bar",
        "reserve_bookmark_panel",
        "search_bar",
        "bookmark_panel",
    ] {
        assert!(
            !scan(
                &[("src/x.rs".into(), format!("fn f() {{ {token}; }}"))],
                &[]
            )
            .is_empty()
        );
    }
    for token in [
        "scroll_offset",
        "auto_scroll",
        "pending_consent_approvals",
        "retract_confirm",
        "pending_consolidation_card",
    ] {
        assert!(
            !scan(
                &[(
                    "src/infrastructure/runtime/attach_keys.rs".into(),
                    format!("fn f() {{ let mut {token} = 0; }}")
                )],
                &[]
            )
            .is_empty()
        );
    }
    for text in [
        "AttachedApprovals::default();",
        "with_lent_input();",
        "render_attached_composer();",
        "paint_overlays();",
    ] {
        assert!(
            !scan(
                &[(
                    "src/infrastructure/runtime/attach_keys.rs".into(),
                    format!("fn f() {{ {text} }}")
                )],
                &[]
            )
            .is_empty()
        );
    }
    for (path, name) in HOSTS {
        for text in [
            "fn other() { presentation::compose(f); }".to_owned(),
            format!("fn {name}() {{}}"),
            format!("fn {name}() {{ presentation::compose(f); presentation::compose(f); }}"),
            format!("fn {name}() {{}} fn {name}() {{}}"),
            format!("fn {name}() {{}} fn other() {{ presentation::compose(f); }}"),
            format!(
                "fn {name}() {{ presentation::compose(f); }} fn other() {{ presentation::compose(f); }}"
            ),
        ] {
            assert!(!scan(&[((*path).into(), text)], &[]).is_empty());
        }
        assert!(
            scan(
                &[(
                    (*path).into(),
                    format!("fn {name}() {{ presentation::compose(f); }}")
                )],
                &[]
            )
            .is_empty()
        );
    }
}
#[test]
fn k6_cfg_test_imports_do_not_hide_later_production_and_stale_pins_fail() {
    let good = "#[cfg(test)]\nuse crate::{\n TestOnly,\n};\n#[cfg(test)]\nmod tests { fn t() { s.focus = next; } }\nfn real() {}";
    assert!(scan(&[("src/x.rs".into(), good.into())], &[]).is_empty());
    assert!(
        !scan(
            &[(
                "src/x.rs".into(),
                format!("{good}\nfn later() {{ s.focus = next; }}")
            )],
            &[]
        )
        .is_empty()
    );
    const ONE: &[Pin<'_>] = &[(
        "src/x.rs",
        "4-C",
        &["focus-write|real|fn real() { s.focus = next; }"],
    )];
    let file = ("src/x.rs".into(), "fn real() { s.focus = next; }".into());
    assert!(scan(&[file], ONE).is_empty());
    for files in [vec![], vec![("src/x.rs".into(), "fn real() {}".into())]] {
        assert!(scan(&files, ONE).iter().any(|s| s.contains("stale pin")));
    }
    assert!(
        !scan(
            &[(
                "src/x.rs".into(),
                "fn real() { s.focus = next; s.focus = other; }".into()
            )],
            ONE
        )
        .is_empty()
    );
    // Equality, comments and strings are not writes; an inline cfg import ends at ';'.
    assert!(scan(&[("src/x.rs".into(),"#[cfg(test)] use crate::T;\nfn real() { let _ = s.focus == next; let _ = \"s.focus = next;\"; /* s.focus = no; */ }".into())],&[]).is_empty());
}

#[test]
fn k6_named_pins_survive_formatting_not_scope_tokens_or_multiplicity_changes() {
    const ONE: &[Pin<'_>] = &[(
        "src/x.rs",
        "4-C",
        &["focus-write|real|s.focus = restore(old, next);"],
    )];
    assert!(
        scan(
            &[(
                "src/x.rs".into(),
                "fn real() {\n s . focus = restore(\n old,\n next,\n );\n}".into()
            )],
            ONE
        )
        .is_empty()
    );
    const ARG: &[Pin<'_>] = &[(
        "src/x.rs",
        "4-B",
        &["overlay|real|OverlayType::ModelSelector,"],
    )];
    assert!(
        scan(
            &[(
                "src/x.rs".into(),
                "fn real() { open(OverlayType::ModelSelector,); }".into()
            )],
            ARG
        )
        .is_empty()
    );
    for (path, source) in [
        ("src/x.rs", "fn renamed() { s.focus = restore(old, next); }"),
        ("src/y.rs", "fn real() { s.focus = restore(old, next); }"),
        ("src/x.rs", "fn real() { s.focus = restore(other, next); }"),
        (
            "src/x.rs",
            "fn real() { s.focus = restore(old, next); s.focus = restore(old, next); }",
        ),
    ] {
        assert!(!scan(&[(path.into(), source.into())], ONE).is_empty());
    }
    const TWO: &[Pin<'_>] = &[(
        "src/x.rs",
        "4-C",
        &[
            "focus-write|real|s.focus = next;",
            "focus-write|real|s.focus = next;",
        ],
    )];
    let errors = scan(
        &[("src/x.rs".into(), "fn real() { s.focus = next; }".into())],
        TWO,
    );
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("stale pin"));
}

#[test]
fn k6_a1_bans_cannot_be_pinned_and_payload_exemptions_are_exact() {
    for (rule, text) in [
        ("option", "s.quit_confirm.is_some();"),
        (
            "option",
            "let Some(log) = &s.notification_log else { return; };",
        ),
        ("interceptor", "quit_confirm::execute(s, host, key);"),
        (
            "interceptor",
            "notification_log::handle_key(s, host, key, facts);",
        ),
        ("close-surfaces", "close_surfaces(s);"),
        ("render", "notification_log::render(f);"),
        ("render", "quit_confirm::render(f);"),
    ]
    .into_iter()
    .chain(PATTERN_FIELD_READS.map(|text| ("option", text)))
    {
        let site = format!("{rule}|real|{text}");
        let sites = [site.as_str()];
        // These pins are fixture-local, not additions to the production allowlist.
        let pins = [("src/x.rs", "4-A2", sites.as_slice())];
        let errors = scan(
            &[("src/x.rs".into(), format!("fn real() {{ {text} }}"))],
            &pins,
        );
        assert!(
            errors
                .iter()
                .any(|s| s.contains(&format!("{rule}|")) && !s.starts_with("stale pin"))
        );
        assert!(errors.iter().any(|s| s.starts_with("stale pin")));
    }
    let payload = format!(
        "fn real() {{ s.notification_log.as_ref(); s.quit_confirm.is_some(); {} }}",
        PATTERN_FIELD_READS.join(" ")
    );
    for path in [
        "src/adapters/tui/notification_log.rs",
        "src/adapters/tui/quit_confirm.rs",
        "src/adapters/tui/presentation/route.rs",
    ] {
        assert!(scan(&[(path.into(), payload.clone())], &[]).is_empty());
    }
    for path in [
        "src/unrelated/notification_log.rs",
        "src/unrelated/quit_confirm.rs",
    ] {
        assert!(!scan(&[(path.into(), payload.clone())], &[]).is_empty());
    }
    assert!(scan(&[("src/adapters/tui/presentation/route.rs".into(),"fn real() { quit_confirm::execute(s, h, k); notification_log::render(f); s.focus = base; }".into())],&[]).is_empty());
}
