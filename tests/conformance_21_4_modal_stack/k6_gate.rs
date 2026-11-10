//! K6 / A19: final 4-C ratchet. Behaviour belongs to the keystones, not this scanner.
//! Every retained occurrence has an exact named-site pin and a migration owner.
//! Removed/changed pins fail stale; new hits never inherit a file exemption.
//! Story 21-5 extends the same production scan to retired tool-block inspector identifiers.
use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const PRESENTATION: &str = "src/adapters/tui/presentation/";
const HOSTS: &[(&str, &str)] = &[
    ("src/infrastructure/runtime/event_loop.rs", "render"),
    (
        "src/infrastructure/runtime/attach_loop.rs",
        "paint_attached",
    ),
];
const TEST_FILES: &[&str] = &[
    "src/adapters/auth/flows/api_key/tests.rs",
    "src/adapters/daemon/approval_lifecycle_tests.rs",
    "src/adapters/daemon/server/turn_interrupt/tests.rs",
    "src/adapters/responses/tests.rs",
    "src/adapters/tui/auth_outcome_tests.rs",
    "src/adapters/tui/decision_queue/cards_tests.rs",
    "src/adapters/tui/story21_2_slash_tests.rs",
    "src/infrastructure/runtime/attach_loop/inspect_hint_tests.rs",
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
    code_only_with(source, false)
}

/// `keep_strings` leaves string and raw-string literals readable (comments and
/// char literals stay masked): the OSC 8 introducer lives only in a literal.
fn code_only_with(source: &str, keep_strings: bool) -> String {
    let mut b = source.as_bytes().to_vec();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let mut literal = false;
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
            literal = true;
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
            literal = true;
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
        if !(keep_strings && literal) {
            blank(&mut b[start..i]);
        }
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
/// Production text with comments and test items masked but string literals kept.
fn production_literal(source: &str) -> String {
    let (_, ranges) = production_ranges(source);
    let mut bytes = code_only_with(source, true).into_bytes();
    for (start, end) in ranges {
        blank(&mut bytes[start..end]);
    }
    String::from_utf8(bytes).unwrap()
}
fn production_part(source: &str) -> String {
    production_ranges(source).0
}
/// The masked production text and the byte ranges of its `#[cfg(test)]` items.
fn production_ranges(source: &str) -> (String, Vec<(usize, usize)>) {
    let mut code = code_only(source);
    let mut ranges = Vec::new();
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
        ranges.push((start, cursor));
        code = String::from_utf8(bytes).unwrap();
    }
    (code, ranges)
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

// No pin remains: Story 21-8 slice 8-A removed the last recipient-surface prior-focus slots
// (the thread window never owns or restores base focus). Every new hit fails.
const PINS: &[Pin<'_>] = &[];

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
        r###"\b(?:notification_log|quit_confirm|help_overlay|command_palette|which_key_bar|decision_frame|autocomplete_popup|model_selector|profile_switcher|usage_panel|login_dialog|wave_overlay|stage_search|search_bar|cross_search|bookmark_list|reverse_search|tool_inspector|block_viewer)\s*::\s*render\w*\s*\("###,
    ),
    // Payload presence cannot become a second open authority, regardless of read syntax.
    ("option", r"\.\s*(?:notification_log|quit_confirm)\b"),
    (
        "interceptor",
        r"\b(?:notification_log|quit_confirm|decision_queue)\s*::\s*(?:handle_\w+|execute)\s*\(",
    ),
    // Story 21.6 slice 6-A (A20): the interim pane, its bottom-anchored placement and the
    // singleton slots its kinds used are gone. Field/type/fn accesses, never module paths:
    // `decision_queue::` and `widgets::decision_frame::` line builders stay legal.
    (
        "decision-21-6-a",
        r"\b(?:decision_pane|DecisionPane|StageBottom|pending_delete|pending_large_image)\b",
    ),
    // Story 21.6 slice 6-B (A20): retired permission and consent ownership.
    // The reused permission_prompt widget and advance_deferred_permission stay legal.
    (
        "decision-21-6-b",
        r"\b(?:pending_permission|permission_queue|PermissionQueue|pending_feedback_input|permission_reader|PermissionReaderState|FeedbackInputState|PendingPermission|AttachedApprovals|PendingConsentApproval|permission_modal_owns_focus)\b|\bConfirmationType\s*::\s*(?:Permission|PermissionFeedback)\b|\bapproval_bridge\s*::\s*advance\b",
    ),
    // Story 21.6 slice 6-C (A20): the inline question, skill-trust, plan-family, delegation,
    // spawn-gate and agent-kill slots, their focus-mirror variants and the trust-queue
    // advance are gone. Field/type/fn accesses, never module paths:
    // `widgets::ask_user_question::` line builders and `decision_queue::` stay legal.
    (
        "decision-21-6-c",
        r"\.\s*ask_user_question\b|\btask_panel_state\s*\.\s*pending_deviation\b|\b(?:question_response_tx|pending_skill_trust|skill_trust_queue|skill_trust_inspect_mode|pending_activation(?:_inspect_content)?|pending_plan_approval|pending_plan_card|skip_cascade_pending|cancel_plan_confirm|pending_delegation_card|pending_spawn_gate|pending_kill_confirm|advance_skill_trust_queue)\b|\bConfirmationType\s*::\s*(?:Question|SkillTrust|SkillTrustInspect|PlanApproval)\b",
    ),
    // Story 21.6 slice 6-D (A20): the legacy focus mirror and card slots are gone.
    // Match declarations/accesses, not reusable queue or widget module paths.
    (
        "decision-21-6-d",
        r"\bFocusState\s*::\s*Overlay\b|\b(?:OverlayType|ConfirmationType|AttachedPresentation|InlineCard)\b|\b(?:mod|use)\s+attached_presentation\b|\b(?:raise_confirmation|settle_legacy|settle_legacy_inner|restore_focus_after_modal|take_chat_focus_for_card|settle_borrowed_card_focus|card_focus_blocked|current_confirmation_pending|inline_card|card_context|card_special_key)\s*\(|\bfn\s+confirmation_pending\b|\breducer\s*::\s*confirmation_pending\b|\.\s*(?:card_focus_borrowed_from|pending_confirmation|pending_consolidation_card|pending_forget_card|pending_artifact_card|pending_peer_add|pending_team_retract|pending_fork_index|pending_rewind_index|rewind_preview|pending_export|retract_confirm|consolidation_token)\b|\b(?:card_focus_borrowed_from|pending_confirmation|pending_consolidation_card|pending_forget_card|pending_artifact_card|pending_peer_add|pending_team_retract|pending_fork_index|pending_rewind_index|rewind_preview|pending_export|retract_confirm|consolidation_token)\s*:",
    ),
    // Story 21.6 slice 6-E (A20, final): the proposal card slots, the attached pending peer
    // card, the bottom-anchored card painter and the shared Card/Cue contexts are gone.
    // Field/type/fn accesses, never module paths: `widgets::team_proposal_card::` body
    // builders and `decision_queue::` stay legal.
    (
        "decision-21-6-e",
        r"\b(?:pending_team_proposals|team_proposal_visible|team_proposal_reviews|team_proposal_scroll(?:_max)?|team_proposal_footer_visible|team_proposal_viewport_rows|pending_peer_card|peer_response_card_lines|render_bottom_anchored_decision_card|proposal_higher_confirmation_active|visible_team_proposal|card_cue)\b|\bContext\s*::\s*Card\b|\bCardCmd\s*::\s*Cue\b",
    ),
    // Story 21.8 slice 8-A (A17): the recipient surface, its trap, its direct paint and its
    // prior-focus slot are gone; the S-THREAD frame replaces them. Field/type/fn accesses and
    // the retired action literals, never module paths. Scanned with string literals kept
    // (`[c] Use as context` lives only in a literal); `recipient_surface_flash` is a longer
    // identifier and stays out of scope.
    (
        "thread-21-8-a",
        r"\bContext\s*::\s*RecipientSurface\b|\bOverlayCmd\s*::\s*Recipient\w*|\b(?:recipient_higher_confirmation_active|attached_higher_confirmation|recipient_surface_key|dispatch_recipient_surface_key|render_recipient_surface|recipient_surface_ignores_raw_key|exec_recipient|recipient_blocked)\b|\bRecipientSurface(?:Target|Access|Action)?\b|\brecipient_surface\b|\[c\] Use as context|\[c\] Stop using as context",
    ),
    // Slice 8-B removes the old outcome blocks and the pinned-node composer.
    (
        "thread-21-8-b",
        r#"\bViewKind\s*::\s*RecipientOutcome\b|"recipient-actions"|"recipient-view"|input already contains text|\b(?:PinnedPeerCompose|pinned_peer_compose|take_current_peer_compose|PeerComposeAction)\b|\bRecipientActionState\s*::\s*Completed\b"#,
    ),
    // Slice 8-C retires transcript thread/log/retract surfaces and their old renderer.
    (
        "thread-21-8-c",
        r#"\b(?:show_team_thread|TEAM_THREAD_BLOCK_ID|view_block|TEAM_LOG_BLOCK_ID|MAX_INCHAT_ROWS|run_team_log)\b|\bViewKind\s*::\s*(?:TeamThread|PeerRetract|TeamLog)\b|"peer-retract"|\brender_team_send_rows\s*\("#,
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
    // Story 21.13 D17: one OSC 8 encoder, no inline block expansion, and the
    // natural-width Window presentation only where blocks are laid out.
    ("osc8-encoder", r"\]8;"),
    (
        "block-inline-expansion",
        r"\b(?:expanded_code|code_expanded|expand_block|block_expanded)\b",
    ),
    ("block-window", r"\bBlockPresentation\s*::\s*Window\b"),
    (
        "tool-inspector-legacy",
        r"\b(?:ToolBlockState|tool_block_states|tool_block_states_version|peek_active|render_peek_overlay|PeekToolBlock|ToggleToolBlock|expanded_body_height|expanded_diff_body|TOGGLE_TOOL_BLOCK)\b",
    ),
    // Story 21.9 D19: the old tool-line shape (quoted summary, `→ running...`,
    // `→ ✓`, closing ` ┄`) and the two-line welcome widget are gone.
    ("tool-line-quoted-summary", r###"" \\"\{\}\\"""###),
    ("tool-line-arrow-running", r###"→ running"###),
    ("tool-line-arrow-done", r###"" +→ +""###),
    ("tool-line-closing-border", r###"" ┄""###),
    (
        "welcome-widget",
        r"\bempty_state\s*::|\bpub\s+mod\s+empty_state\b|\bwidgets\s*::\s*empty_state\b",
    ),
    (
        "field-string",
        r"\b(?:query|filter_text)\s*:\s*(?:std\s*::\s*string\s*::\s*)?String\b",
    ),
    (
        "field-tail",
        r"\.\s*(?:command_palette\s*\.\s*filter_text|(?:search_state|reverse_search|cross_search)\s*\.\s*query|(?:help_overlay|notification_log)\s*\.\s*search\s*\.\s*query)\s*\.\s*(?:push|push_str|pop|truncate|drain|replace_range|insert_str)\s*\(|\bfn\s+(?:palette_edit|search_edit|cross_search_edit)\b|\b(?:SearchClear|PaletteBackspace|RevBackspace|CrossBackspace)\b",
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
            r"^(?:notification_log|quit_confirm|help_overlay|command_palette|which_key_bar|decision_frame|autocomplete_popup|model_selector|profile_switcher|usage_panel|login_dialog|stage_search|reverse_search|cross_search|bookmark_list|wave_overlay|tool_inspector|block_viewer)\b",
        )
    });
    static TOOL_OPEN_ARM: LazyLock<Regex> =
        LazyLock::new(|| re(r"\bInputAction\s*::\s*InspectTool\s*=>\s*\{"));
    static TOOL_OPEN_ONLY: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r"(?s)^\{\s*(?:crate\s*::\s*adapters\s*::\s*tui\s*::\s*)?(?:tool_inspector|block_viewer)\s*::\s*inspect_focused\s*\([^;{}]*\)\s*;\s*\}$",
        )
    });
    static TOOL_REFRESH: LazyLock<Regex> =
        LazyLock::new(|| re(r"\btool_inspector\s*::\s*refresh\s*\("));
    let has_tool_owner = files
        .iter()
        .any(|(path, _)| path == "src/adapters/tui/tool_inspector/mod.rs");
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
        let literal = production_literal(source);
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
            if *rule == "block-window"
                && (path.starts_with("src/adapters/tui/markdown/")
                    || path == "src/adapters/tui/presentation/markdown_body.rs")
            {
                continue;
            }
            if *rule == "osc8-encoder" && path == "src/adapters/tui/widgets/hyperlink.rs" {
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
            if rule.starts_with("tool-line-") && !path.starts_with("src/adapters/tui/widgets/") {
                continue;
            }
            let haystack = if *rule == "osc8-encoder"
                || *rule == "thread-21-8-a"
                || *rule == "thread-21-8-b"
                || *rule == "thread-21-8-c"
                || rule.starts_with("tool-line-")
            {
                &literal
            } else {
                &code
            };
            for m in pattern.find_iter(haystack) {
                // The query moved to the queue; its implementation remains legal.
                if *rule == "thread-21-8-c"
                    && m.as_str().starts_with("render_team_send_rows")
                    && (path.ends_with("/team_command.rs") || path.ends_with("/team_send_cli.rs"))
                {
                    continue;
                }
                if *rule == "decision-21-6-d"
                    && path == "src/adapters/tui/decision_queue/retract.rs"
                    && m.as_str().starts_with("fn confirmation_pending")
                {
                    continue;
                }
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
                        "AutocompleteState" | "DecisionQueue"
                    )
                {
                    continue;
                }
                if *rule == "field-string"
                    && !matches!(
                        scope(&code, m.start()).as_str(),
                        "CommandPaletteState"
                            | "SearchState"
                            | "ReverseSearchState"
                            | "CrossSearchState"
                            | "InspectorSearch"
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
                    "osc8-encoder",
                    "block-inline-expansion",
                    "block-window",
                    "tool-inspector-legacy",
                    "tool-line-quoted-summary",
                    "tool-line-arrow-running",
                    "tool-line-arrow-done",
                    "tool-line-closing-border",
                    "welcome-widget",
                    "field-string",
                    "field-tail",
                    "decision-21-6-a",
                    "decision-21-6-b",
                    "decision-21-6-c",
                    "decision-21-6-d",
                    "decision-21-6-e",
                    "thread-21-8-a",
                    "thread-21-8-b",
                    "thread-21-8-c",
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
            if has_tool_owner && path == HOSTS[0].0 {
                let arms: Vec<_> = TOOL_OPEN_ARM.find_iter(&code).collect();
                if arms.len() != 1 {
                    errors.push(format!(
                        "{path}: tool-inspector-host needs one InspectTool arm"
                    ));
                } else {
                    let open = arms[0].end() - 1;
                    let arm = &code[open..item_end(&code, open)];
                    if !TOOL_OPEN_ONLY.is_match(arm) {
                        errors.push(format!("{path}: tool-inspector-host InspectTool must be one inspect_focused call"));
                    }
                }
                for (name, pattern, callee) in [
                    (
                        "StepTool",
                        r"\bInputAction\s*::\s*StepTool\s*\{\s*forward\s*,?\s*\}\s*=>\s*\{",
                        r"step",
                    ),
                    (
                        "CopyToolSource",
                        r"\bInputAction\s*::\s*CopyToolSource\s*=>\s*\{",
                        r"copy\s*::\s*copy_source",
                    ),
                ] {
                    let pattern = re(pattern);
                    let arms: Vec<_> = pattern.find_iter(&code).collect();
                    let only = re(&format!(
                        r"(?s)^\{{\s*(?:crate\s*::\s*adapters\s*::\s*tui\s*::\s*)?tool_inspector\s*::\s*{callee}\s*\([^;{{}}]*\)\s*;\s*\}}$"
                    ));
                    if arms.len() != 1
                        || !only.is_match({
                            let open = arms.first().map_or(0, |arm| arm.end() - 1);
                            &code[open..item_end(&code, open)]
                        })
                    {
                        errors.push(format!(
                            "{path}: tool-inspector-host {name} must be one owned call"
                        ));
                    }
                }
                if TOOL_REFRESH.find_iter(body).count() != 1
                    || TOOL_REFRESH.find_iter(&code).count() != 1
                {
                    errors.push(format!(
                        "{path}: tool-inspector-host render must refresh exactly once"
                    ));
                }
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
    assert_eq!(
        files
            .iter()
            .filter(|(path, _)| path == "src/adapters/tui/tool_inspector/mod.rs")
            .count(),
        1,
        "the live tree must contain its tool payload owner"
    );
    for module in [
        "mod",
        "stack",
        "reducer",
        "route",
        "compose",
        "geometry",
        "stage_search",
        "occlusion",
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
            .filter(|(_, owner, _)| matches!(*owner, "4-B" | "4-C"))
            .map(|(_, _, sites)| sites.len())
            .sum::<usize>(),
        0,
        "4-C removes the final 109 slice pins; no 21-4 exemption remains"
    );
    assert_eq!(PINS.len(), 0, "21-8 slice 8-A leaves no pin");
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
        ("interceptor", "decision_queue::handle_key(s, k, f);"),
        ("render", "decision_frame::render(f);"),
        ("decision-21-6-a", "decision_pane::raise(s, d, a);"),
        ("decision-21-6-a", "let t = Surface::DecisionPane;"),
        ("decision-21-6-a", "let p = Placement::StageBottom;"),
        ("decision-21-6-a", "s.pending_large_image = Some(image);"),
        ("decision-21-6-a", "let t = s.pending_delete.take();"),
        ("thread-21-8-a", "let c = Context::RecipientSurface;"),
        ("thread-21-8-a", "let c = OverlayCmd::RecipientPageUp;"),
        ("thread-21-8-a", "let c = OverlayCmd::RecipientToggle;"),
        (
            "thread-21-8-a",
            "if recipient_higher_confirmation_active(s) {}",
        ),
        ("thread-21-8-a", "if attached_higher_confirmation(s) {}"),
        ("thread-21-8-a", "recipient_surface_key(k, w, c, s, t);"),
        (
            "thread-21-8-a",
            "dispatch_recipient_surface_key(k, w, c, s, t);",
        ),
        ("thread-21-8-a", "render_recipient_surface(f, r, s, c);"),
        (
            "thread-21-8-a",
            "if recipient_surface_ignores_raw_key(s, e) {}",
        ),
        ("thread-21-8-a", "exec_recipient(s, c, k);"),
        ("thread-21-8-a", "let s: RecipientSurface = make();"),
        ("thread-21-8-a", "let t = RecipientSurfaceTarget::Item(a);"),
        ("thread-21-8-a", "let a = RecipientSurfaceAccess::Writer;"),
        ("thread-21-8-a", "let a = RecipientSurfaceAction::Respond;"),
        ("thread-21-8-a", "let r = s.recipient_surface.take();"),
        ("thread-21-8-a", "recipient_surface::render(f, r, s, c);"),
        ("thread-21-8-a", "let b = recipient_blocked(s);"),
        ("thread-21-8-a", "let t = \"[c] Use as context\";"),
        ("thread-21-8-a", "let t = \"[c] Stop using as context\";"),
        ("thread-21-8-b", "let v = ViewKind::RecipientOutcome;"),
        ("thread-21-8-b", "let id = \"recipient-actions\";"),
        ("thread-21-8-b", "let id = \"recipient-view\";"),
        (
            "thread-21-8-b",
            "let message = \"input already contains text\";",
        ),
        ("thread-21-8-b", "let p: PinnedPeerCompose = make();"),
        ("thread-21-8-b", "let p = s.pinned_peer_compose.take();"),
        ("thread-21-8-b", "take_current_peer_compose(s);"),
        ("thread-21-8-b", "let a = PeerComposeAction::Send;"),
        ("thread-21-8-b", "let a = RecipientActionState::Completed;"),
        ("thread-21-8-c", "show_team_thread(s);"),
        ("thread-21-8-c", "let _ = TEAM_THREAD_BLOCK_ID;"),
        ("thread-21-8-c", "let _ = ViewKind::TeamThread;"),
        ("thread-21-8-c", "let _ = ViewKind::PeerRetract;"),
        ("thread-21-8-c", "let _ = \"peer-retract\";"),
        ("thread-21-8-c", "s.view_block = None;"),
        ("thread-21-8-c", "let _ = ViewKind::TeamLog;"),
        ("thread-21-8-c", "let _ = TEAM_LOG_BLOCK_ID;"),
        ("thread-21-8-c", "let _ = MAX_INCHAT_ROWS;"),
        ("thread-21-8-c", "run_team_log(s);"),
        ("thread-21-8-c", "render_team_send_rows(rows);"),
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
        "struct DecisionQueue { focused: Option<Key> }",
        "decision_frame::render(f);",
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
    // A 4-C focus-write pin cannot revive the retired confirmation mirror.
    let source = "fn real() { s.focus = FocusState::Overlay(OverlayType::Confirmation(kind)); }";
    let pins = [(
        "src/x.rs",
        "4-C",
        &["focus-write|real|s.focus = FocusState::Overlay(OverlayType::Confirmation(kind));"][..],
    )];
    assert!(
        scan(&[("src/x.rs".into(), source.into())], &pins)
            .iter()
            .any(|error| error.contains("decision-21-6-d|"))
    );
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
    let legacy = "fn real(s: &mut TuiState) { s.focus = next; }";
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
            "s.cross_search.query.insert(\"c\");",
            "s.cross_search.selected = Some(0);",
            "s.cross_search.request_revision += 1;",
            "s.cross_search.results.clear();",
            "s.cross_search.running = false;",
            "s.cross_search.reset();",
            "presentation::reducer::open(s, Surface::CrossSearch);",
            "let CrossSearchState { query, selected, request_revision, .. } = payload;",
            "struct CrossSearchState { query: TextField, selected: Option<usize>, request_revision: u64, running: bool }",
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
fn k6_bb2_fields_are_typed_and_tail_helpers_cannot_return() {
    for (kind, field) in [
        ("CommandPaletteState", "filter_text"),
        ("SearchState", "query"),
        ("ReverseSearchState", "query"),
        ("CrossSearchState", "query"),
        ("InspectorSearch", "query"),
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
        "cross_search.query",
        "help_overlay.search.query",
        "notification_log.search.query",
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
        "fn cross_search_edit() {}",
        "fn old() { let x = InputAction::SearchClear; }",
        "fn old() { let x = OverlayCmd::PaletteBackspace; }",
        "fn old() { let x = OverlayCmd::RevBackspace; }",
        "fn old() { let x = OverlayCmd::CrossBackspace; }",
    ] {
        assert!(
            scan(&[("src/field.rs".into(), source.into())], &[])
                .iter()
                .any(|error| error.contains("field-tail|")),
            "{source}"
        );
    }
    // Independent negative control: genuinely unmigrated editors remain allowed.
    let legacy = "struct OtherEditor { query: String }
        fn edit_field() { state.model_selector.search_query.push(x); }";
    assert!(scan(&[("src/field.rs".into(), legacy.into())], &[]).is_empty());
}

const LEGACY_TOOL_INSPECTOR_IDENTIFIERS: &[&str] = &[
    "ToolBlockState",
    "tool_block_states",
    "tool_block_states_version",
    "peek_active",
    "render_peek_overlay",
    "PeekToolBlock",
    "ToggleToolBlock",
    "expanded_body_height",
    "expanded_diff_body",
    "TOGGLE_TOOL_BLOCK",
];

#[test]
fn k6_story21_5_each_legacy_tool_identifier_is_unpinnable_in_production() {
    for token in LEGACY_TOOL_INSPECTOR_IDENTIFIERS {
        for path in [
            "src/x.rs",
            "src/adapters/tui/state.rs",
            "src/adapters/tui/notification_log.rs",
            "src/adapters/tui/quit_confirm.rs",
            "src/adapters/tui/presentation/compose.rs",
        ] {
            let files = [(path.into(), format!("fn real() {{ let _ = {token}; }}"))];
            let first = scan(&files, &[]);
            assert_eq!(first.len(), 1, "{path}: {token}: {first:?}");
            assert!(
                first[0].contains("tool-inspector-legacy|real|"),
                "{path}: {token}: {first:?}"
            );
            let sites: Vec<_> = first
                .iter()
                .map(|error| error.split_once(": ").unwrap().1)
                .collect();
            for owner in ["21-6", "21-8"] {
                let errors = scan(&files, &[(path, owner, sites.as_slice())]);
                assert!(
                    errors.iter().any(|error| {
                        !error.starts_with("stale pin")
                            && error.contains("tool-inspector-legacy|real|")
                    }),
                    "{path}: {token}: {owner}: {errors:?}"
                );
                assert!(
                    errors.iter().any(|error| error.starts_with("stale pin")),
                    "{path}: {token}: {owner}: {errors:?}"
                );
            }
        }
    }
}

#[test]
fn k6_story21_5_legacy_tool_copies_are_masked_and_identifier_boundaries_are_exact() {
    for token in LEGACY_TOOL_INSPECTOR_IDENTIFIERS {
        for source in [
            format!("// {token}\nfn real() {{}}"),
            format!("/* outer /* {token} */ comment */ fn real() {{}}"),
            format!("fn real() {{ let _ = \"escaped \\\" {token}\"; }}"),
            format!("fn real() {{ let _ = r##\"{token}\"##; }}"),
            format!("#[cfg(test)] fn test_only() {{ let _ = {token}; }}\nfn real() {{}}"),
            format!(
                "#[cfg(test)] mod tests {{ fn test_only() {{ let _ = {token}; }} }}\nfn real() {{}}"
            ),
            format!("#[cfg(test)] use crate::{{{token}}};\nfn real() {{}}"),
            format!("fn real() {{ let _ = prefix_{token}; let _ = {token}_suffix; }}"),
        ] {
            for path in ["src/x.rs", "src/adapters/tui/presentation/compose.rs"] {
                let errors = scan(&[(path.into(), source.clone())], &[]);
                assert!(errors.is_empty(), "{path}: {source}: {errors:?}");
                let errors = scan(
                    &[(
                        path.into(),
                        format!("{source}\nfn later() {{ let _ = {token}; }}"),
                    )],
                    &[],
                );
                assert_eq!(errors.len(), 1, "{path}: {source}: {errors:?}");
                assert!(
                    errors[0].contains("tool-inspector-legacy|later|"),
                    "{path}: {source}: {errors:?}"
                );
            }
        }
    }
}

#[test]
fn k6_story21_5_host_calls_are_single_seams_and_the_painter_has_one_owner() {
    let host = "fn dispatch() { match action { InputAction::InspectTool => { tool_inspector::inspect_focused(state, conversation, None); }, InputAction::StepTool { forward } => { tool_inspector::step(state, conversation, None, forward); }, InputAction::CopyToolSource => { tool_inspector::copy::copy_source(state, conversation, None, copier); }, _ => {} } } fn render() { tool_inspector::refresh(state, conversation, None, clock, 1); presentation::compose(frame); }";
    let files = |source: &str| {
        vec![
            (HOSTS[0].0.into(), source.into()),
            (
                "src/adapters/tui/tool_inspector/mod.rs".into(),
                "fn owner() {}".into(),
            ),
        ]
    };
    assert!(scan(&files(host), &[]).is_empty());
    let refresh = "tool_inspector::refresh(state, conversation, None, clock, 1);";
    let open = "tool_inspector::inspect_focused(state, conversation, None);";
    for bad in [
        host.replace(open, &format!("{open} state.needs_redraw = true;")),
        host.replace(open, &format!("{open} {open}")),
        host.replace("InputAction::InspectTool =>", "InputAction::Other =>"),
        host.replace(refresh, ""),
        format!("{host} fn stray() {{ {refresh} }}"),
        format!("{} fn stray() {{ {refresh} }}", host.replace(refresh, "")),
    ] {
        assert!(
            scan(&files(&bad), &[])
                .iter()
                .any(|error| error.contains("tool-inspector-host"))
        );
    }
    assert!(
        !scan(
            &[(
                "src/side_door.rs".into(),
                "fn draw() { tool_inspector::render(frame, area, state, theme, title); }".into(),
            )],
            &[]
        )
        .is_empty()
    );
}

#[test]
fn k6_story21_5_step_and_copy_host_arms_are_single_owned_calls() {
    let host = "fn dispatch() { match action { InputAction::InspectTool => { tool_inspector::inspect_focused(state, conversation, None); }, InputAction::StepTool { forward } => { tool_inspector::step(state, conversation, None, forward); }, InputAction::CopyToolSource => { tool_inspector::copy::copy_source(state, conversation, None, copier); }, _ => {} } } fn render() { tool_inspector::refresh(state, conversation, None, clock, 1); presentation::compose(frame); }";
    let files = |source: &str| {
        vec![
            (HOSTS[0].0.into(), source.into()),
            (
                "src/adapters/tui/tool_inspector/mod.rs".into(),
                "fn owner() {}".into(),
            ),
        ]
    };
    assert!(
        scan(&files(host), &[]).is_empty(),
        "complete host is the positive control"
    );
    for (call, action) in [
        (
            "tool_inspector::step(state, conversation, None, forward);",
            "InputAction::StepTool",
        ),
        (
            "tool_inspector::copy::copy_source(state, conversation, None, copier);",
            "InputAction::CopyToolSource",
        ),
    ] {
        for bad in [
            host.replace(call, ""),
            host.replace(call, &format!("{call} {call}")),
            host.replace(call, &format!("{call} state.needs_redraw = true;")),
            host.replace(action, "InputAction::Other"),
            host.replace(
                call,
                "handlers::clipboard::requested(state, conversation, String::new(), copier);",
            ),
        ] {
            assert!(
                scan(&files(&bad), &[])
                    .iter()
                    .any(|error| error.contains("tool-inspector-host")),
                "scanner accepted an unowned or non-single-call arm: {bad}"
            );
        }
    }
}

#[test]
fn k6_story21_13_block_cutover_clauses_have_positive_controls_and_exact_allowlists() {
    for (rule, text, allowed, denied) in [
        (
            "osc8-encoder",
            "fn f(url: &str) -> String { format!(\"\\x1b]8;;{url}\\x07\") }\n// \\x1b]8;; in a comment is masked\nfn g() { let _ = ']'; }",
            "src/adapters/tui/widgets/hyperlink.rs",
            "src/adapters/tui/widgets/block_viewer.rs",
        ),
        (
            "block-inline-expansion",
            "fn f() { let _ = expand_block; }",
            "",
            "src/adapters/tui/widgets/block_viewer.rs",
        ),
        (
            "block-window",
            "fn f() { let _ = BlockPresentation::Window; }",
            "src/adapters/tui/markdown/layout.rs",
            "src/adapters/tui/widgets/chat_pane/mod.rs",
        ),
        (
            "block-window",
            "fn f() { let _ = BlockPresentation::Window; }",
            "src/adapters/tui/presentation/markdown_body.rs",
            "src/adapters/tui/widgets/chat_pane/mod.rs",
        ),
    ] {
        let text = text.to_owned();
        let errors = scan(&[(denied.into(), text.clone())], &[]);
        assert!(
            errors.iter().any(|e| e.contains(&format!("{rule}|"))),
            "{rule}: {errors:?}"
        );
        if !allowed.is_empty() {
            let errors = scan(&[(allowed.into(), text.clone())], &[]);
            assert!(
                !errors.iter().any(|e| e.contains(&format!("{rule}|"))),
                "{errors:?}"
            );
        }
    }
    // Stale-exemption check: the hyperlink.rs allowance is exercised by the real
    // encoder, which the same scanner flags anywhere else; and a comment alone
    // never trips the rule.
    let encoder = include_str!("../../src/adapters/tui/widgets/hyperlink.rs");
    let elsewhere = scan(
        &[(
            "src/adapters/tui/widgets/block_viewer.rs".into(),
            encoder.into(),
        )],
        &[],
    );
    assert!(
        elsewhere.iter().any(|e| e.contains("osc8-encoder|")),
        "{elsewhere:?}"
    );
    // The markdown_body.rs allowance is exercised by its real Window layout: the
    // same scanner flags that text anywhere else, and not under its own path.
    let body = include_str!("../../src/adapters/tui/presentation/markdown_body.rs");
    let flagged = |path: &str| {
        scan(&[(path.into(), body.into())], &[])
            .iter()
            .any(|e| e.contains("block-window|"))
    };
    assert!(flagged("src/adapters/tui/widgets/chat_pane/mod.rs"));
    assert!(!flagged("src/adapters/tui/presentation/markdown_body.rs"));
    let here = scan(
        &[(
            "src/adapters/tui/widgets/hyperlink.rs".into(),
            encoder.into(),
        )],
        &[],
    );
    assert!(
        !here.iter().any(|e| e.contains("osc8-encoder|")),
        "{here:?}"
    );
    let comment = scan(
        &[(
            "src/x.rs".into(),
            "// \\x1b]8;; only a comment\nfn f() {}".into(),
        )],
        &[],
    );
    assert!(
        !comment.iter().any(|e| e.contains("osc8-encoder|")),
        "{comment:?}"
    );
    // Only the two layouts that build Window geometry are exempt: the S-BLOCK
    // widget never names it, so naming it there (or in any sibling) is a hit.
    for denied in [
        "src/adapters/tui/widgets/block_viewer.rs",
        "src/adapters/tui/presentation/viewer.rs",
        "src/adapters/tui/block_viewer/mod.rs",
    ] {
        let errors = scan(
            &[(
                denied.into(),
                "fn f() { let _ = BlockPresentation::Window; }".into(),
            )],
            &[],
        );
        assert!(
            errors.iter().any(|e| e.contains("block-window|")),
            "{denied}: {errors:?}"
        );
    }
}

/// Story 21.9 D19: the retired tool-line format and the two-line welcome widget
/// cannot come back in the widget tree. Each clause has a synthetic positive
/// control; no clause carries an exemption (all are hard), so there is no
/// stale-exemption entry to keep.
#[test]
fn k6_story21_9_old_tool_line_and_welcome_clauses_have_positive_controls() {
    let owner = "src/adapters/tui/widgets/tool_block.rs";
    for (rule, text) in [
        (
            "tool-line-quoted-summary",
            "fn f(s: &str) -> String { format!(\" \\\"{}\\\"\", s) }",
        ),
        (
            "tool-line-arrow-running",
            "fn f(e: &str) -> String { format!(\" → running... ({})\", e) }",
        ),
        ("tool-line-arrow-done", "fn f() -> &'static str { \" → \" }"),
        (
            "tool-line-closing-border",
            "fn f() -> &'static str { \" ┄\" }",
        ),
        (
            "welcome-widget",
            "use super::empty_state;\nfn f() { empty_state::render(); }",
        ),
    ] {
        let errors = scan(&[(owner.into(), text.into())], &[]);
        assert!(
            errors.iter().any(|e| e.contains(&format!("{rule}|"))),
            "{rule}: {errors:?}"
        );
        // The same text in a comment is masked, never a hit.
        let commented = format!("// {}\nfn g() {{}}", text.replace('\n', " "));
        assert!(
            !scan(&[(owner.into(), commented)], &[])
                .iter()
                .any(|e| e.contains(&format!("{rule}|"))),
            "{rule}: comments are masked"
        );
    }
    // The tool-line clauses are scoped to the widget tree; the welcome clause is
    // crate-wide.
    for text in [
        "fn f() -> &'static str { \" → \" }",
        "fn f() -> &'static str { \" ┄\" }",
    ] {
        assert!(
            !scan(&[("src/domain/x.rs".into(), text.into())], &[])
                .iter()
                .any(|e| e.contains("tool-line-")),
            "{text}: outside the widget tree"
        );
    }
    // Stale-exemption control: these clauses are hard, so no pin may exempt
    // them; a pin naming one would be a stale or smuggled exemption.
    for (path, owner, sites) in PINS {
        for site in *sites {
            assert!(
                !site.starts_with("tool-line-") && !site.starts_with("welcome-widget|"),
                "{path} ({owner}): {site}"
            );
        }
    }
    // The welcome clause is crate-wide: it trips in any production path.
    for path in [
        "src/domain/x.rs",
        "src/infrastructure/runtime/event_loop.rs",
    ] {
        let errors = scan(
            &[(path.into(), "fn f() { empty_state::render(); }".into())],
            &[],
        );
        assert!(
            errors.iter().any(|e| e.contains("welcome-widget|")),
            "{path}: {errors:?}"
        );
    }
    // Controls: the shipped humanized builder trips none of them.
    let line = include_str!("../../src/adapters/tui/widgets/tool_line.rs");
    let shipped = scan(
        &[("src/adapters/tui/widgets/tool_line.rs".into(), line.into())],
        &[],
    );
    assert!(
        !shipped
            .iter()
            .any(|e| e.contains("tool-line-") || e.contains("welcome-widget|")),
        "{shipped:?}"
    );
}

/// 6-A (A20): the interim pane's names are gone from production, a pin can never
/// exempt them, and the reused queue/frame API passes the same scanner.
#[test]
fn k11a_the_interim_pane_symbols_are_unpinnable_and_the_queue_api_passes() {
    for text in [
        "decision_pane::raise(s, d, a);",
        "let t = Surface::DecisionPane;",
        "let p = Placement::StageBottom;",
        "s.pending_large_image = Some(image);",
        "let t = s.pending_delete.take();",
    ] {
        let files = [("src/x.rs".to_owned(), format!("fn real() {{ {text} }}"))];
        let first = scan(&files, &[]);
        assert!(
            first.iter().any(|s| s.contains("decision-21-6-a|")),
            "{text}: {first:?}"
        );
        let sites: Vec<_> = first
            .iter()
            .map(|error| error.split_once(": ").unwrap().1)
            .collect();
        let pins = [("src/x.rs", "21-6", sites.as_slice())];
        let errors = scan(&files, &pins);
        assert!(
            errors.iter().any(|s| s.starts_with("stale pin")),
            "{text}: {errors:?}"
        );
        assert!(
            errors.iter().any(|s| !s.starts_with("stale pin")),
            "{text}: a pin never exempts a 21-6 symbol: {errors:?}"
        );
    }
    // Negative control: the queue API, its kind enum and the new frame are not the
    // retired names.
    let queue = "fn real() { crate::adapters::tui::decision_queue::raise(s, d, a); \
                 s.decisions.holds(DecisionKind::LargeImage); \
                 let q: DecisionQueue = Default::default(); \
                 let k = Surface::Decision; let n = s.pending_images.len(); }";
    assert!(scan(&[("src/x.rs".into(), queue.into())], &[]).is_empty());
}

/// 6-B (A20): retired permission and consent owners cannot be pinned, while
/// the reused widget and queue APIs remain legal.
#[test]
fn k11b_permission_and_consent_symbols_are_unpinnable_and_reused_apis_pass() {
    for text in [
        "s.pending_permission.take();",
        "s.permission_queue.pop_front();",
        "let _: PermissionQueue = queue;",
        "s.pending_feedback_input.take();",
        "s.permission_reader.take();",
        "let _: PermissionReaderState = reader;",
        "let _: FeedbackInputState = feedback;",
        "let _: PendingPermission = permission;",
        "let _: AttachedApprovals = approvals;",
        "let _: PendingConsentApproval = consent;",
        "permission_modal_owns_focus(s);",
        "let kind = ConfirmationType::Permission;",
        "let kind = ConfirmationType::PermissionFeedback;",
        "approval_bridge::advance(s);",
    ] {
        let files = [("src/x.rs".to_owned(), format!("fn real() {{ {text} }}"))];
        let first = scan(&files, &[]);
        assert!(
            first.iter().any(|s| s.contains("decision-21-6-b|real|")),
            "{text}: {first:?}"
        );
        let sites: Vec<_> = first
            .iter()
            .map(|error| error.split_once(": ").unwrap().1)
            .collect();
        let pins = [("src/x.rs", "21-6", sites.as_slice())];
        let errors = scan(&files, &pins);
        assert!(
            errors.iter().any(|s| s.starts_with("stale pin")),
            "{text}: {errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|s| !s.starts_with("stale pin") && s.contains("decision-21-6-b|")),
            "{text}: a pin never exempts a 21-6 symbol: {errors:?}"
        );
    }
    let legal = "fn real() { permission_prompt::card_rows(); \
                 let _: PermissionPayload = payload; \
                 decision_queue::raise_permission(s, request); \
                 advance_deferred_permission(s); }";
    assert!(scan(&[("src/x.rs".into(), legal.into())], &[]).is_empty());
    let widget = include_str!("../../src/adapters/tui/widgets/permission_prompt.rs");
    let errors = scan(
        &[(
            "src/adapters/tui/widgets/permission_prompt.rs".into(),
            widget.into(),
        )],
        &[],
    );
    assert!(
        !errors.iter().any(|s| s.contains("decision-21-6-b|")),
        "{errors:?}"
    );
}

/// 6-C (A20): the retired question, skill-trust, plan-family, delegation, spawn-gate and
/// agent-kill owners cannot be pinned, while the reused builders and queue APIs stay legal.
#[test]
fn k11c_inline_card_symbols_are_unpinnable_and_reused_apis_pass() {
    for text in [
        "let q = state.ask_user_question.take();",
        "let tx = state.question_response_tx.take();",
        "state.pending_skill_trust = None;",
        "state.skill_trust_queue.push_back(x);",
        "state.skill_trust_inspect_mode = true;",
        "state.pending_activation = None;",
        "state.pending_activation_inspect_content = None;",
        "state.pending_plan_approval.take();",
        "state.pending_plan_card.take();",
        "state.task_panel_state.pending_deviation = None;",
        "panel.skip_cascade_pending = None;",
        "panel.cancel_plan_confirm = None;",
        "state.pending_delegation_card.take();",
        "state.pending_spawn_gate.take();",
        "panel.pending_kill_confirm = None;",
        "advance_skill_trust_queue(state);",
        "let kind = ConfirmationType::Question;",
        "let kind = ConfirmationType::SkillTrust;",
        "let kind = ConfirmationType::SkillTrustInspect;",
        "let kind = ConfirmationType::PlanApproval;",
    ] {
        let files = [("src/x.rs".to_owned(), format!("fn real() {{ {text} }}"))];
        let first = scan(&files, &[]);
        assert!(
            first.iter().any(|s| s.contains("decision-21-6-c|real|")),
            "{text}: {first:?}"
        );
        let sites: Vec<_> = first
            .iter()
            .map(|error| error.split_once(": ").unwrap().1)
            .collect();
        let pins = [("src/x.rs", "21-6", sites.as_slice())];
        let errors = scan(&files, &pins);
        assert!(
            errors.iter().any(|s| s.starts_with("stale pin")),
            "{text}: {errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|s| !s.starts_with("stale pin") && s.contains("decision-21-6-c|")),
            "{text}: a pin never exempts a 21-6 symbol: {errors:?}"
        );
    }
    // Positive control through the same scanner: the queue's own entry points and the
    // reused line builders are module paths and payload names, never the retired slots.
    let legal = "fn real() { widgets::ask_user_question::question_lines(q); \
                 let _ = widgets::cancel_plan_confirm_card::lines(c); \
                 decision_queue::question::raise_question(s, c, id, q, tx); \
                 decision_queue::raise_plan_card(s, payload); \
                 s.decisions.holds(DecisionKind::SkillTrust); \
                 let k = DecisionKind::Question; \
                 let _: ConfirmationType = ConfirmationType::Permission; }";
    let errors = scan(&[("src/x.rs".into(), legal.into())], &[]);
    assert!(
        !errors.iter().any(|s| s.contains("decision-21-6-c|")),
        "{errors:?}"
    );
    // The same retired text inside a `#[cfg(test)]` item is test code, not production.
    let test_only = "#[cfg(test)] mod tests { fn t(s: &mut S) { s.ask_user_question.take(); } }";
    assert!(
        scan(&[("src/x.rs".into(), test_only.into())], &[])
            .iter()
            .all(|s| !s.contains("decision-21-6-c|"))
    );
}

/// 6-D / 6D-L1-08: retired card ownership, including the old focus helper,
/// cannot be pinned; reused queue APIs remain legal.
#[test]
fn k11d_legacy_card_symbols_are_unpinnable_and_reused_apis_pass() {
    for text in [
        "let _ = FocusState::Overlay(x);",
        "let _: OverlayType = x;",
        "let _: ConfirmationType = x;",
        "let _: AttachedPresentation = x;",
        "mod attached_presentation;",
        "raise_confirmation(s, x);",
        "settle_legacy(s);",
        "settle_legacy_inner(s);",
        "reducer::restore_focus_after_modal(s);",
        "take_chat_focus_for_card(s, true);",
        "settle_borrowed_card_focus(s, true);",
        "s.card_focus_borrowed_from = None;",
        "card_focus_blocked(s);",
        "s.pending_confirmation = None;",
        "fn confirmation_pending() {}",
        "reducer::confirmation_pending(s);",
        "let _: InlineCard = x;",
        "fn inline_card() {}",
        "card_context(s);",
        "card_special_key(s);",
        "s.pending_consolidation_card = None;",
        "s.pending_forget_card = None;",
        "s.pending_artifact_card = None;",
        "s.pending_peer_add = None;",
        "s.pending_team_retract = None;",
        "s.pending_fork_index = None;",
        "s.pending_rewind_index = None;",
        "s.rewind_preview = None;",
        "s.pending_export = None;",
        "s.retract_confirm = None;",
        "s.consolidation_token = None;",
    ] {
        let files = [("src/x.rs".to_owned(), format!("fn real() {{ {text} }}"))];
        let first = scan(&files, &[]);
        assert!(
            first.iter().any(|s| s.contains("decision-21-6-d|")),
            "{text}: {first:?}"
        );
        let sites: Vec<_> = first
            .iter()
            .map(|e| e.split_once(": ").unwrap().1)
            .collect();
        let errors = scan(&files, &[("src/x.rs", "21-6", sites.as_slice())]);
        assert!(
            errors.iter().any(|s| s.starts_with("stale pin")),
            "{text}: {errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|s| !s.starts_with("stale pin") && s.contains("decision-21-6-d|")),
            "{text}: {errors:?}"
        );
    }
    let legal = "fn real() { decision_queue::raise_consolidation(s, card); \
        let _ = DecisionKind::Consolidation; widgets::consolidation_card::lines(card, theme); \
        state.decisions.holds(DecisionKind::Consolidation); \
        decision_queue::confirmation_pending(state); \
        settle_login(state); set_base(state, focus); }";
    let errors = scan(&[("src/x.rs".into(), legal.into())], &[]);
    assert!(
        !errors.iter().any(|s| s.contains("decision-21-6-d|")),
        "{errors:?}"
    );
    let queue_query = "pub fn confirmation_pending(state: &TuiState) -> bool { true }";
    assert!(
        scan(
            &[(
                "src/adapters/tui/decision_queue/retract.rs".into(),
                queue_query.into()
            )],
            &[]
        )
        .iter()
        .all(|s| !s.contains("decision-21-6-d|"))
    );
    let test_only = "#[cfg(test)] mod tests { fn t(s: &mut S) { s.pending_export.take(); } }";
    assert!(
        scan(&[("src/x.rs".into(), test_only.into())], &[])
            .iter()
            .all(|s| !s.contains("decision-21-6-d|"))
    );
}

/// 6-E (final, K11e): the proposal card slots, the attached pending peer card, the bottom
/// anchored card painter and the shared `Card`/`Cue` contexts cannot be revived or pinned;
/// the reused body builders and queue APIs pass, and the 21-6 pin list is empty.
#[test]
fn k11e_proposal_and_peer_card_symbols_are_unpinnable_and_reused_apis_pass() {
    for text in [
        "s.pending_team_proposals.insert(a, b);",
        "s.team_proposal_visible = None;",
        "tab.team_proposal_reviews.clear();",
        "s.team_proposal_scroll = 0;",
        "s.team_proposal_scroll_max = 0;",
        "s.team_proposal_footer_visible = true;",
        "s.team_proposal_viewport_rows = 4;",
        "let c = pending_peer_card(conv, left_out);",
        "peer_response_card_lines(&p, false, 60);",
        "render_bottom_anchored_decision_card(f, a, b);",
        "proposal_higher_confirmation_active(s);",
        "visible_team_proposal(s);",
        "keymap::render::card_cue(Context::Card, h);",
        "let c = Context::Card;",
        "let c = CardCmd::Cue;",
    ] {
        let files = [("src/x.rs".to_owned(), format!("fn real() {{ {text} }}"))];
        let first = scan(&files, &[]);
        assert!(
            first.iter().any(|s| s.contains("decision-21-6-e|")),
            "{text}: {first:?}"
        );
        let sites: Vec<_> = first
            .iter()
            .map(|e| e.split_once(": ").unwrap().1)
            .collect();
        let errors = scan(&files, &[("src/x.rs", "21-6", sites.as_slice())]);
        assert!(
            errors.iter().any(|s| s.starts_with("stale pin")),
            "{text}: {errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|s| !s.starts_with("stale pin") && s.contains("decision-21-6-e|")),
            "{text}: {errors:?}"
        );
    }
    let legal = "fn real() { decision_queue::raise_team_proposal(s, p, r); \
        let _ = DecisionKind::TeamProposal; widgets::team_proposal_card::lines(p, theme); \
        state.decisions.presenting(DecisionKind::TeamProposal); \
        let _ = Context::TeamProposal; let _ = CardCmd::Review; }";
    let errors = scan(&[("src/x.rs".into(), legal.into())], &[]);
    assert!(
        !errors.iter().any(|s| s.contains("decision-21-6-e|")),
        "{errors:?}"
    );
    let test_only =
        "#[cfg(test)] mod tests { fn t(s: &mut S) { s.pending_team_proposals.clear(); } }";
    assert!(
        scan(&[("src/x.rs".into(), test_only.into())], &[])
            .iter()
            .all(|s| !s.contains("decision-21-6-e|"))
    );
    // Slice 8-A removed the last 21-8 pins: none remains.
    assert_eq!(PINS.len(), 0, "a pin survives");
}

/// Story 21.8 slice 8-A (A17): each retired recipient-surface symbol is rejected in
/// production, cannot be pinned (the pin goes stale and the hit still fails), while the
/// S-THREAD frame's own identifiers, test-only items, comments and unrelated recipient
/// names (`Context::RecipientDraft`, `thread_flash`) pass.
#[test]
fn k6_story21_8_retired_recipient_surface_symbols_are_unpinnable_and_thread_apis_pass() {
    for text in [
        "let c = Context::RecipientSurface;",
        "let c = OverlayCmd::RecipientPrev;",
        "let c = OverlayCmd::RecipientNext;",
        "let c = OverlayCmd::RecipientClose;",
        "let c = OverlayCmd::RecipientPageUp;",
        "let c = OverlayCmd::RecipientPageDown;",
        "let c = OverlayCmd::RecipientToggle;",
        "if recipient_higher_confirmation_active(s) {}",
        "if attached_higher_confirmation(s) {}",
        "recipient_surface_key(k, w, c, s, t);",
        "dispatch_recipient_surface_key(k, w, c, s, t);",
        "render_recipient_surface(f, r, s, c);",
        "if recipient_surface_ignores_raw_key(s, e) {}",
        "exec_recipient(s, c, k);",
        "let s: RecipientSurface = make();",
        "let t = RecipientSurfaceTarget::Item(a);",
        "let a = RecipientSurfaceAccess::Writer;",
        "let a = RecipientSurfaceAction::Respond;",
        "let r = s.recipient_surface.take();",
        "let b = recipient_blocked(s);",
        "let t = \"[c] Use as context\";",
        "let t = \"[c] Stop using as context\";",
    ] {
        let files = [("src/x.rs".to_owned(), format!("fn real() {{ {text} }}"))];
        let first = scan(&files, &[]);
        assert!(
            first.iter().any(|s| s.contains("thread-21-8-a|")),
            "{text}: {first:?}"
        );
        let sites: Vec<_> = first
            .iter()
            .map(|e| e.split_once(": ").unwrap().1)
            .collect();
        let errors = scan(&files, &[("src/x.rs", "21-8", sites.as_slice())]);
        assert!(
            errors.iter().any(|s| s.starts_with("stale pin")),
            "{text}: {errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|s| !s.starts_with("stale pin") && s.contains("thread-21-8-a|")),
            "{text}: {errors:?}"
        );
    }
    let legal = "fn real() { let _ = Context::ThreadInspector; let _ = Context::RecipientDraft; \
        let _ = OverlayCmd::QuitRequest; reducer::open_thread(s, p, t); \
        widgets::thread_inspector::render(f, r, s, c, t); thread_flash(s, m); \
        let _ = \"[u] Use as context\"; let _ = \"Use as context\"; }";
    let errors = scan(&[("src/x.rs".into(), legal.into())], &[]);
    assert!(
        !errors.iter().any(|s| s.contains("thread-21-8-a|")),
        "{errors:?}"
    );
    let masked = "// RecipientSurface recipient_surface prior_focus\n\
        #[cfg(test)] mod tests { fn t(s: &mut S) { s.recipient_surface = None; } }\n\
        fn real() { let _ = 1; }";
    assert!(
        scan(&[("src/x.rs".into(), masked.into())], &[])
            .iter()
            .all(|s| !s.contains("thread-21-8-a|"))
    );
}

#[test]
fn k6_story21_8_c_retired_symbols_are_unpinnable_and_cli_renderer_remains_legal() {
    for text in [
        "show_team_thread(s);",
        "let _ = TEAM_THREAD_BLOCK_ID;",
        "let _ = ViewKind::TeamThread;",
        "let _ = ViewKind::PeerRetract;",
        "let _ = \"peer-retract\";",
        "s.view_block = None;",
        "let _ = ViewKind::TeamLog;",
        "let _ = TEAM_LOG_BLOCK_ID;",
        "let _ = MAX_INCHAT_ROWS;",
        "run_team_log(s);",
        "render_team_send_rows(rows);",
    ] {
        let files = [("src/x.rs".to_owned(), format!("fn real() {{ {text} }}"))];
        let first = scan(&files, &[]);
        assert!(
            first.iter().any(|s| s.contains("thread-21-8-c|")),
            "{text}: {first:?}"
        );
        let sites: Vec<_> = first
            .iter()
            .map(|e| e.split_once(": ").unwrap().1)
            .collect();
        let errors = scan(&files, &[("src/x.rs", "21-8", sites.as_slice())]);
        assert!(
            errors.iter().any(|s| s.starts_with("stale pin")),
            "{text}: {errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|s| !s.starts_with("stale pin") && s.contains("thread-21-8-c|")),
            "{text}: {errors:?}"
        );
    }
    for path in [
        "src/adapters/tui/handlers/team_command.rs",
        "src/adapters/cli/team_send_cli.rs",
    ] {
        let errors = scan(
            &[(
                path.into(),
                "fn real() { render_team_send_rows(rows); }".into(),
            )],
            &[],
        );
        assert!(
            !errors.iter().any(|s| s.contains("thread-21-8-c|")),
            "{errors:?}"
        );
    }
    assert!(PINS.is_empty());
}
