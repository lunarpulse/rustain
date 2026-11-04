//! K6 / A19: 4-Bb2a ratchet. Behaviour belongs to K1–K5/K7–K10, not this scanner.
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

// Exact named-site pins from da41ddae, minus A1/A2a cutovers; whitespace is not ownership.
#[rustfmt::skip]
const PINS: &[Pin<'_>] = &[
    ("src/adapters/tui/agent_inspector_keys.rs", "4-C", &[
        r###"focus-write|on_key|state.focus = FocusState::Chat;"###,
    ]),
    ("src/adapters/tui/app.rs", "4-C", &[
        r###"focus-write|advance_deferred_permission|state.focus = FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Permission));"###, r###"focus-write|advance_deferred_permission|state.focus = if state.recipient_surface.is_some() {"###, r###"focus-write|dispatch_input|state.focus = FocusState::Chat;"###,
        r###"focus-write|card_special_key|state.focus ="###, r###"focus-write|card_special_key|state.focus = FocusState::Input;"###, r###"focus-write|exec_input|state.focus = FocusState::Chat;"###,
        r###"focus-write|exec_chat|state.focus = FocusState::Input;"###, r###"focus-write|exec_sidebar|state.focus = FocusState::Chat;"###, r###"focus-write|exec_sidebar|state.focus = FocusState::Input;"###, r###"focus-write|sidebar_move|state.focus = FocusState::Sidebar {"###,
        r###"focus-write|exec_panel|state.focus = FocusState::Sidebar {"###, r###"focus-write|exec_panel|state.focus = FocusState::Chat;"###, r###"focus-write|exec_recipient|state.focus = surface.prior_focus;"###,
        r###"focus-write|handle_char|state.focus = FocusState::Input;"###,
        r###"focus-write|dispatch_palette_action|state.focus = FocusState::Input;"###,
    ]),
    ("src/adapters/tui/app.rs", "21-8", &[
        r###"prior|exec_recipient|state.focus = surface.prior_focus;"###,
    ]),
    ("src/adapters/tui/decision_pane.rs", "4-C", &[
        r###"focus-write|settle_borrowed_card_focus|state.focus = state.card_focus_borrowed_from.take().unwrap();"###, r###"focus-borrow|take_chat_focus_for_local_card|&mut state.focus,"###,
        r###"focus-write|apply_resolved_action|state.focus = FocusState::Input;"###, r###"focus-write|apply_resolved_action|state.focus = if state.sidebar_visible {"###,
    ]),
    ("src/adapters/tui/handlers/artifact_command.rs", "4-C", &[
        r###"focus-write|resolve_apply_card|state.focus = card.prior_focus.clone();"###,
    ]),
    ("src/adapters/tui/handlers/artifact_command.rs", "21-6", &[
        r###"prior|resolve_apply_card|state.focus = card.prior_focus.clone();"###,
    ]),
    ("src/adapters/tui/handlers/export.rs", "4-C", &[
        r###"focus-write|apply_confirm_export_overwrite|state.focus = FocusState::Chat;"###, r###"focus-write|apply_cancel_export_overwrite|state.focus = FocusState::Chat;"###, r###"focus-write|apply_export_command|state.focus ="###,
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
    ("src/adapters/tui/state.rs", "21-6", &[
        r###"prior|PendingArtifactCard|pub prior_focus: crate::domain::models::FocusState,"###, r###"prior|PendingPeerAdd|pub prior_focus: crate::domain::models::FocusState,"###, r###"prior|PendingTeamRetract|pub prior_focus: crate::domain::models::FocusState,"###,
    ]),
    ("src/adapters/tui/state.rs", "21-8", &[
        r###"prior|RecipientSurface|pub prior_focus: FocusState,"###,
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
        r###"focus-write|execute_attached|ctx.state.focus = FocusState::Input;"###,
        r###"focus-write|execute_attached|FocusState::Input => ctx.state.focus = FocusState::Chat,"###, r###"focus-write|execute_attached|FocusState::Chat => ctx.state.focus = FocusState::Input,"###,
        r###"focus-write|begin_retract|ctx.state.focus = FocusState::Chat;"###, r###"focus-borrow|begin_retract|&mut ctx.state.focus,"###, r###"attached|attached_card_context|fn with_lent_input<R>("###, r###"attached|execute_attached|with_lent_input(ctx.state, ctx.input, |state| {"###, r###"attached|execute_attached|with_lent_input(ctx.state, ctx.input, cancel_autocomplete_popup);"###,
        r###"attached|popup_delegate|with_lent_input(ctx.state, ctx.input, |state| {"###, r###"attached|palette_delegate|let action = with_lent_input(ctx.state, ctx.input, |state| {"###,
    ]),
    ("src/infrastructure/runtime/attach_loop.rs", "4-C", &[
        r###"focus-write|open_response_surface|state.focus = FocusState::Chat;"###, r###"focus-write|recipient_surface_key|state.focus = surface.prior_focus;"###, r###"focus-write|recipient_surface_key|state.focus = FocusState::Input;"###, r###"focus-write|attached_proposal_key|state.focus = surface.prior_focus;"###, r###"focus-write|run_attached|state.focus = FocusState::Chat;"###, r###"focus-borrow|open_response_surface|None => (std::mem::replace(&mut state.focus, FocusState::Chat), None),"###,
        r###"focus-borrow|attached_card_arrived|&mut state.focus,"###, r###"focus-mem|open_response_surface|None => (std::mem::replace(&mut state.focus, FocusState::Chat), None),"###, r###"attached|apply_attached_consolidation_frame|fn render_attached_composer("###, r###"attached|run_attached|let mut scroll_offset = 0usize;"###, r###"attached|run_attached|let mut auto_scroll = true;"###, r###"attached|run_attached|let mut pending_consolidation_card: Option<"###,
        r###"attached|run_attached|let mut retract_confirm: Option<(String, String)> = None;"###, r###"attached|run_attached|let mut pending_consent_approvals = std::collections::VecDeque::<PendingConsentApproval>::new();"###, r###"attached|run_attached|let mut approvals = AttachedApprovals::default();"###, r###"attached|run_attached|render_attached_composer("###,
    ]),
    ("src/infrastructure/runtime/attach_loop.rs", "21-8", &[
        r###"prior|open_response_surface|let (prior_focus, root_id) = match state.recipient_surface.take() {"###, r###"prior|open_response_surface|surface.prior_focus,"###, r###"prior|open_response_surface|prior_focus,"###, r###"prior|recipient_surface_key|state.focus = surface.prior_focus;"###, r###"prior|attached_proposal_key|state.focus = surface.prior_focus;"###,
    ]),
    ("src/infrastructure/runtime/event_loop.rs", "4-C", &[
        r###"focus-write|run|state.focus = FocusState::Overlay(OverlayType::Confirmation("###, r###"focus-write|run|state.focus = FocusState::Input;"###, r###"focus-write|run|state.focus = FocusState::Input;"###, r###"focus-write|run|state.focus = FocusState::Input;"###,
        r###"focus-write|run|state.focus = FocusState::Input;"###, r###"focus-write|run|state.focus = FocusState::Input;"###, r###"focus-write|run|state.focus = FocusState::Input;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###,
        r###"focus-write|run|state.focus = FocusState::Overlay("###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###,
        r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Overlay("###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###,
        r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###, r###"focus-write|run|state.focus = FocusState::Chat;"###,
        r###"focus-write|run|state.focus = FocusState::Sidebar {"###,
        r###"focus-write|run|state.focus = FocusState::Overlay(OverlayType::Confirmation("###,
        r###"focus-write|run|state.focus = FocusState::Input;"###, r###"focus-write|run|state.focus = FocusState::Input;"###, r###"focus-write|run|state.focus = FocusState::Overlay(OverlayType::Confirmation("###, r###"focus-write|run|state.focus = FocusState::Overlay(OverlayType::Confirmation("###, r###"focus-write|run|state.focus = FocusState::Overlay(OverlayType::Confirmation("###, r###"focus-write|run|state.focus = FocusState::Overlay(OverlayType::Confirmation("###,
        r###"focus-write|restore_focus_after_modal|state.focus = if state.pending_feedback_input.is_some() {"###, r###"focus-write|surface_deferred_modal|state.focus ="###, r###"focus-write|advance_skill_trust_queue|state.focus = FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::SkillTrust));"###,
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
        r###"\b(?:notification_log|quit_confirm|help_overlay|command_palette|which_key_bar|decision_pane|autocomplete_popup|model_selector|profile_switcher|usage_panel|login_dialog|wave_overlay|stage_search|search_bar|cross_search|bookmark_list|reverse_search)\s*::\s*render\w*\s*\("###,
    ),
    // Payload presence cannot become a second open authority, regardless of read syntax.
    ("option", r"\.\s*(?:notification_log|quit_confirm)\b"),
    (
        "interceptor",
        r"\b(?:notification_log|quit_confirm|decision_pane)\s*::\s*(?:handle_\w+|execute)\s*\(",
    ),
    ("close-surfaces", r"\bclose_surfaces\b"),
    (
        "migrated-owner",
        r"\b(?:WhichKeyState|which_key)\b|OverlayType\s*::\s*(?:Help|CommandPalette|WhichKey|ModelSelector|ProfileSwitcher|UsagePanel|Login|Search|ReverseSearch|CrossSearch|BookmarkList|WaveOverlay)\b|\.\s*(?:help_overlay|command_palette|model_selector|profile_switcher|usage_panel|login|search_state|cross_search|bookmark_list|wave_overlay)\s*\.\s*(?:active|previous_focus|prior_focus|open|close|dismiss|is_open)\b|\.\s*reverse_search\s*\.\s*(?:active|previous_focus|prior_focus|open|close)\b|\b(?:HelpOverlayState|CommandPaletteState|ModelSelectorState|ProfileSwitcherState|UsagePanelState|LoginOverlayState|SearchState|ReverseSearchState|CrossSearchState|BookmarkListState|WaveOverlayState)\s*\{[^{}]*\b(?:active|previous_focus|prior_focus)\b|\b(?:help_overlay|command_palette|model_selector|profile_switcher|usage_panel|login|search_state|reverse_search|cross_search|bookmark_list|wave_overlay)\s*:\s*(?:bool|Option\s*<)",
    ),
    (
        "migrated-payload-owner",
        r"\b(?:active|previous_focus|prior_focus)\b|\bfn\s+(?:open|close|dismiss|is_open)\s*\(",
    ),
    (
        "reverse-payload-owner",
        r"\b(?:active|previous_focus|prior_focus)\b|\bfn\s+(?:open|close)\s*\(",
    ),
    (
        "a2b-owner",
        r"OverlayType\s*::\s*Autocomplete\b|\.\s*autocomplete\s*\.\s*active\b|\.\s*decisions\s*\.\s*(?:focused|is_focused)\b",
    ),
    ("a2b-payload-owner", r"\b(?:pub\s+)?(?:active|focused)\s*:"),
    ("legacy-painter", r"\bpaint_legacy\b"),
    (
        "field-string",
        r"\b(?:query|filter_text)\s*:\s*(?:std\s*::\s*string\s*::\s*)?String\b",
    ),
    (
        "field-tail",
        r"\.\s*(?:command_palette\s*\.\s*filter_text|(?:search_state|reverse_search)\s*\.\s*query)\s*\.\s*(?:push|push_str|pop|truncate|drain|replace_range|insert_str)\s*\(|\bfn\s+(?:palette_edit|search_edit)\b|\b(?:SearchClear|PaletteBackspace|RevBackspace)\b",
    ),
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
            r"NotificationLog|QuitConfirm|CommandPalette|HelpOverlay|WhichKey|Autocomplete|ReverseSearch|SearchState|CrossSearch|ModelSelector|ProfileSwitcher|UsagePanel|LoginOverlay|BookmarkList|WaveOverlay",
        )
    });
    static MIGRATED: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r"\b(?:Help|CommandPalette|WhichKey|Autocomplete|ModelSelector|ProfileSwitcher|UsagePanel|Login|Search|ReverseSearch|CrossSearch|BookmarkList|WaveOverlay)\b",
        )
    });
    static PAYLOAD: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r"\b(?:HelpOverlayState|CommandPaletteState|WhichKeyState|ModelSelectorState|ProfileSwitcherState|UsagePanelState|LoginOverlayState|SearchState|CrossSearchState|BookmarkListState|WaveOverlayState)\b",
        )
    });
    static PAINTER: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r"^(?:notification_log|quit_confirm|help_overlay|command_palette|which_key_bar|decision_pane|autocomplete_popup|model_selector|profile_switcher|usage_panel|login_dialog|stage_search|reverse_search|cross_search|bookmark_list|wave_overlay)\b",
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
        assert!(["4-B", "4-C", "21-6", "21-8"].contains(owner));
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
                if *rule == "migrated-payload-owner" && !PAYLOAD.is_match(&scope(&code, m.end())) {
                    continue;
                }
                if *rule == "reverse-payload-owner"
                    && !scope(&code, m.end()).contains("ReverseSearchState")
                {
                    continue;
                }
                if *rule == "active" && !ACTIVE_TYPES.is_match(&scope(&code, m.start())) {
                    continue;
                }
                if *rule == "a2b-payload-owner"
                    && !matches!(
                        scope(&code, m.start()).as_str(),
                        "AutocompleteState" | "DecisionPane"
                    )
                {
                    continue;
                }
                if *rule == "field-string"
                    && !matches!(
                        scope(&code, m.start()).as_str(),
                        "CommandPaletteState" | "SearchState" | "ReverseSearchState"
                    )
                {
                    continue;
                }
                let start = code[..m.start()].rfind('\n').map_or(0, |n| n + 1);
                let end = code[m.end()..]
                    .find('\n')
                    .map_or(code.len(), |n| m.end() + n);
                let name = scope(&code, m.start());
                let site = format!("{rule}|{name}|{}", compact(&code[start..end]));
                // Migrated ownership cannot be restored under a future-slice pin.
                let hard = [
                    "option",
                    "interceptor",
                    "close-surfaces",
                    "migrated-owner",
                    "migrated-payload-owner",
                    "reverse-payload-owner",
                    "a2b-owner",
                    "a2b-payload-owner",
                    "legacy-painter",
                    "field-string",
                    "field-tail",
                ]
                .contains(rule)
                    || (["overlay", "variant"].contains(rule) && MIGRATED.is_match(m.as_str()))
                    || (["active", "prior", "previous"].contains(rule) && PAYLOAD.is_match(&name))
                    || (*rule == "render" && PAINTER.is_match(m.as_str()));
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
    for module in [
        "mod",
        "stack",
        "reducer",
        "route",
        "compose",
        "geometry",
        "stage_search",
    ] {
        assert_eq!(
            files
                .iter()
                .filter(|(p, _)| p == &format!("{PRESENTATION}{module}.rs"))
                .count(),
            1
        );
    }
    assert_eq!(
        PINS.iter()
            .filter(|(_, owner, _)| *owner == "4-B")
            .map(|(_, _, sites)| sites.len())
            .sum::<usize>(),
        0,
        "Bb1b-ii removes the final six CrossSearch ownership pins"
    );
    for (path, count) in [
        ("src/adapters/tui/app.rs", 0),
        ("src/adapters/tui/state.rs", 0),
        ("src/domain/models/visual.rs", 0),
        ("src/infrastructure/runtime/event_loop.rs", 0),
    ] {
        assert_eq!(
            PINS.iter()
                .filter(|(p, owner, _)| *p == path && *owner == "4-B")
                .map(|(_, _, sites)| sites.len())
                .sum::<usize>(),
            count,
            "Bb1b-ii final ownership allocation for {path}"
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
    const ARG: &[Pin<'_>] = &[("src/x.rs", "21-6", &["prior|real|prior_focus,"])];
    assert!(
        scan(
            &[(
                "src/x.rs".into(),
                "fn real() { open(prior_focus,); }".into()
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
        let pins = [("src/x.rs", "4-C", sites.as_slice())];
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

#[test]
fn k6_a2a_ownership_reads_patterns_and_painters_are_unpinnable() {
    for text in [
        "s.help_overlay.active;",
        "matches!(s.help_overlay.active, true);",
        "match &s.command_palette.previous_focus { Some(_) => (), None => () }",
        "let HelpOverlayState { active, .. } = payload;",
        "struct CommandPaletteState { previous_focus: FocusState }",
        "impl HelpOverlayState { fn open(&mut self) {} }",
        "s.command_palette.is_open();",
        "s.help_overlay.close();",
        "s.which_key;",
        "struct WhichKeyState {}",
        "let o = OverlayType::CommandPalette;",
        "let o = OverlayType::WhichKey;",
        "struct TuiState { help_overlay: Option<HelpOverlayState> }",
        "help_overlay::render(f);",
        "command_palette::render(f);",
        "which_key_bar::render(f);",
        "s.autocomplete.active;",
        "matches!(s.autocomplete.active, true);",
        "let o = OverlayType::Autocomplete(kind);",
        "struct AutocompleteState { active: bool }",
        "s.decisions.is_focused();",
        "match &s.decisions.focused { Some(_) => (), None => () }",
        "struct DecisionPane { focused: Option<Key> }",
        "decision_pane::render(f);",
        "autocomplete_popup::render(f);",
        "s.model_selector.active;",
        "matches!(&s.profile_switcher.active, true);",
        "match &s.usage_panel.previous_focus { Some(_) => (), None => () }",
        "let LoginOverlayState { active, .. } = payload;",
        "impl ModelSelectorState { fn open(&mut self) {} }",
        "s.login.is_open();",
        "struct TuiState { usage_panel: Option<UsagePanelState> }",
        "let o = OverlayType::Login;",
        "model_selector::render(f);",
        "profile_switcher::render(f);",
        "usage_panel::render(f);",
        "login_dialog::render(f);",
    ] {
        let files = [("src/x.rs".to_owned(), format!("fn real() {{ {text} }}"))];
        let first = scan(&files, &[]);
        assert!(!first.is_empty(), "{text}");
        // Attempt all matching pins together: generic legacy rules may be
        // pinnable, but they can never exempt the migrated ownership violation.
        let sites: Vec<_> = first
            .iter()
            .map(|error| error.split_once(": ").unwrap().1)
            .collect();
        let pins = [("src/x.rs", "4-C", sites.as_slice())];
        let errors = scan(&files, &pins);
        assert!(
            errors.iter().any(|s| s.starts_with("stale pin")),
            "{text}: {errors:?}"
        );
        assert!(
            errors.iter().any(|s| !s.starts_with("stale pin")),
            "{text}: {errors:?}"
        );
    }
    for path in ["src/x.rs", "src/adapters/tui/presentation/route.rs"] {
        let payload = "fn real() { s.help_overlay.scroll_offset += 1; s.command_palette.filter_text.clear(); s.command_palette.reset(); s.command_palette.clear(); s.pending_leader = Some(Context::Help); }";
        assert!(scan(&[(path.into(), payload.into())], &[]).is_empty());
        let old = "fn real() { matches!(&s.help_overlay.active, true); }";
        assert!(!scan(&[(path.into(), old.into())], &[]).is_empty());
    }
    let payload = "struct HelpOverlayState { scroll_offset: usize } impl HelpOverlayState { fn reset(&mut self) { self.scroll_offset = 0; } }";
    assert!(scan(&[("src/x.rs".into(), payload.into())], &[]).is_empty());
    for variant in [
        "Help",
        "CommandPalette",
        "WhichKey",
        "ModelSelector",
        "ProfileSwitcher",
        "UsagePanel",
        "Login",
    ] {
        let path = "src/domain/models/visual.rs";
        let source = format!("enum OverlayType {{\n {variant},\n}}");
        let site = format!("variant|OverlayType|{variant},");
        let sites = [site.as_str()];
        let pins = [(path, "4-B", sites.as_slice())];
        let errors = scan(&[(path.into(), source)], &pins);
        assert!(
            errors
                .iter()
                .any(|s| s.contains("variant|") && !s.starts_with("stale pin"))
        );
        assert!(errors.iter().any(|s| s.starts_with("stale pin")));
    }
}

#[test]
fn k6_bb1a_legacy_owners_and_relocated_painters_cannot_be_pinned() {
    for (rule, text) in [
        ("migrated-owner", "s.search_state.active = true;"),
        ("migrated-owner", "matches!(&s.search_state.active, true);"),
        (
            "migrated-owner",
            "match &mut s.search_state.prior_focus { Some(_) => (), None => () }",
        ),
        ("migrated-owner", "s.search_state.is_open();"),
        ("migrated-owner", "s.search_state.close();"),
        (
            "migrated-owner",
            "let SearchState { active, .. } = payload;",
        ),
        (
            "migrated-owner",
            "struct TuiState { search_state: Option<SearchState> }",
        ),
        (
            "migrated-payload-owner",
            "impl SearchState { fn open(&mut self) {} }",
        ),
        ("migrated-owner", "s.reverse_search.active;"),
        (
            "migrated-owner",
            "matches!(&mut s.reverse_search.active, true);",
        ),
        (
            "migrated-owner",
            "match &s.reverse_search.previous_focus { Some(_) => (), None => () }",
        ),
        (
            "migrated-owner",
            "let ReverseSearchState { active, .. } = payload;",
        ),
        ("migrated-owner", "struct TuiState { reverse_search: bool }"),
        (
            "reverse-payload-owner",
            "impl ReverseSearchState { fn open(&mut self) {} }",
        ),
        (
            "reverse-payload-owner",
            "impl ReverseSearchState { fn close(&mut self) {} }",
        ),
        ("overlay", "let o = OverlayType::Search;"),
        ("overlay", "let o = OverlayType::ReverseSearch;"),
        ("render", "stage_search::render(f);"),
        ("render", "reverse_search::render(f);"),
    ] {
        let files = [("src/x.rs".to_owned(), format!("fn real() {{ {text} }}"))];
        let first = scan(&files, &[]);
        assert!(
            first
                .iter()
                .any(|error| error.contains(&format!("{rule}|"))),
            "{text}: {first:?}"
        );
        let sites: Vec<_> = first
            .iter()
            .map(|error| error.split_once(": ").unwrap().1)
            .collect();
        let pins = [("src/x.rs", "4-B", sites.as_slice())];
        let errors = scan(&files, &pins);
        assert!(
            errors.iter().any(|error| error.starts_with("stale pin")),
            "{text}: {errors:?}"
        );
        assert!(
            errors.iter().any(|error| !error.starts_with("stale pin") && error.contains(&format!("{rule}|"))),
            "{text}: {errors:?}"
        );
    }
    for variant in ["Search", "ReverseSearch"] {
        let path = "src/domain/models/visual.rs";
        let source = format!("enum OverlayType {{\n {variant},\n}}");
        let site = format!("variant|OverlayType|{variant},");
        let sites = [site.as_str()];
        let pins = [(path, "4-B", sites.as_slice())];
        let errors = scan(&[(path.into(), source)], &pins);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("variant|") && !error.starts_with("stale pin"))
        );
        assert!(errors.iter().any(|error| error.starts_with("stale pin")));
    }
    // Presentation is the painter boundary, never an escape for banned reservations.
    let path = "src/adapters/tui/presentation/stage_search.rs";
    assert!(
        scan(
            &[(
                path.into(),
                "fn real() { stage_search::render(f); reverse_search::render(f); }".into()
            )],
            &[]
        )
        .is_empty()
    );
    for token in [
        "reserve_search_bar",
        "reserve_bookmark_panel",
        "search_bar",
        "bookmark_panel",
    ] {
        let errors = scan(&[(path.into(), format!("fn real() {{ {token}; }}"))], &[]);
        assert!(
            errors.iter().any(|error| error.contains("reservation|")),
            "{token}: {errors:?}"
        );
    }
}

#[test]
fn k6_bb1a_payload_edits_and_routed_popup_api_remain_allowed() {
    for path in ["src/x.rs", "src/adapters/tui/presentation/route.rs"] {
        let source = "fn real() { s.search_state.query.insert_char(c); s.reverse_search.query.clear(); s.reverse_search.is_open(); s.reverse_search.reset(); s.reverse_search.dismiss(); s.reverse_search.selected_match = Some(0); }";
        assert!(
            scan(&[(path.into(), source.into())], &[]).is_empty(),
            "{path}"
        );
        for field in ["search_state", "reverse_search"] {
            let source = format!("fn real() {{ matches!(&s.{field}.active, true); }}");
            assert!(
                !scan(&[(path.into(), source)], &[]).is_empty(),
                "{path}: {field}"
            );
        }
    }
    let path = "src/adapters/tui/state.rs";
    let source = "struct SearchState { query: TextField } impl SearchState { fn reset(&mut self) { self.query.clear(); } } struct ReverseSearchState { selected_match: Option<usize> } impl ReverseSearchState { fn is_open(&self) -> bool { self.selected_match.is_some() } fn reset(&mut self) { self.selected_match = Some(0); } fn dismiss(&mut self) { self.selected_match = None; } }";
    assert!(scan(&[(path.into(), source.into())], &[]).is_empty());
    // Confirmation focus is still 4-C-owned; exact named-site pins remain usable.
    let source = "fn real() { s.focus = FocusState::Overlay(OverlayType::Confirmation(kind)); }";
    let pins = [(
        "src/x.rs",
        "4-C",
        &["focus-write|real|s.focus = FocusState::Overlay(OverlayType::Confirmation(kind));"][..],
    )];
    assert!(scan(&[("src/x.rs".into(), source.into())], &pins).is_empty());
}

#[test]
fn bookmark_wave_ownership_and_painters_cannot_hide_behind_future_pins() {
    for (surface, payload, painter) in [
        ("BookmarkList", "BookmarkListState", "bookmark_list"),
        ("WaveOverlay", "WaveOverlayState", "wave_overlay"),
    ] {
        for (source, rule, scope_name) in [
            (
                format!(
                    "fn real(s: &TuiState) {{ let _ = matches!(&s.focus, FocusState::Overlay(OverlayType::{surface})); }}"
                ),
                "overlay",
                "real",
            ),
            (
                format!(
                    "fn real(s: &TuiState) {{ match &s.focus {{ FocusState::Overlay(OverlayType::{surface}) => (), _ => () }} }}"
                ),
                "overlay",
                "real",
            ),
            (
                format!("struct {payload} {{ pub active: bool }}"),
                "active",
                payload,
            ),
            (
                format!("fn real() {{ {painter}::render(); }}"),
                "render",
                "real",
            ),
        ] {
            let path = "src/ownership_control.rs";
            assert!(
                !scan(&[(path.into(), source.clone())], &[]).is_empty(),
                "{source}"
            );
            let pin = format!("{rule}|{scope_name}|{source}");
            let errors = scan(&[(path.into(), source.clone())], &[(path, "4-C", &[&pin])]);
            assert!(
                errors.iter().any(|error| !error.starts_with("stale pin")),
                "a hard ownership violation, not merely a stale pin: {errors:?}"
            );
        }
        let legal = format!(
            "fn real(s: &mut TuiState) {{ presentation::reducer::open(s, Surface::{surface}); }}"
        );
        assert!(
            scan(&[("src/ownership_control.rs".into(), legal)], &[]).is_empty(),
            "ordinary frame requests are not legacy ownership"
        );
    }
    let legacy = "fn real(s: &mut TuiState) { s.focus = FocusState::Overlay(OverlayType::Confirmation(kind)); }";
    let pin = format!("focus-write|real|{legacy}");
    assert!(!scan(&[("src/ownership_control.rs".into(), legacy.into())], &[]).is_empty());
    assert!(
        scan(
            &[("src/ownership_control.rs".into(), legacy.into())],
            &[("src/ownership_control.rs", "4-C", &[&pin])]
        )
        .is_empty(),
        "control: the same named pin mechanism really accepts an owned legacy site"
    );
    assert_eq!(
        PINS.iter()
            .filter(|(_, owner, _)| *owner == "4-B")
            .flat_map(|(_, _, sites)| sites.iter())
            .count(),
        0
    );
}

#[test]
fn k6_cross_search_owners_patterns_and_renderer_reject_future_pins() {
    for (rule, text) in [
        ("migrated-owner", "s.cross_search.active = true;"),
        ("migrated-owner", "matches!(s.cross_search.active, true);"),
        ("migrated-owner", "matches!(&s.cross_search.active, true);"),
        (
            "migrated-owner",
            "matches!(&mut s.cross_search.active, true);",
        ),
        (
            "migrated-owner",
            "match s.cross_search.active { true => (), false => () }",
        ),
        (
            "migrated-owner",
            "match &s.cross_search.previous_focus { Some(_) => (), None => () }",
        ),
        (
            "migrated-owner",
            "match &mut s.cross_search.prior_focus { Some(_) => (), None => () }",
        ),
        (
            "migrated-owner",
            "let CrossSearchState { active, .. } = payload;",
        ),
        (
            "migrated-owner",
            "let CrossSearchState { previous_focus, .. } = payload;",
        ),
        (
            "migrated-owner",
            "let CrossSearchState { prior_focus, .. } = payload;",
        ),
        ("migrated-owner", "struct TuiState { cross_search: bool }"),
        (
            "migrated-owner",
            "struct TuiState { cross_search: Option<CrossSearchState> }",
        ),
        ("migrated-owner", "s.cross_search.open();"),
        ("migrated-owner", "s.cross_search.close();"),
        ("migrated-owner", "s.cross_search.dismiss();"),
        ("migrated-owner", "s.cross_search.is_open();"),
        ("active", "struct CrossSearchState { pub active: bool }"),
        (
            "migrated-payload-owner",
            "struct CrossSearchState { previous_focus: FocusState }",
        ),
        (
            "migrated-payload-owner",
            "impl CrossSearchState { fn open(&mut self) {} }",
        ),
        (
            "migrated-payload-owner",
            "impl CrossSearchState { fn close(&mut self) {} }",
        ),
        (
            "migrated-payload-owner",
            "impl CrossSearchState { fn dismiss(&mut self) {} }",
        ),
        (
            "migrated-payload-owner",
            "impl CrossSearchState { fn is_open(&self) -> bool { true } }",
        ),
        (
            "overlay",
            "matches!(&s.focus, FocusState::Overlay(OverlayType::CrossSearch));",
        ),
        (
            "overlay",
            "match &s.focus { FocusState::Overlay(OverlayType::CrossSearch) => (), _ => () }",
        ),
        ("render", "cross_search::render(f);"),
        ("render", "cross_search::render_fitted(f, area, payload);"),
    ] {
        for path in ["src/x.rs", "src/adapters/tui/presentation/route.rs"] {
            if rule == "render" && path.starts_with(PRESENTATION) {
                continue;
            }
            let files = [(path.into(), format!("fn real() {{ {text} }}"))];
            let first = scan(&files, &[]);
            assert!(
                first
                    .iter()
                    .any(|error| error.contains(&format!("{rule}|"))),
                "{path}: {text}: {first:?}"
            );
            let sites: Vec<_> = first
                .iter()
                .map(|error| error.split_once(": ").unwrap().1)
                .collect();
            let errors = scan(&files, &[(path, "4-C", sites.as_slice())]);
            assert!(
                errors
                    .iter()
                    .any(|error| !error.starts_with("stale pin")
                        && error.contains(&format!("{rule}|"))),
                "{path}: {text}: {errors:?}"
            );
            assert!(errors.iter().any(|error| error.starts_with("stale pin")));
        }
    }
    let path = "src/domain/models/visual.rs";
    let source = "enum OverlayType {\n CrossSearch,\n}";
    let pins = [(path, "4-C", &["variant|OverlayType|CrossSearch,"][..])];
    let errors = scan(&[(path.into(), source.into())], &pins);
    assert!(
        errors
            .iter()
            .any(|error| error.contains("variant|") && !error.starts_with("stale pin"))
    );
    assert!(errors.iter().any(|error| error.starts_with("stale pin")));
}

#[test]
fn k6_cross_search_payload_edits_and_presentation_widget_data_remain_allowed() {
    for path in ["src/x.rs", "src/adapters/tui/presentation/route.rs"] {
        for text in [
            "s.cross_search.query.push(c);",
            "s.cross_search.selected = Some(0);",
            "s.cross_search.request_revision += 1;",
            "s.cross_search.results.clear();",
            "s.cross_search.running = false;",
            "s.cross_search.reset();",
            "presentation::reducer::open(s, Surface::CrossSearch);",
            "let CrossSearchState { query, selected, request_revision, .. } = payload;",
            "struct CrossSearchState { query: String, selected: Option<usize>, request_revision: u64, running: bool }",
            "impl CrossSearchState { fn reset(&mut self) { self.query.clear(); } }",
        ] {
            assert!(
                scan(&[(path.into(), format!("fn real() {{ {text} }}"))], &[]).is_empty(),
                "{path}: {text}"
            );
        }
    }
    let source = "fn real() { cross_search::render(f); cross_search::render_fitted(f, area, &s.cross_search); }";
    assert!(
        scan(
            &[(
                "src/adapters/tui/presentation/compose.rs".into(),
                source.into()
            )],
            &[]
        )
        .is_empty()
    );
}

#[test]
fn k6_paint_legacy_seam_cannot_return_even_inside_presentation() {
    for path in ["src/x.rs", "src/adapters/tui/presentation/compose.rs"] {
        for source in [
            "fn real() { paint_legacy(f); }",
            "fn real() { cross_search::paint_legacy(f); }",
            "fn real() { use widgets::cross_search::paint_legacy as paint; paint(f); }",
            "fn paint_legacy() {}",
        ] {
            let files = [(path.into(), source.into())];
            let first = scan(&files, &[]);
            assert!(
                first.iter().any(|error| error.contains("legacy-painter|")),
                "{source}"
            );
            let sites: Vec<_> = first
                .iter()
                .map(|error| error.split_once(": ").unwrap().1)
                .collect();
            let errors = scan(&files, &[(path, "4-C", sites.as_slice())]);
            assert!(
                errors
                    .iter()
                    .any(|error| error.contains("legacy-painter|")
                        && !error.starts_with("stale pin"))
            );
            assert!(errors.iter().any(|error| error.starts_with("stale pin")));
        }
        for source in [
            "fn real() { /* paint_legacy(f); */ let _ = \"paint_legacy\"; }",
            "fn real() { let paint_legacy_data = payload; }",
            "#[cfg(test)] fn paint_legacy() {} fn real() {}",
        ] {
            assert!(
                scan(&[(path.into(), source.into())], &[]).is_empty(),
                "{path}: {source}"
            );
        }
    }
}

#[test]
fn k6_bb2a_fields_are_typed_and_tail_helpers_cannot_return() {
    for (kind, field) in [
        ("CommandPaletteState", "filter_text"),
        ("SearchState", "query"),
        ("ReverseSearchState", "query"),
    ] {
        let old = format!("struct {kind} {{ {field}: String }}");
        let site = format!("field-string|{kind}|{old}");
        let sites = [site.as_str()];
        let pins = [("src/field.rs", "4-C", sites.as_slice())];
        let errors = scan(&[("src/field.rs".into(), old.clone())], &pins);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("field-string|") && !error.starts_with("stale pin")),
            "{kind}: {errors:?}"
        );
        assert!(errors.iter().any(|error| error.starts_with("stale pin")));
        assert!(
            scan(
                &[(
                    "src/field.rs".into(),
                    old.replace(": String", ": TextField")
                )],
                &[]
            )
            .is_empty()
        );
    }
    for field in [
        "command_palette.filter_text",
        "search_state.query",
        "reverse_search.query",
    ] {
        for method in [
            "push",
            "push_str",
            "pop",
            "truncate",
            "drain",
            "replace_range",
            "insert_str",
        ] {
            let source = format!("fn old() {{ state.{field}.{method}(x); }}");
            assert!(
                scan(&[("src/field.rs".into(), source)], &[])
                    .iter()
                    .any(|error| error.contains("field-tail|")),
                "{field}.{method}"
            );
        }
    }
    for source in [
        "fn palette_edit() {}",
        "fn search_edit() {}",
        "fn old() { let x = InputAction::SearchClear; }",
        "fn old() { let x = OverlayCmd::PaletteBackspace; }",
        "fn old() { let x = OverlayCmd::RevBackspace; }",
    ] {
        assert!(
            scan(&[("src/field.rs".into(), source.into())], &[])
                .iter()
                .any(|error| error.contains("field-tail|")),
            "{source}"
        );
    }
    // Independent negative control: CrossSearch's String editor is allocated to 2b.
    let legacy = "struct CrossSearchState { query: String }
        fn cross_search_edit() { state.cross_search.query.push(x); }";
    assert!(scan(&[("src/field.rs".into(), legacy.into())], &[]).is_empty());
}
