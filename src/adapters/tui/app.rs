use crate::adapters::tui::state::{ChordAction, Direction, TuiState};
use crate::adapters::tui::widgets::chat_pane::virtual_scroll::find_next_boundary;
use crate::adapters::tui::widgets::input_box;
use crate::domain::events::{DomainInputEvent, DomainKey};
use crate::domain::models::FocusState;
use crate::domain::models::StatusState;
use crate::domain::models::autocomplete::AutocompleteKind;
use crate::domain::models::visual::{ConfirmationType, OverlayType};

/// Action returned by handle_input to tell the event loop what to do.
/// app.rs is a pure input→action mapper; the event loop owns all side effects.
#[derive(Debug, PartialEq, Eq)]
pub enum InputAction {
    /// Event handled, no further action needed.
    Consumed,
    /// Event not handled by this focus mode.
    Ignored,
    /// Enter pressed with this text (buffer already cleared by handle_input).
    SubmitMessage(String),
    /// User wants to exit.
    Quit,
    /// Ctrl+C: cancel streaming if active, otherwise quit.
    CancelOrQuit,
    /// Permission prompt: user pressed y.
    PermissionAllow,
    /// Permission prompt: user pressed n.
    PermissionDeny,
    /// Permission prompt: user pressed a.
    PermissionAlwaysAllow,
    /// Permission prompt: user pressed s (AC4 — session allow).
    PermissionSessionAllow,
    /// Permission prompt: user pressed f (AC5 — deny + feedback).
    PermissionDenyFeedback,
    /// Feedback input: user typed a character (AC5).
    FeedbackInputChar(char),
    /// Feedback input: user pressed backspace (AC5).
    FeedbackInputBackspace,
    /// Feedback input: user pressed enter (AC5).
    FeedbackInputSubmit,
    /// Feedback input: user pressed Esc (AC5 — cancel feedback, restore prompt).
    FeedbackInputCancel,
    /// Feedback block: user pressed r to retry (Ctrl+K r).
    FeedbackRetry,
    /// Feedback block: user pressed c to compact (Ctrl+K c).
    FeedbackCompact,
    /// Feedback block: user pressed x to dismiss (Ctrl+K x).
    FeedbackDismiss,
    /// Feedback block: user pressed n to start fresh (Ctrl+K n).
    FeedbackStartFresh,
    /// AskUserQuestion: user submitted their answer.
    SubmitQuestionAnswer(String),
    /// Execute a built-in command (e.g., "/new", "/export").
    /// `args` carries an optional trailing argument for commands that support
    /// one — e.g., `/export meeting-notes.md` → `args: Some("meeting-notes.md")`.
    /// Empty / whitespace-only arguments are normalized to `None` by the parser.
    ExecuteCommand {
        name: String,
        args: Option<String>,
    },
    /// Submit message with file context and/or command context.
    /// Contains (user_text, resolved_mentions, command_name_if_any).
    SubmitWithContext {
        text: String,
        command: Option<String>,
        command_args: Option<String>,
    },
    /// Skill selected from autocomplete (Story 5-2 AC8).
    #[allow(dead_code)]
    SkillSelected {
        name: String,
        arguments: String,
    },
    /// Skill trust prompt: user pressed y (Story 5-2 AC4).
    SkillTrustAccept,
    /// Skill trust prompt: user pressed n (Story 5-2 AC4).
    SkillTrustDecline,
    /// Skill trust prompt: user pressed i to inspect skill file (Story 5-2 AC4).
    SkillTrustInspect,
    /// Plan approval: user pressed y (approve Normal).
    PlanApproveNormal,
    /// Plan approval: user pressed a (approve AutoEdit).
    PlanApproveAutoEdit,
    /// Plan approval: user pressed n (reject).
    PlanReject,
    /// Plan approval: user pressed e (revise in editor).
    PlanRevise,
    /// Plan card (6-1a): user pressed y (approve).
    PlanCardApprove,
    /// Plan card (6-1a): user pressed n (reject).
    PlanCardReject,
    /// Plan card (6-1a): user pressed e (edit in external editor).
    PlanCardEdit,
    /// Create a new tab (Ctrl+T or palette).
    NewTab,
    /// Close the active tab (palette).
    CloseTab,
    /// Switch to the next tab (Tab key when focus is Chat).
    #[allow(dead_code)] // constructed in integration tests, not in lib build
    SwitchToNextTab,
    /// Switch to the previous tab (Shift+Tab when focus is not Input).
    SwitchToPrevTab,
    /// Cycle permission mode (Shift+Tab when focus is Input).
    CycleMode,
    /// Switch directly to a tab by 1-based index (number keys 1-9 in Chat focus).
    SwitchToTab(usize),
    /// Toggle the history sidebar (Ctrl+H).
    // Covers: FR107, UX-DR20
    ToggleSidebar,
    /// Open or toggle a sidebar panel (Ctrl+X, T for Tasks).
    OpenPanel(crate::domain::models::visual::PanelType),
    /// Copy task result/error from drill-down detail view (Story 6-3 AC8).
    CopyTaskResult {
        plan_id: Option<String>,
        task_number: u32,
    },
    /// Story 6.4: Pause/Resume task (panel `p` or drill-down `p` on Paused).
    TaskPause(u32),
    /// Story 6.4: Skip task (panel `s` or drill-down `s` on Failed).
    TaskSkip(u32),
    /// Story 6.4: Retry failed task (drill-down `r` on Failed).
    TaskRetry(u32),
    /// Story 6.4: Edit failed task (drill-down `e` on Failed).
    TaskEdit(u32),
    /// Story 6.4: Cancel plan (panel `x` or palette `!cancel-plan`).
    TaskCancelPlan,
    /// Story 6.4: Resume all paused tasks (palette `!resume-all-tasks`).
    TaskResumeAll,
    /// Story 6.4: Enter reorder mode for selected task (palette `!reorder-task`).
    TaskReorderEnter(u32),
    /// Story 6.4: Move reorder target up/down.
    TaskReorderMove(Direction),
    /// Story 6.4: Commit reorder (Enter).
    TaskReorderCommit,
    /// Story 6.4: Cancel reorder (Esc).
    TaskReorderCancel,
    /// Story 6.4: Skip cascade card response.
    SkipCascadeAck(crate::adapters::tui::state::SkipCascadeChoice),
    /// Story 6.4: Plan deviation card decision.
    PlanDeviationDecided(String, crate::domain::models::plan::PlanDecision),
    /// Story 6.4: Cancel plan confirm card response.
    CancelPlanConfirm(bool),
    /// Open the selected conversation from sidebar.
    // Covers: FR107, AC4
    OpenSidebarConversation,
    /// Delete the selected conversation from sidebar (shows confirmation overlay).
    // Covers: FR113, AC5
    DeleteSidebarConversation,
    /// Delete all conversations (via command palette).
    // Covers: AC5 (bulk), P9
    DeleteAllConversations,
    /// User confirmed pending delete (pressed 'y').
    ConfirmDelete,
    /// User cancelled pending delete (pressed 'n' or Esc).
    CancelDelete,
    /// Copy content to clipboard.
    // Covers: FR116, UX-DR68
    CopyToClipboard(String),
    /// User confirmed attaching a large image.
    // Covers: FR112 (AC4)
    ImageConfirmAttach,
    /// User cancelled attaching a large image.
    // Covers: FR112 (AC4)
    ImageConfirmCancel,
    /// Pasted image format is unsupported — show FeedbackBlock.
    // Covers: FR112 (AC3)
    ImageFormatError,
    /// Pasted image exceeds size threshold — needs user confirmation.
    // Covers: FR112 (AC4)
    ImageSizeWarning {
        media_type: String,
        data: String,
        warning: String,
    },
    /// Request a system-clipboard paste: read image (or text) from the OS clipboard.
    /// The event loop handles this asynchronously via ClipboardPort, then re-enters
    /// handle_input with the resulting ImagePaste or Paste event.
    RequestClipboardPaste,
    /// Fork conversation at the currently focused message (f key in Chat focus).
    // Covers: Story 4-3a, AC1
    ForkAtMessage,
    /// Fork confirmation: user pressed y.
    // Covers: Story 4-3a, AC1
    ForkConfirm,
    /// Fork confirmation: user pressed n or Esc.
    // Covers: Story 4-3a, AC1
    ForkCancel,
    /// Rewind conversation to the currently focused message (R key in Chat focus).
    // Covers: Story 4-3b, AC1; UX-DR90
    RewindAtMessage,
    /// Rewind confirmation: user pressed y.
    // Covers: Story 4-3b, AC1
    RewindConfirm,
    /// From rewind confirmation: user pressed f to fork instead.
    // Covers: Story 4-3b, AC4
    RewindForkInstead,
    /// Rewind confirmation: user pressed n or Esc.
    // Covers: Story 4-3b, AC1
    RewindCancel,
    /// Open the within-conversation search overlay (Ctrl+F in Chat focus).
    // Covers: Story 4-4, AC1 (UX-DR86)
    OpenSearch,
    /// Close the search overlay (Esc from Search overlay).
    // Covers: Story 4-4, AC4
    CloseSearch,
    /// Query string changed — event loop should re-run `find_matches`
    /// and apply the calm-jump rule. Fired for printable chars and Backspace
    /// in the Typing sub-state.
    // Covers: Story 4-4, AC2
    SearchQueryChanged,
    /// Enter pressed in Typing sub-state — if matches exist, transition
    /// to Navigating.
    // Covers: Story 4-4, AC3
    SearchCommit,
    /// `n` in Navigating — advance focused match (wraps at end).
    // Covers: Story 4-4, AC3
    SearchNext,
    /// `N` (Shift+n) in Navigating — reverse focused match (wraps at start).
    // Covers: Story 4-4, AC3
    SearchPrev,
    /// `Ctrl+U` — clear query, stay in Typing sub-state.
    // Covers: Story 4-4, AC3 (Reviewer Fix 4)
    SearchClear,
    /// Printable char or Backspace in Navigating — return to Typing and
    /// re-apply the keystroke so the user can refine the query without
    /// pressing Esc. The char has already been appended / removed by
    /// `handle_input`.
    // Covers: Story 4-4, AC3 amendment
    SearchReturnToTyping,
    /// Toggle bookmark on the currently focused message (m in Chat focus).
    // Covers: Story 4-4, AC8 (UX-DR91)
    ToggleBookmark,
    /// Open the bookmark list panel (' in Chat focus).
    // Covers: Story 4-4, AC10 (UX-DR91)
    OpenBookmarkList,
    /// Jump to the selected bookmark (Enter in BookmarkList overlay).
    // Covers: Story 4-4, AC10
    JumpToBookmark,
    /// Delete the currently selected bookmark from the list (d/Del/Backspace).
    // Covers: Story 4-4, AC10
    DeleteBookmark,
    /// Undo the last bookmark delete (u in BookmarkList overlay, within 5 s).
    // Covers: Story 4-4, AC10 synthesis
    UndoBookmarkDelete,
    /// Close the bookmark list panel (Esc in BookmarkList overlay).
    // Covers: Story 4-4, AC10
    CloseBookmarkList,
    /// Open the cross-conversation search overlay (`/` in sidebar focus).
    // Covers: Story 4-4, AC5 (UX-DR87)
    OpenCrossSearch,
    /// Query updated in the cross-search overlay — event loop should kick
    /// off a new scan (if query.len() >= 2).
    // Covers: Story 4-4, AC5
    CrossSearchQueryChanged,
    /// Open the currently selected cross-search result in a new (or existing)
    /// tab, applying a peek highlight per AC6 amendment.
    // Covers: Story 4-4, AC6
    OpenCrossSearchResult,
    /// Close the cross-search overlay (Esc).
    // Covers: Story 4-4, AC5
    CloseCrossSearch,
    /// Export-overwrite confirmation: user pressed `y` — commit the
    /// pre-rendered content to the target path atomically.
    // Covers: Story 4-4, AC12
    ConfirmExportOverwrite,
    /// Export-overwrite confirmation: user pressed `n` / `Esc` — discard
    /// the pending content and flash "Export cancelled".
    // Covers: Story 4-4, AC12
    CancelExportOverwrite,
    SetActiveAgent {
        name: String,
        then_submit: Option<String>,
    },
    ClearActiveAgent {
        then_submit: Option<String>,
    },
    #[allow(dead_code)]
    UnknownAgent(String),
    /// Agent mention submitted before background discovery finished.
    /// The name and optional trailing text are queued in TuiState.
    AgentDiscoveryPending {
        name: String,
        then_submit: Option<String>,
    },
    // === Story 16.6: vim keymap ===
    /// vim `za` — toggle fold on focused turn. AC1
    FoldToggleAtFocus,
    /// vim `zc` — collapse focused turn. AC1 (idempotent)
    CollapseFocus,
    /// vim `zo` — expand focused turn. AC1 (idempotent)
    ExpandFocus,
    /// vim `zM` — collapse all turns (global). AC2
    CollapseAllTurns,
    /// vim `zR` — expand all turns (global). AC2
    ExpandAllTurns,
    /// vim `zs` — toggle summary tier (Tier1 <-> Tier2) globally. AC7
    ToggleSummaryTier,
    /// vim `zz` — recenter view on focused (sticky-reply) anchor. AC4
    RecenterAnchor,
    /// vim `]]` / `[[` — jump to next/previous assistant-prose turn. AC3
    JumpProseAnchor(Direction),
    /// vim `]P` chord — jump to latest assistant-prose turn.
    /// Originally bound to `G` by Story 16.6 AC6; relocated to `]P` by S16.8
    /// preflight rebinding (2026-05-03) so `G` returns to vim-bottom semantic
    /// per ADR-16-03 (Anchor as Explicit User Investment). The bracket-prefix
    /// chord (vs a `g`-prefix chord) avoids the flicker problem where single-`g`
    /// would fire `ScrollToTop` first; bracket leader has no first-key side
    /// effect. Mnemonic: `]` family for "forward to last X"; capital `P` = "Prose".
    /// The dispatcher arm at `event_loop.rs::JumpToLatestProseAnchor` is
    /// unchanged — only the keystroke that produces this variant moved.
    JumpToLatestProseAnchor,
    /// vim `Tab` narrow override — cycle invocations within focused expanded turn. AC5
    CycleInvocationInFocusedTurn,
    // === Story 16.8: fast scroll + mouse ===
    /// Ctrl+d — scroll half page down (Chat focus). AC1
    ScrollHalfPageDown,
    /// Ctrl+u — scroll half page up (Chat focus). AC1
    ScrollHalfPageUp,
    /// Ctrl+b — scroll full page up (Chat focus). AC1
    ScrollFullPageUp,
    /// Ctrl+f — scroll full page down (Chat focus narrow override). AC1, AC9
    ScrollFullPageDown,
    /// `gg` or single `g` — jump to top (Reading mode). AC2
    ScrollToTop,
    /// `G` — jump to bottom (Following mode) or no-op when Pinned. AC3
    ScrollToBottom,
    /// Mouse wheel scroll event. AC4
    MouseScroll(crate::domain::models::view_state::ScrollDelta),
    /// Legacy `j` — scroll one line down. Migrated from direct mutation. AC7
    ScrollLineDown,
    /// Legacy `k` — scroll one line up. Migrated from direct mutation. AC7
    ScrollLineUp,
    /// Block-boundary jump (J/K/{/}) — carries pre-computed offset + auto_scroll.
    /// The event-loop dispatcher sets view_state.scroll_offset directly. AC7
    BlockJump {
        offset: usize,
        auto_scroll: bool,
    },
    /// Open the model/provider selector overlay (Story 7.2 AC1).
    OpenModelSelector,
    /// Apply a model/provider switch (Story 7.2 AC3).
    /// `provider_id: None` means resolve from the registry by `model_id` (`:` palette path).
    SwitchModelProvider {
        provider_id: Option<String>,
        model_id: String,
    },
    /// Compact context then switch model (Story 7.4 AC13).
    CompactThenSwitchModel {
        provider_id: String,
        model_id: String,
    },
    /// Open the usage/cost panel overlay (Story 7.5 AC3).
    OpenUsagePanel,
    /// Daily-budget warning — user picked "Continue anyway" (Story 7.5 AC5).
    FeedbackBudgetContinue,
    /// Daily-budget warning — user picked "Switch to cheaper model" (Story 7.5 AC5).
    FeedbackBudgetSwitchCheaper,
    /// Daily-budget warning — user picked "Pause until tomorrow" (Story 7.5 AC5/AC7).
    FeedbackBudgetPause,
}

/// Bridge FeedbackAction → InputAction. Compiler-enforced exhaustiveness:
/// every new FeedbackAction variant must have a mapping arm here or the match
/// won't compile — this is the adapter-layer half of the single-source-of-truth contract.
fn feedback_action_to_input_action(action: crate::domain::models::FeedbackAction) -> InputAction {
    use crate::domain::models::FeedbackAction;
    match action {
        FeedbackAction::Retry => InputAction::FeedbackRetry,
        FeedbackAction::Compact => InputAction::FeedbackCompact,
        FeedbackAction::StartFresh => InputAction::FeedbackStartFresh,
        FeedbackAction::Dismiss => InputAction::FeedbackDismiss,
        FeedbackAction::BudgetContinue => InputAction::FeedbackBudgetContinue,
        FeedbackAction::BudgetSwitchCheaper => InputAction::FeedbackBudgetSwitchCheaper,
        FeedbackAction::BudgetPause => InputAction::FeedbackBudgetPause,
        FeedbackAction::Custom(_) => {
            tracing::warn!("Custom feedback action dispatched via chord key — ignoring");
            InputAction::Consumed
        }
    }
}

/// Handle a domain input event by updating TUI state.
/// Returns an InputAction telling the event loop what to do.
pub fn handle_input(state: &mut TuiState, event: &DomainInputEvent) -> InputAction {
    // Cancel pending chord leader on any event that isn't a character key or Ctrl+K.
    if !matches!(
        event,
        DomainInputEvent::KeyPress(_) | DomainInputEvent::SpecialKey(DomainKey::CtrlK)
    ) {
        state.chord_leader_active = false;
        state.pending_z = false;
        state.pending_bracket = None;
        state.pending_g = false;
    }
    match event {
        DomainInputEvent::KeyPress(c) => handle_char(state, *c),
        DomainInputEvent::SpecialKey(key) => handle_special_key(state, *key),
        DomainInputEvent::MouseScroll(delta) => {
            state.needs_redraw = true;
            InputAction::MouseScroll(*delta)
        }
        DomainInputEvent::Resize(w, h) => {
            // Anchor-based scroll preservation: find the message index at the
            // top of the viewport using the *old* HeightCache before invalidation.
            if state.scroll_snapshot > 0 && !state.block_boundaries.is_empty() {
                let old_vp = state.viewport_height as usize;
                let max_offset = state.total_content_height.saturating_sub(old_vp);
                let clamped = state.scroll_snapshot.min(max_offset);
                let top_line = max_offset.saturating_sub(clamped);

                // Find which message index contains this top_line by scanning
                // block_boundaries (sorted). The last boundary <= top_line is the anchor.
                let anchor_idx = match state.block_boundaries.binary_search(&top_line) {
                    Ok(i) => i,
                    Err(i) => i.saturating_sub(1),
                };
                state.pending_anchor = Some(anchor_idx);
            }

            state.terminal_width = *w;
            state.terminal_height = *h;
            state
                .tab_render_state(state.active_tab_id)
                .height_cache
                .invalidate_all();
            state.tab_render_state(state.active_tab_id).cached_width = Some(*w);
            // P11: Reset sidebar if terminal shrinks below minimum width
            if *w < crate::adapters::tui::layout::SIDEBAR_MIN_WIDTH && state.sidebar_visible {
                state.sidebar_visible = false;
                state.sidebar_panel = None;
                state.task_panel_state.drill_down_task = None;
                state.task_panel_state.expanded_detail = false;
                state.task_panel_state.detail_scroll_offset = 0;
                if matches!(state.focus, FocusState::Sidebar { .. }) {
                    state.focus = FocusState::Chat;
                }
            }
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainInputEvent::FocusGained | DomainInputEvent::FocusLost => {
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainInputEvent::ImagePaste(raw_bytes) => {
            // Validate and attach image from clipboard paste
            // Covers: FR112 (AC1)
            use crate::adapters::tui::image;

            // P1: Reject oversized images before base64 encoding to prevent OOM
            const MAX_RAW_IMAGE_SIZE: usize = 20 * 1024 * 1024; // 20MB
            if raw_bytes.len() > MAX_RAW_IMAGE_SIZE {
                return InputAction::ImageFormatError; // Reuse error path — too large to process
            }

            match image::detect_image_format(raw_bytes) {
                Ok(media_type) => {
                    use base64::Engine;
                    let data = base64::engine::general_purpose::STANDARD.encode(raw_bytes);
                    let base64_len = data.len();

                    // Check size threshold
                    if let Some(warning) = image::validate_image_size(base64_len) {
                        return InputAction::ImageSizeWarning {
                            media_type: media_type.to_string(),
                            data,
                            warning,
                        };
                    }

                    let _total_kb = base64_len / 1024;
                    let attachment = crate::domain::models::ImageAttachment {
                        media_type: media_type.to_string(),
                        data,
                    };
                    state.pending_images.push(attachment);
                    state.image_indicator = Some(image::format_image_indicator(
                        state.pending_images.len(),
                        state
                            .pending_images
                            .iter()
                            .fold(0usize, |acc, i| acc.saturating_add(i.data.len() / 1024)),
                    ));
                    state.needs_redraw = true;
                    InputAction::Consumed
                }
                Err(_) => {
                    // Unsupported format — return action for event loop to create FeedbackBlock
                    InputAction::ImageFormatError
                }
            }
        }
        DomainInputEvent::Paste(text) => {
            // Insert text at cursor position (bracketed paste mode)
            if matches!(state.focus, FocusState::Input) {
                let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
                state.input_buffer.insert_str(byte_pos, text);
                state.cursor_position += text.chars().count();
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }
    }
}

/// Convert a char-index to a byte-index in the string.
fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

fn handle_char(state: &mut TuiState, c: char) -> InputAction {
    // Ctrl+K chord-prefix dispatch (FeedbackAction arbiter — UX-DR-GLOBAL-CHORD-PREFIX).
    // Fires BEFORE any focus/overlay checks so it works regardless of keyboard focus.
    if state.chord_leader_active {
        state.chord_leader_active = false;
        if let Some(action) = crate::domain::models::FeedbackAction::dispatch_key(c)
            .map(feedback_action_to_input_action)
        {
            return action;
        }
        state.needs_redraw = true;
        return InputAction::Consumed;
    }

    // Command palette: typing updates filter text
    // Covers: UX-DR18
    if state.command_palette.active {
        // Detect scope prefix on first char
        if state.command_palette.filter_text.is_empty() {
            if let Some(scope) =
                crate::adapters::palette_registry::PaletteRegistry::scope_for_prefix(c)
            {
                state.command_palette.current_scope = Some(scope);
                state.command_palette.filter_text.push(c);
                state.needs_redraw = true;
                return InputAction::Consumed;
            }
        }
        state.command_palette.filter_text.push(c);
        state.needs_redraw = true;
        return InputAction::Consumed;
    }

    // Help overlay: handle char keys (j/k/g/G scrolling and ? dismiss)
    // Covers: FR108, UX-DR94
    if state.focus == FocusState::Overlay(OverlayType::Help) {
        return handle_help_overlay_char(state, c);
    }

    // Model selector overlay: handle vim-style navigation (h/l/j/k) and y/n confirmation
    // Story 7.2 AC1, AC5
    if state.focus == FocusState::Overlay(OverlayType::ModelSelector) {
        return handle_model_selector_char(state, c);
    }

    // Usage panel overlay (Story 7.5 AC3): all chars consumed; arrows handled in key path.
    if state.focus == FocusState::Overlay(OverlayType::UsagePanel) {
        // No char-action mappings yet — just consume.
        let _ = c;
        return InputAction::Consumed;
    }

    // Which-key: single char lookup in chord map
    // Covers: UX-DR19
    if state.which_key.active {
        let chord = state.which_key.lookup_chord(c).cloned();
        if let Some(action) = chord {
            let prior_focus = state.which_key.dismiss().unwrap_or(FocusState::Input);
            state.focus = prior_focus.clone();
            match action {
                ChordAction::Noop(msg) => {
                    // Show "Not yet available" feedback
                    let block = crate::domain::models::FeedbackBlock {
                        id: format!("chord-{}", c),
                        level: crate::domain::models::FeedbackLevel::Info,
                        message: msg,
                        actions: Vec::new(),
                    };
                    state.feedback_blocks.insert(block.id.clone(), block);
                }
                ChordAction::ShowHelp => {
                    // Open help overlay
                    state.help_overlay.open(prior_focus);
                    state.focus = FocusState::Overlay(OverlayType::Help);
                }
                ChordAction::OpenPanel(panel_type) => {
                    state.which_key.dismiss();
                    return InputAction::OpenPanel(panel_type);
                }
                ChordAction::OpenModelSelector => {
                    state.which_key.dismiss();
                    return InputAction::OpenModelSelector;
                }
                ChordAction::OpenUsagePanel => {
                    state.which_key.dismiss();
                    return InputAction::OpenUsagePanel;
                }
            }
            state.needs_redraw = true;
            return InputAction::Consumed;
        } else {
            // Invalid key: dismiss silently (AC6)
            state.focus = state.which_key.dismiss().unwrap_or(FocusState::Input);
            state.needs_redraw = true;
            return InputAction::Consumed;
        }
    }

    // Reverse search: typing adds to query
    // Covers: UX-DR74
    if state.reverse_search.active {
        state.reverse_search.query.push(c);
        update_reverse_search_matches(state);
        state.needs_redraw = true;
        return InputAction::Consumed;
    }

    // Inline PlanCard takes priority over Permission overlay when pending (H1 fix).
    // Without this guard, ApprovalRuntime steals focus to Overlay(Permission) and 'y'
    // routes to PermissionAllow instead of PlanCardApprove.
    if state.pending_plan_card.is_some() {
        match c {
            'y' => return InputAction::PlanCardApprove,
            'n' => return InputAction::PlanCardReject,
            'e' => return InputAction::PlanCardEdit,
            _ => {}
        }
    }

    // Story 6.4: Plan deviation card key intercept (y/e/n)
    if let Some((ref pid, _)) = state.task_panel_state.pending_deviation {
        match c {
            'y' => {
                return InputAction::PlanDeviationDecided(
                    pid.clone(),
                    crate::domain::models::plan::PlanDecision::Approve,
                );
            }
            'e' => {
                return InputAction::PlanDeviationDecided(
                    pid.clone(),
                    crate::domain::models::plan::PlanDecision::Edit,
                );
            }
            'n' => {
                return InputAction::PlanDeviationDecided(
                    pid.clone(),
                    crate::domain::models::plan::PlanDecision::Reject,
                );
            }
            _ => return InputAction::Ignored,
        }
    }

    // Story 6.4: Cancel plan confirm card key intercept (y/n)
    if let Some(ref _pid) = state.task_panel_state.cancel_plan_confirm {
        match c {
            'y' => return InputAction::CancelPlanConfirm(true),
            'n' => return InputAction::CancelPlanConfirm(false),
            _ => return InputAction::Ignored,
        }
    }

    // Story 6.4: Skip cascade card key intercept (s/c/n)
    if state.task_panel_state.skip_cascade_pending.is_some() {
        match c {
            's' => {
                return InputAction::SkipCascadeAck(
                    crate::adapters::tui::state::SkipCascadeChoice::CascadeSkip,
                );
            }
            'c' => {
                return InputAction::SkipCascadeAck(
                    crate::adapters::tui::state::SkipCascadeChoice::ContinueAnyway,
                );
            }
            'n' => {
                return InputAction::SkipCascadeAck(
                    crate::adapters::tui::state::SkipCascadeChoice::CancelSkip,
                );
            }
            _ => return InputAction::Ignored,
        }
    }

    // Permission prompt focus: y/n/a/s/f are handled, chat-scroll keys pass through
    // to the chat pane (AC6), all others ignored.
    if state.focus == FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Permission)) {
        match c {
            'y' => return InputAction::PermissionAllow,
            'n' => return InputAction::PermissionDeny,
            'a' => return InputAction::PermissionAlwaysAllow,
            's' => return InputAction::PermissionSessionAllow,
            'f' => return InputAction::PermissionDenyFeedback,
            // AC6: chat-scroll preserved while the prompt is up.
            // Story 16.8, AC7: migrated from direct mutation to InputAction emission.
            'j' => {
                state.needs_redraw = true;
                return InputAction::ScrollLineDown;
            }
            'k' => {
                state.needs_redraw = true;
                return InputAction::ScrollLineUp;
            }
            'G' => {
                state.needs_redraw = true;
                return InputAction::ScrollToBottom;
            }
            'g' => {
                state.needs_redraw = true;
                return InputAction::ScrollToTop;
            }
            _ => return InputAction::Consumed,
        }
    }

    // Permission feedback input focus (AC5): printable chars + backspace handled.
    // Reject control characters, tab, and newline (spec: single-line, multi-line disallowed).
    if state.focus
        == FocusState::Overlay(OverlayType::Confirmation(
            ConfirmationType::PermissionFeedback,
        ))
    {
        if c.is_control() || c == '\t' || c == '\n' || c == '\r' {
            return InputAction::Consumed;
        }
        return InputAction::FeedbackInputChar(c);
    }

    // Skill trust prompt focus: y/n/i (Story 5-2 AC4)
    if state.focus == FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::SkillTrust)) {
        match c {
            'y' => return InputAction::SkillTrustAccept,
            'n' => return InputAction::SkillTrustDecline,
            'i' => return InputAction::SkillTrustInspect,
            // AC6: chat-scroll preserved while the prompt is up. S16.8: migrated to InputActions.
            'j' => {
                state.needs_redraw = true;
                return InputAction::ScrollLineDown;
            }
            'k' => {
                state.needs_redraw = true;
                return InputAction::ScrollLineUp;
            }
            _ => return InputAction::Consumed,
        }
    }

    // Skill trust inspect mode: all char keys consumed (read-only view)
    if state.focus
        == FocusState::Overlay(OverlayType::Confirmation(
            ConfirmationType::SkillTrustInspect,
        ))
    {
        return InputAction::Consumed;
    }

    // Plan approval card focus: y/a/n/e (Story 6-0d AC4)
    if state.focus == FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::PlanApproval))
    {
        match c {
            'y' => return InputAction::PlanApproveNormal,
            'a' => return InputAction::PlanApproveAutoEdit,
            'n' => return InputAction::PlanReject,
            'e' => return InputAction::PlanRevise,
            _ => return InputAction::Consumed,
        }
    }

    // Inline PlanCard: y/e/n when focus is Chat and card is pending (Story 6-1a AC7).
    // Overlay PlanApproval (6-0d) takes precedence — handled above.
    if state.pending_plan_card.is_some() {
        match state.focus {
            FocusState::Chat | FocusState::Input => match c {
                'y' => return InputAction::PlanCardApprove,
                'n' => return InputAction::PlanCardReject,
                'e' => return InputAction::PlanCardEdit,
                _ => {}
            },
            _ => {}
        }
    }

    // AskUserQuestion focus: type into question input buffer
    if state.focus == FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Question)) {
        if let Some(ref mut aq) = state.ask_user_question {
            aq.input_buffer.push(c);
            aq.cursor_position += 1;
            state.needs_redraw = true;
        }
        return InputAction::Consumed;
    }

    // Any keypress dismisses an active peek overlay (AC5)
    if state.focus == FocusState::Chat {
        let had_peek = state.tool_block_states.values().any(|tbs| tbs.peek_active);
        if had_peek {
            for tbs in state.tool_block_states.values_mut() {
                tbs.peek_active = false;
            }
            state.needs_redraw = true;
            return InputAction::Consumed;
        }
    }

    match state.focus {
        FocusState::Input => {
            // When autocomplete is active, intercept characters to update filter.
            // Exception: a space in SlashCommand mode terminates the command name
            // and begins arguments (e.g. `/export file.md`). Slash-command names
            // cannot contain spaces — the parser uses `split_whitespace().next()` —
            // so dismissing here lets ENTER submit `/cmd args` as a whole.
            // Without this, the ENTER handler below (DomainKey::Enter arm) consumes
            // the submit and the command never dispatches.
            if state.autocomplete.active {
                if matches!(state.autocomplete.kind, AutocompleteKind::SlashCommand) && c == ' ' {
                    state.autocomplete.dismiss();
                    // Fall through to normal character insertion below.
                } else {
                    let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
                    state.input_buffer.insert(byte_pos, c);
                    state.cursor_position += 1;
                    // Extract filter text: everything after the trigger character
                    let trigger = state.autocomplete.trigger_position;
                    let filter: String = state
                        .input_buffer
                        .chars()
                        .skip(trigger + 1)
                        .take(state.cursor_position.saturating_sub(trigger + 1))
                        .collect();
                    // Signal that autocomplete filter needs updating
                    // The actual filtering is done by the event loop which has access to registries
                    state.autocomplete.filter_text = if state.autocomplete.kind
                        == AutocompleteKind::FileMention
                        && filter == "Agents/"
                    {
                        state.autocomplete.kind = AutocompleteKind::AgentMention;
                        String::new()
                    } else if state.autocomplete.kind == AutocompleteKind::AgentMention {
                        let after_slash = filter.strip_prefix("Agents/").unwrap_or(&filter);
                        after_slash.to_string()
                    } else {
                        filter
                    };
                    state.needs_redraw = true;
                    return InputAction::Consumed;
                }
            }

            // Detect '/' at position 0 → trigger slash command autocomplete
            if c == '/' && state.cursor_position == 0 && state.input_buffer.is_empty() {
                let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
                state.input_buffer.insert(byte_pos, c);
                state.cursor_position += 1;
                state.autocomplete.active = true;
                state.autocomplete.kind = AutocompleteKind::SlashCommand;
                state.autocomplete.trigger_position = 0;
                state.autocomplete.filter_text.clear();
                state.autocomplete.selected_index = 0;
                state.autocomplete.scroll_offset = 0;
                // Suggestions will be populated by the event loop (lazy loading)
                state.needs_redraw = true;
                return InputAction::Consumed;
            }

            // Detect '@' anywhere → trigger file mention autocomplete
            if c == '@' {
                let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
                state.input_buffer.insert(byte_pos, c);
                let trigger_pos = state.cursor_position;
                state.cursor_position += 1;
                state.autocomplete.active = true;
                state.autocomplete.kind = AutocompleteKind::FileMention;
                state.autocomplete.trigger_position = trigger_pos;
                state.autocomplete.filter_text.clear();
                state.autocomplete.selected_index = 0;
                state.autocomplete.scroll_offset = 0;
                // Suggestions will be populated by the event loop
                state.needs_redraw = true;
                return InputAction::Consumed;
            }

            let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
            state.input_buffer.insert(byte_pos, c);
            state.cursor_position += 1;
            state.needs_redraw = true;
            InputAction::Consumed
        }
        FocusState::Chat if state.task_panel_state.drill_down_task.is_some() => {
            let n = state.task_panel_state.drill_down_task.unwrap();
            match c {
                'c' => {
                    let plan_id = state.task_panel_state.last_executed_plan_id.clone();
                    InputAction::CopyTaskResult {
                        plan_id,
                        task_number: n,
                    }
                }
                // Story 6.4: status-conditional dispatch — gating in event_loop.rs
                'r' => InputAction::TaskRetry(n),
                's' => InputAction::TaskSkip(n),
                'e' => InputAction::TaskEdit(n),
                'p' => InputAction::TaskPause(n),
                // 6-3 AC7: scroll the result body within the drill-down view.
                // Widget clamps the offset against actual content height.
                'j' => {
                    state.task_panel_state.detail_scroll_offset = state
                        .task_panel_state
                        .detail_scroll_offset
                        .saturating_add(1);
                    state.needs_redraw = true;
                    InputAction::Consumed
                }
                'k' => {
                    state.task_panel_state.detail_scroll_offset = state
                        .task_panel_state
                        .detail_scroll_offset
                        .saturating_sub(1);
                    state.needs_redraw = true;
                    InputAction::Consumed
                }
                'g' => {
                    state.task_panel_state.detail_scroll_offset = 0;
                    state.needs_redraw = true;
                    InputAction::Consumed
                }
                'G' => {
                    state.task_panel_state.detail_scroll_offset = u16::MAX;
                    state.needs_redraw = true;
                    InputAction::Consumed
                }
                _ => InputAction::Ignored,
            }
        }
        FocusState::Chat => {
            // Story 16.8 AC2: reset pending_g on any non-`g` key (falls through
            // to normal handler — unlike z/bracket prefixes which discard non-matching keys).
            if state.pending_g && c != 'g' {
                state.pending_g = false;
            }
            match c {
                // --- Story 16.6: vim z-prefix chord state machine (AC10) ---
                _z if state.pending_z => {
                    state.pending_z = false;
                    // P8: Let 'g' through — user intended jump-to-top, not a cancelled z-chord.
                    // 'g' is also reserved for future z-g chord expansion; consume it
                    // as ScrollToTop rather than as a discarded cancelled-chord keystroke.
                    state.needs_redraw = true;
                    tracing::debug!("vim z-prefix chord: z + '{}'", c);
                    return match c {
                        'a' => InputAction::FoldToggleAtFocus,
                        'c' => InputAction::CollapseFocus,
                        'o' => InputAction::ExpandFocus,
                        'M' => InputAction::CollapseAllTurns,
                        'R' => InputAction::ExpandAllTurns,
                        's' => InputAction::ToggleSummaryTier,
                        'z' => InputAction::RecenterAnchor,
                        'g' => InputAction::ScrollToTop,
                        _ => {
                            tracing::debug!("z-prefix chord cancelled with '{}'", c);
                            InputAction::Consumed
                        }
                    };
                }
                // --- Story 16.6: vim bracket-prefix chord state machine (AC10) ---
                // S16.8 preflight rebinding (2026-05-03): added `]P` arm — the new home
                // for `JumpToLatestProseAnchor` (relocated from S16.6's `G` binding so
                // S16.8 can return `G` to vim-bottom semantic per ADR-16-03). Bracket
                // leader has no first-key side effect, so `]P` produces no flicker
                // unlike a hypothetical `gp` chord (single-`g` would jump to top first).
                // Mnemonic: `]` family for "forward to last X"; capital `P` = "Prose".
                _b if state.pending_bracket.is_some() => {
                    let leader = state.pending_bracket.take().unwrap();
                    state.needs_redraw = true;
                    tracing::debug!("vim bracket chord: {} + '{}'", leader, c);
                    return match (leader, c) {
                        (']', ']') => InputAction::JumpProseAnchor(Direction::Down),
                        ('[', '[') => InputAction::JumpProseAnchor(Direction::Up),
                        (']', 'P') => InputAction::JumpToLatestProseAnchor,
                        _ => {
                            tracing::debug!(
                                "bracket-prefix chord cancelled with '{}{}'",
                                leader,
                                c
                            );
                            InputAction::Consumed
                        }
                    };
                }
                // --- Story 16.6: z / [ / ] chord leaders (AC10) ---
                'z' => {
                    state.pending_z = true;
                    state.needs_redraw = true;
                    InputAction::Consumed
                }
                ']' => {
                    state.pending_bracket = Some(']');
                    state.needs_redraw = true;
                    InputAction::Consumed
                }
                '[' => {
                    state.pending_bracket = Some('[');
                    state.needs_redraw = true;
                    InputAction::Consumed
                }
                // AC4: Large image confirmation
                'y' if state.pending_large_image.is_some() => {
                    return InputAction::ImageConfirmAttach;
                }
                'n' if state.pending_large_image.is_some() => {
                    return InputAction::ImageConfirmCancel;
                }
                'i' => {
                    state.focus = FocusState::Input;
                    state.needs_redraw = true;
                    InputAction::Consumed
                }
                'q' => InputAction::Quit,
                // j = scroll down (toward newer content). Story 16.8, AC7:
                // migrated from direct mutation to InputAction emission;
                // event-loop dispatcher calls dispatch_view_scroll(LineDown).
                'j' => {
                    state.needs_redraw = true;
                    InputAction::ScrollLineDown
                }
                // k = scroll up (toward older content). Story 16.8, AC7:
                // migrated from direct mutation to InputAction emission;
                // event-loop dispatcher calls dispatch_view_scroll(LineUp).
                'k' => {
                    state.needs_redraw = true;
                    InputAction::ScrollLineUp
                }
                // G = jump to bottom (vim-native semantic restored).
                // Story 16.6 had bound G to JumpToLatestProseAnchor (AC6 override);
                // S16.8 preflight rebinding (2026-05-03) returned G to vim-bottom per
                // ADR-16-03 (Anchor as Explicit User Investment). The displaced
                // S16.8 binding moved to the `]P` bracket chord.
                // Mode-aware dispatcher: Pinned → no-op + teaching toast; else ScrollToBottom.
                // See event_loop.rs dispatcher for the mode check (AC3 + AC15).
                'G' => {
                    state.needs_redraw = true;
                    InputAction::ScrollToBottom
                }
                // g = jump to top (mirror of G). Single-`g` immediately fires
                // ScrollToTop (legacy alias preserved from Story 1.4) AND sets
                // pending_g so a follow-up `g` is detected as `gg` chord.
                // Idempotent: if already at top, ScrollToTop is a no-op.
                // S16.8 Task 6, AC2.
                'g' => {
                    let was_gg = state.pending_g;
                    // P6: On gg chord, the first g already jumped to top — second g
                    // is idempotent, so skip the redundant dispatch.
                    state.pending_g = !was_gg;
                    state.needs_redraw = true;
                    if was_gg {
                        tracing::debug!("gg chord: second g, idempotent skip");
                        InputAction::Consumed
                    } else {
                        InputAction::ScrollToTop
                    }
                }
                // J = jump to next content block boundary. S16.8, AC7: migrated to BlockJump.
                'J' => {
                    if let Some(new_offset) = find_next_boundary(
                        state.scroll_snapshot,
                        &state.block_boundaries,
                        Direction::Down,
                        state.total_content_height,
                        state.viewport_height as usize,
                    ) {
                        state.needs_redraw = true;
                        InputAction::BlockJump {
                            offset: new_offset,
                            auto_scroll: new_offset == 0,
                        }
                    } else {
                        InputAction::Consumed
                    }
                }
                // K = jump to previous content block boundary
                'K' => {
                    if let Some(new_offset) = find_next_boundary(
                        state.scroll_snapshot,
                        &state.block_boundaries,
                        Direction::Up,
                        state.total_content_height,
                        state.viewport_height as usize,
                    ) {
                        state.needs_redraw = true;
                        InputAction::BlockJump {
                            offset: new_offset,
                            auto_scroll: false,
                        }
                    } else {
                        InputAction::Consumed
                    }
                }
                // { = jump to previous user message
                '{' => {
                    if let Some(new_offset) = find_next_boundary(
                        state.scroll_snapshot,
                        &state.user_message_boundaries,
                        Direction::Up,
                        state.total_content_height,
                        state.viewport_height as usize,
                    ) {
                        state.needs_redraw = true;
                        InputAction::BlockJump {
                            offset: new_offset,
                            auto_scroll: false,
                        }
                    } else {
                        InputAction::Consumed
                    }
                }
                // } = jump to next user message
                '}' => {
                    if let Some(new_offset) = find_next_boundary(
                        state.scroll_snapshot,
                        &state.user_message_boundaries,
                        Direction::Down,
                        state.total_content_height,
                        state.viewport_height as usize,
                    ) {
                        state.needs_redraw = true;
                        InputAction::BlockJump {
                            offset: new_offset,
                            auto_scroll: new_offset == 0,
                        }
                    } else {
                        InputAction::Consumed
                    }
                }
                // ? = toggle help overlay
                // Covers: FR108, UX-DR94 (AC1: ? opens help from any non-Input focus)
                '?' => {
                    let prior = state.focus.clone();
                    state.help_overlay.open(prior);
                    state.focus = FocusState::Overlay(OverlayType::Help);
                    state.needs_redraw = true;
                    return InputAction::Consumed;
                }
                // c = copy focused content to clipboard
                // Covers: FR116, UX-DR68 (AC6, AC7, AC8, AC9)
                'c' => {
                    return InputAction::CopyToClipboard(String::new());
                }
                // p = peek preview on focused collapsed tool block
                'p' => {
                    if let Some(ref tool_id) = state.focused_tool_id {
                        let entry = state.tool_block_states.entry(tool_id.clone()).or_default();
                        if entry.collapsed {
                            entry.peek_active = !entry.peek_active;
                            let tab_id = state.active_tab_id;
                            state.tab_render_state(tab_id).tool_block_states_version = state
                                .tab_render_state(tab_id)
                                .tool_block_states_version
                                .wrapping_add(1);
                            state.needs_redraw = true;
                        }
                    }
                    InputAction::Consumed
                }
                // 1-9 = direct tab switch (AC2: number key direct switch)
                '1'..='9' => {
                    let n = (c as u8 - b'0') as usize;
                    InputAction::SwitchToTab(n)
                }
                // f = fork conversation at the currently focused message (Story 4-3a, AC1)
                'f' => InputAction::ForkAtMessage,
                // R = rewind conversation to the currently focused message (Story 4-3b, AC1; UX-DR90)
                'R' => InputAction::RewindAtMessage,
                // m = toggle bookmark on the focused message (Story 4-4, AC8; UX-DR91)
                'm' => InputAction::ToggleBookmark,
                // ' = open the bookmark list panel (Story 4-4, AC10; UX-DR91)
                '\'' => InputAction::OpenBookmarkList,
                _ => InputAction::Ignored,
            }
        }
        FocusState::Sidebar {
            panel: _panel,
            selected: _selected,
        } => {
            // Story 6.4: reorder-mode intercept — override j/k/↑/↓/Enter/Esc when reorder active
            if state.task_panel_state.reorder_mode_for.is_some()
                && _panel == crate::domain::models::visual::PanelType::Tasks
            {
                state.needs_redraw = true;
                return match c {
                    'j' | '\u{2193}' => InputAction::TaskReorderMove(Direction::Down),
                    'k' | '\u{2191}' => InputAction::TaskReorderMove(Direction::Up),
                    '\r' => InputAction::TaskReorderCommit,
                    '\u{1b}' => InputAction::TaskReorderCancel,
                    _ => InputAction::Ignored,
                };
            }
            match c {
                'j' => {
                    let moved = if _panel == crate::domain::models::visual::PanelType::Tasks {
                        let max = state.task_panel_max_index();
                        if state.task_panel_state.selected_index < max {
                            state.task_panel_state.selected_index += 1;
                            state.sidebar_selected = state.task_panel_state.selected_index;
                            true
                        } else {
                            false
                        }
                    } else if state.sidebar_entry_count > 0 {
                        let max = state.sidebar_entry_count - 1;
                        if state.sidebar_selected < max {
                            state.sidebar_selected += 1;
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if moved {
                        state.focus = FocusState::Sidebar {
                            panel: _panel,
                            selected: state.sidebar_selected,
                        };
                        state.needs_redraw = true;
                    }
                    InputAction::Consumed
                }
                'k' => {
                    let moved = if _panel == crate::domain::models::visual::PanelType::Tasks {
                        if state.task_panel_state.selected_index > 0 {
                            state.task_panel_state.selected_index -= 1;
                            state.sidebar_selected = state.task_panel_state.selected_index;
                            true
                        } else {
                            false
                        }
                    } else if state.sidebar_selected > 0 {
                        state.sidebar_selected -= 1;
                        true
                    } else {
                        false
                    };
                    if moved {
                        state.focus = FocusState::Sidebar {
                            panel: _panel,
                            selected: state.sidebar_selected,
                        };
                        state.needs_redraw = true;
                    }
                    InputAction::Consumed
                }
                'x' if _panel == crate::domain::models::visual::PanelType::Tasks => {
                    state.needs_redraw = true;
                    InputAction::TaskCancelPlan
                }
                'p' if _panel == crate::domain::models::visual::PanelType::Tasks => {
                    state.needs_redraw = true;
                    // Panel `p`: dispatch with selected_index (0-based); event_loop resolves to task number
                    InputAction::TaskPause(state.task_panel_state.selected_index as u32)
                }
                's' if _panel == crate::domain::models::visual::PanelType::Tasks => {
                    state.needs_redraw = true;
                    // Panel `s`: dispatch with selected_index (0-based); event_loop resolves to task number
                    InputAction::TaskSkip(state.task_panel_state.selected_index as u32)
                }
                'd' if _panel == crate::domain::models::visual::PanelType::History => {
                    InputAction::DeleteSidebarConversation
                }
                '/' if _panel == crate::domain::models::visual::PanelType::History => {
                    state.cross_search = crate::adapters::tui::state::CrossSearchState::new();
                    state.cross_search.active = true;
                    state.focus = FocusState::Overlay(OverlayType::CrossSearch);
                    state.needs_redraw = true;
                    InputAction::OpenCrossSearch
                }
                'q' => InputAction::Quit,
                _ => InputAction::Ignored,
            }
        }
        FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::DeleteConfirmation(_))) => {
            match c {
                'y' | 'Y' => InputAction::ConfirmDelete,
                'n' | 'N' => InputAction::CancelDelete,
                _ => InputAction::Consumed,
            }
        }
        // Fork confirmation: y = confirm, n = cancel (Story 4-3a, AC1)
        FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Fork)) => match c {
            'y' => InputAction::ForkConfirm,
            'n' => InputAction::ForkCancel,
            _ => InputAction::Consumed,
        },
        // Rewind confirmation: y = confirm, f = fork instead, n = cancel (Story 4-3b, AC1)
        FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Rewind)) => match c {
            'y' => InputAction::RewindConfirm,
            'f' => InputAction::RewindForkInstead,
            'n' => InputAction::RewindCancel,
            _ => InputAction::Consumed,
        },
        // Export-overwrite confirmation: y = overwrite, n = cancel (Story 4-4 AC12).
        FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::ExportOverwrite(_))) => {
            match c {
                'y' | 'Y' => InputAction::ConfirmExportOverwrite,
                'n' | 'N' => InputAction::CancelExportOverwrite,
                _ => InputAction::Consumed,
            }
        }
        // Search overlay printable-key handling (Story 4-4 AC2, AC3).
        //
        // Dispatches on `search_state.substate`:
        // - Typing: append char to query, signal re-scan via SearchQueryChanged
        // - Navigating: `n` / `N` navigate, any other printable returns to
        //   Typing AND applies the char so the user can refine without Esc
        FocusState::Overlay(OverlayType::Search) => {
            use crate::adapters::tui::state::SearchSubstate;
            match state.search_state.substate {
                SearchSubstate::Typing => {
                    state.search_state.query.push(c);
                    state.needs_redraw = true;
                    InputAction::SearchQueryChanged
                }
                SearchSubstate::Navigating => match c {
                    'n' => InputAction::SearchNext,
                    'N' => InputAction::SearchPrev,
                    other => {
                        // Return to Typing and apply the char.
                        state.search_state.substate = SearchSubstate::Typing;
                        state.search_state.query.push(other);
                        state.needs_redraw = true;
                        InputAction::SearchReturnToTyping
                    }
                },
            }
        }
        // Cross-search overlay printable-key handling (Story 4-4 AC5).
        // j/k navigate results; every other printable char extends the query.
        FocusState::Overlay(OverlayType::CrossSearch) => match c {
            'j' => {
                if !state.cross_search.results.is_empty() {
                    let last = state.cross_search.results.len() - 1;
                    state.cross_search.selected = (state.cross_search.selected + 1).min(last);
                }
                state.needs_redraw = true;
                InputAction::Consumed
            }
            'k' => {
                state.cross_search.selected = state.cross_search.selected.saturating_sub(1);
                state.needs_redraw = true;
                InputAction::Consumed
            }
            other => {
                state.cross_search.query.push(other);
                state.needs_redraw = true;
                InputAction::CrossSearchQueryChanged
            }
        },
        // Bookmark list panel printable-key handling (Story 4-4 AC10).
        // j/k navigate, d deletes, u undoes (within 5 s), Enter jumps, Esc closes.
        FocusState::Overlay(OverlayType::BookmarkList) => match c {
            'j' => {
                let last = state.bookmark_list_count.saturating_sub(1);
                state.bookmark_list_selected =
                    state.bookmark_list_selected.saturating_add(1).min(last);
                state.needs_redraw = true;
                InputAction::Consumed
            }
            'k' => {
                state.bookmark_list_selected = state.bookmark_list_selected.saturating_sub(1);
                state.needs_redraw = true;
                InputAction::Consumed
            }
            'd' => InputAction::DeleteBookmark,
            'u' => InputAction::UndoBookmarkDelete,
            _ => InputAction::Consumed,
        },
        FocusState::Overlay(_) => InputAction::Ignored,
    }
}

fn handle_special_key(state: &mut TuiState, key: DomainKey) -> InputAction {
    // Cancel pending chord on any special key other than Ctrl+K itself.
    if state.chord_leader_active && key != DomainKey::CtrlK {
        state.chord_leader_active = false;
        state.needs_redraw = true;
    }
    // Cancel pending vim chords on any special key.
    if state.pending_z || state.pending_bracket.is_some() || state.pending_g {
        state.pending_z = false;
        state.pending_bracket = None;
        state.pending_g = false;
        state.needs_redraw = true;
    }

    // Ctrl+K chord leader: set flag and consume, regardless of overlay.
    // UX-DR-GLOBAL-CHORD-PREFIX — the next char key dispatches through
    // FeedbackAction::dispatch_key().
    if key == DomainKey::CtrlK {
        if state.which_key.active {
            state.focus = state.which_key.dismiss().unwrap_or(FocusState::Input);
            state.needs_redraw = true;
        }
        state.chord_leader_active = true;
        state.needs_redraw = true;
        return InputAction::Consumed;
    }

    // Delete confirmation: Esc → Cancel
    if matches!(
        state.focus,
        FocusState::Overlay(OverlayType::Confirmation(
            ConfirmationType::DeleteConfirmation(_)
        ))
    ) {
        return match key {
            DomainKey::Esc => InputAction::CancelDelete,
            _ => InputAction::Consumed,
        };
    }

    // Fork confirmation: Esc → Cancel (Story 4-3a, AC1)
    if state.focus == FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Fork)) {
        return match key {
            DomainKey::Esc => InputAction::ForkCancel,
            _ => InputAction::Consumed,
        };
    }

    // Rewind confirmation: Esc → Cancel (Story 4-3b, AC1)
    if state.focus == FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Rewind)) {
        return match key {
            DomainKey::Esc => InputAction::RewindCancel,
            _ => InputAction::Consumed,
        };
    }

    // Export-overwrite confirmation: Esc → Cancel (Story 4-4 AC12)
    if matches!(
        state.focus,
        FocusState::Overlay(OverlayType::Confirmation(
            ConfirmationType::ExportOverwrite(_)
        ))
    ) {
        return match key {
            DomainKey::Esc => InputAction::CancelExportOverwrite,
            _ => InputAction::Consumed,
        };
    }

    // Permission prompt: Esc → Deny
    if state.focus == FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Permission)) {
        return match key {
            DomainKey::Esc => InputAction::PermissionDeny,
            _ => InputAction::Consumed,
        };
    }

    // Skill trust prompt: Esc → Decline (Story 5-2 AC4)
    if state.focus == FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::SkillTrust)) {
        return match key {
            DomainKey::Esc => InputAction::SkillTrustDecline,
            _ => InputAction::Consumed,
        };
    }

    // Skill trust inspect mode: Esc returns to trust prompt (Story 5-2 AC4)
    if state.focus
        == FocusState::Overlay(OverlayType::Confirmation(
            ConfirmationType::SkillTrustInspect,
        ))
    {
        return match key {
            DomainKey::Esc => {
                state.skill_trust_inspect_mode = false;
                state.focus =
                    FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::SkillTrust));
                state.needs_redraw = true;
                InputAction::Consumed
            }
            _ => InputAction::Consumed,
        };
    }

    // Permission feedback input: Enter/Backspace/Esc (AC5)
    if state.focus
        == FocusState::Overlay(OverlayType::Confirmation(
            ConfirmationType::PermissionFeedback,
        ))
    {
        return match key {
            DomainKey::Enter => InputAction::FeedbackInputSubmit,
            DomainKey::Backspace => InputAction::FeedbackInputBackspace,
            DomainKey::Esc => InputAction::FeedbackInputCancel,
            _ => InputAction::Consumed,
        };
    }

    // AskUserQuestion: Enter submits, Backspace deletes, Esc cancels
    if state.focus == FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Question)) {
        return match key {
            DomainKey::Enter => {
                if let Some(ref mut aq) = state.ask_user_question {
                    if aq.input_buffer.is_empty() {
                        return InputAction::Consumed; // Don't submit empty answer
                    }
                    let answer = std::mem::take(&mut aq.input_buffer);
                    aq.submitted_answer = Some(answer.clone());
                    aq.cursor_position = 0;
                    state.needs_redraw = true;
                    InputAction::SubmitQuestionAnswer(answer)
                } else {
                    InputAction::Consumed
                }
            }
            DomainKey::Backspace => {
                if let Some(ref mut aq) = state.ask_user_question {
                    if aq.cursor_position > 0 {
                        aq.cursor_position -= 1;
                        aq.input_buffer.pop();
                        state.needs_redraw = true;
                    }
                }
                InputAction::Consumed
            }
            DomainKey::Esc => {
                // Cancel question — dismiss and drop oneshot sender so run_turn gets RecvError
                state.ask_user_question = None;
                drop(state.question_response_tx.take());
                state.focus = FocusState::Input;
                state.needs_redraw = true;
                InputAction::Consumed
            }
            _ => InputAction::Consumed,
        };
    }

    // Any keypress dismisses an active peek overlay (AC5)
    if state.focus == FocusState::Chat {
        let had_peek = state.tool_block_states.values().any(|tbs| tbs.peek_active);
        if had_peek {
            for tbs in state.tool_block_states.values_mut() {
                tbs.peek_active = false;
            }
            state.needs_redraw = true;
            return InputAction::Consumed;
        }
    }

    // Help overlay: route all special keys when active
    // Covers: FR108, UX-DR94
    if state.focus == FocusState::Overlay(OverlayType::Help) {
        return handle_help_overlay_key(state, key);
    }

    // Model selector overlay: route arrow keys, Enter, Esc
    // Story 7.2 AC1, AC3
    if state.focus == FocusState::Overlay(OverlayType::ModelSelector) {
        return handle_model_selector_key(state, key);
    }

    // Usage panel overlay (Story 7.5 AC3): Esc / Ctrl+C dismisses, arrows cycle sections.
    if state.focus == FocusState::Overlay(OverlayType::UsagePanel) {
        return handle_usage_panel_key(state, key);
    }

    // Command palette overlay handling — intercept keys when palette is active
    // Covers: UX-DR18
    if state.command_palette.active {
        return handle_command_palette_key(state, key);
    }

    // Which-key overlay handling — any special key dismisses
    // Covers: UX-DR19
    if state.which_key.active {
        // Special keys (non-char) dismiss which-key without action (AC6)
        state.focus = state.which_key.dismiss().unwrap_or(FocusState::Input);
        state.needs_redraw = true;
        return InputAction::Consumed;
    }

    // Autocomplete overlay handling — intercept keys when autocomplete is active
    // Covers: UX-DR75 (autocomplete)
    if state.autocomplete.active && state.focus == FocusState::Input {
        let result = handle_autocomplete_key(state, key);
        if result != InputAction::Ignored {
            return result;
        }
        // Ignored = autocomplete dismissed, fall through to normal key handling
    }

    // Reverse search overlay handling
    // Covers: UX-DR74 (reverse search)
    // P16: Allow Ctrl+P and Ctrl+X to dismiss reverse search and open their overlays (Tier-2).
    if state.reverse_search.active && !matches!(key, DomainKey::CtrlP | DomainKey::CtrlX) {
        return handle_reverse_search_key(state, key);
    }

    // Within-conversation search overlay handling (Story 4-4 AC3, AC4).
    // Tier-1 overlay: consumes ALL special keys (including Ctrl+P / Ctrl+X)
    // while active, matching the fork/rewind/delete confirmation pattern.
    // Users must Esc first before opening the palette or which-key chords.
    if state.focus == FocusState::Overlay(OverlayType::Search) {
        return handle_search_overlay_key(state, key);
    }

    // Cross-search overlay special-key handling (Story 4-4 AC5, AC6).
    if state.focus == FocusState::Overlay(OverlayType::CrossSearch) {
        return match key {
            DomainKey::Esc => InputAction::CloseCrossSearch,
            DomainKey::Enter => {
                if !state.cross_search.results.is_empty() {
                    InputAction::OpenCrossSearchResult
                } else {
                    InputAction::Consumed
                }
            }
            DomainKey::Backspace => {
                state.cross_search.query.pop();
                state.needs_redraw = true;
                InputAction::CrossSearchQueryChanged
            }
            DomainKey::Up => {
                state.cross_search.selected = state.cross_search.selected.saturating_sub(1);
                state.needs_redraw = true;
                InputAction::Consumed
            }
            DomainKey::Down => {
                if !state.cross_search.results.is_empty() {
                    let last = state.cross_search.results.len() - 1;
                    state.cross_search.selected = (state.cross_search.selected + 1).min(last);
                }
                state.needs_redraw = true;
                InputAction::Consumed
            }
            _ => InputAction::Consumed,
        };
    }

    // Bookmark list panel special-key handling (Story 4-4 AC10).
    if state.focus == FocusState::Overlay(OverlayType::BookmarkList) {
        return match key {
            DomainKey::Esc => InputAction::CloseBookmarkList,
            DomainKey::Enter => InputAction::JumpToBookmark,
            DomainKey::Delete | DomainKey::Backspace => InputAction::DeleteBookmark,
            DomainKey::Up => {
                state.bookmark_list_selected = state.bookmark_list_selected.saturating_sub(1);
                state.needs_redraw = true;
                InputAction::Consumed
            }
            DomainKey::Down => {
                let last = state.bookmark_list_count.saturating_sub(1);
                state.bookmark_list_selected =
                    state.bookmark_list_selected.saturating_add(1).min(last);
                state.needs_redraw = true;
                InputAction::Consumed
            }
            _ => InputAction::Consumed,
        };
    }

    match key {
        DomainKey::Esc => {
            // In multiline mode with content: submit message (alternative send)
            // If navigating history, cancel navigation instead of submitting.
            // Covers: UX-DR76
            if state.focus == FocusState::Input
                && state.multiline_mode
                && !state.input_buffer.is_empty()
            {
                if state.input_history.is_navigating() {
                    state.input_history.reset_navigation();
                    state.input_buffer.clear();
                    state.cursor_position = 0;
                    state.input_scroll_offset = 0;
                    state.needs_redraw = true;
                    return InputAction::Consumed;
                }
                return submit_message(state);
            }
            // Story 6.4: reorder-mode Esc — cancel and restore
            if state.focus == FocusState::Chat && state.task_panel_state.reorder_mode_for.is_some()
            {
                state.needs_redraw = true;
                return InputAction::TaskReorderCancel;
            }
            if state.focus == FocusState::Chat && state.task_panel_state.drill_down_task.is_some() {
                state.task_panel_state.drill_down_task = None;
                state.task_panel_state.expanded_detail = false;
                state.task_panel_state.detail_scroll_offset = 0;
                state.task_panel_state.detail_scroll_offset = 0;
                state.focus = FocusState::Sidebar {
                    panel: crate::domain::models::visual::PanelType::Tasks,
                    selected: state.task_panel_state.selected_index,
                };
                state.needs_redraw = true;
                return InputAction::Consumed;
            }
            state.focus = match state.focus {
                FocusState::Input => FocusState::Chat,
                FocusState::Chat => FocusState::Input,
                // AC11: Esc from Sidebar → Chat
                FocusState::Sidebar { .. } => FocusState::Chat,
                FocusState::Overlay(_) => FocusState::Input,
            };
            state.needs_redraw = true;
            InputAction::Consumed
        }

        // Shift+Enter: always insert newline (when terminal supports it)
        // Covers: UX-DR76 (Shift+Enter)
        DomainKey::ShiftEnter if state.focus == FocusState::Input => {
            insert_newline(state);
            InputAction::Consumed
        }

        // Alt+Enter: insert newline (VS Code terminal alternative to Shift+Enter)
        // Covers: Sprint Change Proposal 2026-04-08, UX-DR76 amendment, AC#1
        DomainKey::AltEnter if state.focus == FocusState::Input => {
            insert_newline(state);
            InputAction::Consumed
        }

        // Alt+M: toggle multi-line mode (VS Code terminal alternative to Ctrl+E)
        // Covers: Sprint Change Proposal 2026-04-08, UX-DR76 amendment, AC#2
        DomainKey::AltM if state.focus == FocusState::Input => {
            state.multiline_mode = !state.multiline_mode;
            state.needs_redraw = true;
            InputAction::Consumed
        }

        // Alt+V: paste image (or text) from the system clipboard.
        // The event loop receives RequestClipboardPaste and handles it async
        // via ClipboardPort, then re-enters handle_input with ImagePaste/Paste.
        DomainKey::AltV => InputAction::RequestClipboardPaste,

        // Ctrl+Enter: submit in multiline mode
        // Covers: UX-DR76
        DomainKey::CtrlEnter if state.focus == FocusState::Input => {
            if !state.input_buffer.is_empty() {
                submit_message(state)
            } else {
                InputAction::Consumed
            }
        }

        // Ctrl+P: open command palette (Tier-1 overlays blocked, Tier-2 overlays allowed)
        // P16: ReverseSearch is Tier-2 — Ctrl+P dismisses it and opens palette.
        // Covers: UX-DR18
        DomainKey::CtrlP
            if !matches!(
                state.focus,
                FocusState::Overlay(
                    OverlayType::CommandPalette
                        | OverlayType::WhichKey
                        | OverlayType::ModelSelector
                        | OverlayType::Help
                        | OverlayType::ProfileSwitcher
                        | OverlayType::Confirmation(_)
                )
            ) =>
        {
            // Dismiss autocomplete if active — only one overlay at a time
            if state.autocomplete.active {
                state.autocomplete.dismiss();
            }
            // Dismiss reverse search (Tier-2) before opening palette
            if state.reverse_search.active {
                state.reverse_search.active = false;
            }
            // Determine prior focus for restoration: if coming from ReverseSearch overlay, restore Input
            let prior_focus = match &state.focus {
                FocusState::Overlay(OverlayType::ReverseSearch) => FocusState::Input,
                FocusState::Overlay(OverlayType::Autocomplete(_)) => FocusState::Input,
                other => other.clone(),
            };
            state.command_palette.open(prior_focus);
            state.focus = FocusState::Overlay(OverlayType::CommandPalette);
            state.needs_redraw = true;
            InputAction::Consumed
        }

        // Ctrl+X: open which-key (Tier-1 overlays blocked, Tier-2 overlays allowed)
        // P16: ReverseSearch is Tier-2 — Ctrl+X dismisses it and opens which-key.
        // Covers: UX-DR19, UX-DR60
        DomainKey::CtrlX
            if !matches!(
                state.focus,
                FocusState::Overlay(
                    OverlayType::CommandPalette
                        | OverlayType::WhichKey
                        | OverlayType::ModelSelector
                        | OverlayType::Help
                        | OverlayType::ProfileSwitcher
                        | OverlayType::Confirmation(_)
                )
            ) =>
        {
            // Dismiss autocomplete if active — only one overlay at a time
            if state.autocomplete.active {
                state.autocomplete.dismiss();
            }
            // Dismiss reverse search (Tier-2) before opening which-key
            if state.reverse_search.active {
                state.reverse_search.active = false;
            }
            // Determine prior focus for restoration
            let prior_focus = match &state.focus {
                FocusState::Overlay(OverlayType::ReverseSearch) => FocusState::Input,
                FocusState::Overlay(OverlayType::Autocomplete(_)) => FocusState::Input,
                other => other.clone(),
            };
            state.which_key.open(prior_focus);
            state.focus = FocusState::Overlay(OverlayType::WhichKey);
            state.needs_redraw = true;
            InputAction::Consumed
        }

        // Ctrl+E: toggle multi-line mode
        // Covers: UX-DR76 (Ctrl+E fallback)
        DomainKey::CtrlE if state.focus == FocusState::Input => {
            state.multiline_mode = !state.multiline_mode;
            state.needs_redraw = true;
            InputAction::Consumed
        }

        // Ctrl+R: activate reverse search
        // Covers: UX-DR74
        DomainKey::CtrlR if state.focus == FocusState::Input => {
            // P16: Dismiss autocomplete if active — only one overlay at a time
            if state.autocomplete.active {
                state.autocomplete.dismiss();
            }
            state.reverse_search.active = true;
            state.reverse_search.query.clear();
            state.reverse_search.matches.clear();
            state.reverse_search.selected_match = 0;
            state.focus = FocusState::Overlay(OverlayType::ReverseSearch);
            state.needs_redraw = true;
            InputAction::Consumed
        }

        DomainKey::Backspace if state.focus == FocusState::Input => {
            if state.cursor_position > 0 {
                state.cursor_position -= 1;
                let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
                state.input_buffer.remove(byte_pos);
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }

        // Delete key: remove character at cursor position
        DomainKey::Delete if state.focus == FocusState::Input => {
            let total_chars = state.input_buffer.chars().count();
            if state.cursor_position < total_chars {
                let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
                state.input_buffer.remove(byte_pos);
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }

        DomainKey::Left if state.focus == FocusState::Input => {
            state.cursor_position = state.cursor_position.saturating_sub(1);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Right if state.focus == FocusState::Input => {
            if state.cursor_position < state.input_buffer.chars().count() {
                state.cursor_position += 1;
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }

        // Home: move to start of current line
        DomainKey::Home if state.focus == FocusState::Input => {
            let (row, _col) =
                input_box::cursor_to_row_col(&state.input_buffer, state.cursor_position);
            state.cursor_position = input_box::row_col_to_cursor(&state.input_buffer, row, 0);
            state.needs_redraw = true;
            InputAction::Consumed
        }

        // End: move to end of current line
        DomainKey::End if state.focus == FocusState::Input => {
            let (row, _col) =
                input_box::cursor_to_row_col(&state.input_buffer, state.cursor_position);
            let line_len = input_box::line_len_at_row(&state.input_buffer, row);
            state.cursor_position =
                input_box::row_col_to_cursor(&state.input_buffer, row, line_len);
            state.needs_redraw = true;
            InputAction::Consumed
        }

        // Up/Down in Input focus: multi-line navigation or history
        // Covers: UX-DR74, UX-DR76
        DomainKey::Up if state.focus == FocusState::Input => {
            let has_multiline_content = state.input_buffer.contains('\n');
            let (row, col) =
                input_box::cursor_to_row_col(&state.input_buffer, state.cursor_position);

            if has_multiline_content && row > 0 && !state.input_history.is_navigating() {
                // Move cursor up one line (only if not already navigating history)
                let target_col = col.min(input_box::line_len_at_row(&state.input_buffer, row - 1));
                state.cursor_position =
                    input_box::row_col_to_cursor(&state.input_buffer, row - 1, target_col);
                ensure_cursor_visible(state);
                state.needs_redraw = true;
            } else if state.input_buffer.is_empty()
                || state.input_history.is_navigating()
                || !has_multiline_content
            {
                // Navigate history
                let current = state.input_buffer.clone();
                if let Some(entry) = state.input_history.navigate_up(&current) {
                    state.input_buffer = entry.to_string();
                    state.cursor_position = state.input_buffer.chars().count();
                    state.input_scroll_offset = 0;
                    ensure_cursor_visible(state);
                    state.needs_redraw = true;
                }
            }
            InputAction::Consumed
        }

        DomainKey::Down if state.focus == FocusState::Input => {
            let has_multiline_content = state.input_buffer.contains('\n');
            let (row, col) =
                input_box::cursor_to_row_col(&state.input_buffer, state.cursor_position);
            let total_lines = input_box::line_count(&state.input_buffer);

            if has_multiline_content
                && row + 1 < total_lines
                && !state.input_history.is_navigating()
            {
                // Move cursor down one line (only if not already navigating history)
                let target_col = col.min(input_box::line_len_at_row(&state.input_buffer, row + 1));
                state.cursor_position =
                    input_box::row_col_to_cursor(&state.input_buffer, row + 1, target_col);
                ensure_cursor_visible(state);
                state.needs_redraw = true;
            } else if state.input_history.is_navigating() {
                // Navigate history forward
                if let Some(entry) = state.input_history.navigate_down() {
                    state.input_buffer = entry.to_string();
                    state.cursor_position = state.input_buffer.chars().count();
                    state.input_scroll_offset = 0;
                    ensure_cursor_visible(state);
                    state.needs_redraw = true;
                }
            }
            InputAction::Consumed
        }

        DomainKey::Enter
            if matches!(
                state.focus,
                FocusState::Sidebar {
                    panel: crate::domain::models::visual::PanelType::Tasks,
                    ..
                }
            ) && state.task_panel_state.reorder_mode_for.is_some() =>
        {
            state.needs_redraw = true;
            InputAction::TaskReorderCommit
        }

        DomainKey::Enter
            if matches!(
                state.focus,
                FocusState::Sidebar {
                    panel: crate::domain::models::visual::PanelType::Tasks,
                    ..
                }
            ) && state.task_panel_state.task_count > 0 =>
        {
            let task_number = (state.task_panel_state.selected_index + 1) as u32;
            state.task_panel_state.drill_down_task = Some(task_number);
            state.task_panel_state.expanded_detail = false;
            state.task_panel_state.detail_scroll_offset = 0;
            state.focus = FocusState::Chat;
            state.needs_redraw = true;
            InputAction::Consumed
        }

        DomainKey::Enter if matches!(state.focus, FocusState::Sidebar { .. }) => {
            // Open selected conversation — event loop resolves ID from session_index
            InputAction::OpenSidebarConversation
        }

        // 6-3 AC7: inside the task drill-down view, Enter toggles the
        // result body between half-viewport and full-viewport rendering.
        // Must precede the chat tool-block arm below so the drill-down
        // doesn't fall through to it (focus is still `Chat` post-drill-in).
        DomainKey::Enter
            if state.focus == FocusState::Chat
                && state.task_panel_state.drill_down_task.is_some() =>
        {
            state.task_panel_state.expanded_detail = !state.task_panel_state.expanded_detail;
            state.task_panel_state.detail_scroll_offset = 0;
            state.needs_redraw = true;
            InputAction::Consumed
        }

        DomainKey::Enter if state.focus == FocusState::Chat => {
            // Toggle collapse/expand on focused tool block
            if let Some(ref tool_id) = state.focused_tool_id {
                let entry = state.tool_block_states.entry(tool_id.clone()).or_default();
                entry.collapsed = !entry.collapsed;
                entry.peek_active = false;
                let tab_id = state.active_tab_id;
                state.tab_render_state(tab_id).height_cache.invalidate_all();
                state.tab_render_state(tab_id).tool_block_states_version = state
                    .tab_render_state(tab_id)
                    .tool_block_states_version
                    .wrapping_add(1);
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }

        // Enter in Input: behavior depends on multiline_mode
        // Covers: UX-DR76
        DomainKey::Enter if state.focus == FocusState::Input => {
            if state.multiline_mode {
                // In multiline mode, Enter inserts newline (only if buffer has content)
                if !state.input_buffer.is_empty() {
                    insert_newline(state);
                }
                InputAction::Consumed
            } else if !state.input_buffer.is_empty() {
                submit_message(state)
            } else {
                InputAction::Consumed
            }
        }

        DomainKey::CtrlC => InputAction::CancelOrQuit,
        // Ctrl+F — narrow-override per S16.8: page-down in Chat focus, search elsewhere
        // (Story 4-4 AC1, UX-DR86 preserved for non-Chat focus).
        DomainKey::CtrlF if state.focus == FocusState::Chat => InputAction::ScrollFullPageDown,
        DomainKey::CtrlF if state.focus == FocusState::Input && state.input_buffer.is_empty() => {
            let prior = state.focus.clone();
            state.search_state = crate::adapters::tui::state::SearchState::new();
            state.search_state.active = true;
            state.search_state.prior_focus = Some(prior);
            state.focus = FocusState::Overlay(OverlayType::Search);
            state.needs_redraw = true;
            InputAction::OpenSearch
        }
        // P11: Ctrl+F in Input with non-empty buffer — give user feedback
        // instead of silently consuming.
        DomainKey::CtrlF if state.focus == FocusState::Input && !state.input_buffer.is_empty() => {
            state.status = StatusState::Flash {
                message: "Clear input or press Esc to search".into(),
                remaining_ms: 1500,
            };
            state.needs_redraw = true;
            InputAction::Consumed
        }
        // Ctrl+D — scroll half page down (Chat focus only). Story 16.8, AC1.
        DomainKey::CtrlD if state.focus == FocusState::Chat => InputAction::ScrollHalfPageDown,
        // Ctrl+U — scroll half page up (Chat focus); clear-buffer in Input focus.
        // Story 16.8, AC1.
        DomainKey::CtrlU if state.focus == FocusState::Chat => InputAction::ScrollHalfPageUp,
        // Ctrl+B — scroll full page up (Chat focus only). Story 16.8, AC1.
        DomainKey::CtrlB if state.focus == FocusState::Chat => InputAction::ScrollFullPageUp,
        DomainKey::CtrlH => InputAction::ToggleSidebar,
        DomainKey::CtrlT => InputAction::NewTab,
        // Tab/focus cycling (AC11):
        // Story 16.6 AC5: Chat Tab now emits CycleInvocationInFocusedTurn first.
        // The event-loop dispatcher checks the guard (focused turn + expanded + >= 2 invocations)
        // and falls through to the legacy sidebar/tab-switch behavior when guard fails.
        DomainKey::Tab if state.focus == FocusState::Chat => {
            InputAction::CycleInvocationInFocusedTurn
        }
        DomainKey::Tab if matches!(state.focus, FocusState::Sidebar { .. }) => {
            state.focus = FocusState::Input;
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::ShiftTab if state.focus != FocusState::Input => InputAction::SwitchToPrevTab,
        DomainKey::ShiftTab if state.focus == FocusState::Input => InputAction::CycleMode,

        // Up/Down arrow keys for task panel navigation (mirror j/k behavior)
        // 6-3 AC7: arrow-key scroll inside the task drill-down result body.
        // Mirrors the j/k char handlers above; widget clamps the offset.
        DomainKey::Down
            if state.focus == FocusState::Chat
                && state.task_panel_state.drill_down_task.is_some() =>
        {
            state.task_panel_state.detail_scroll_offset = state
                .task_panel_state
                .detail_scroll_offset
                .saturating_add(1);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Up
            if state.focus == FocusState::Chat
                && state.task_panel_state.drill_down_task.is_some() =>
        {
            state.task_panel_state.detail_scroll_offset = state
                .task_panel_state
                .detail_scroll_offset
                .saturating_sub(1);
            state.needs_redraw = true;
            InputAction::Consumed
        }

        DomainKey::Up
            if matches!(
                state.focus,
                FocusState::Sidebar {
                    panel: crate::domain::models::visual::PanelType::Tasks,
                    ..
                }
            ) =>
        {
            let moved = if state.task_panel_state.selected_index > 0 {
                state.task_panel_state.selected_index -= 1;
                state.sidebar_selected = state.task_panel_state.selected_index;
                true
            } else {
                false
            };
            if moved {
                state.focus = FocusState::Sidebar {
                    panel: crate::domain::models::visual::PanelType::Tasks,
                    selected: state.sidebar_selected,
                };
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }
        DomainKey::Down
            if matches!(
                state.focus,
                FocusState::Sidebar {
                    panel: crate::domain::models::visual::PanelType::Tasks,
                    ..
                }
            ) =>
        {
            let moved = if state.task_panel_state.task_count > 0 {
                let max = state.task_panel_state.task_count.saturating_sub(1);
                if state.task_panel_state.selected_index < max {
                    state.task_panel_state.selected_index += 1;
                    state.sidebar_selected = state.task_panel_state.selected_index;
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if moved {
                state.focus = FocusState::Sidebar {
                    panel: crate::domain::models::visual::PanelType::Tasks,
                    selected: state.sidebar_selected,
                };
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }

        _ => InputAction::Ignored,
    }
}

/// Insert a newline at cursor position.
// Covers: UX-DR76
fn insert_newline(state: &mut TuiState) {
    let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
    state.input_buffer.insert(byte_pos, '\n');
    state.cursor_position += 1;
    ensure_cursor_visible(state);
    state.needs_redraw = true;
}

/// Submit the current input buffer as a message.
// Covers: UX-DR77
fn submit_message(state: &mut TuiState) -> InputAction {
    let text = state.input_buffer.clone();
    state.input_history.push(text.clone());
    state.input_history.reset_navigation();
    state.cursor_position = 0;
    state.input_scroll_offset = 0;
    state.autocomplete.dismiss();
    state.needs_redraw = true;

    let trimmed = text.trim_start();
    if let Some(after_at) = trimmed.strip_prefix("@Agents/") {
        let name_end = after_at
            .find(|c: char| c.is_whitespace())
            .unwrap_or(after_at.len());
        let agent_name = &after_at[..name_end];
        let trailing = after_at[name_end..].trim();
        let then_submit = if trailing.is_empty() {
            None
        } else {
            Some(trailing.to_string())
        };

        if agent_name == "default" {
            state.input_buffer.clear();
            return InputAction::ClearActiveAgent { then_submit };
        }
        if state.agent_registry.find(agent_name).is_none() {
            if !state.agent_registry.is_discovered() {
                state.pending_agent_activation =
                    Some((agent_name.to_string(), then_submit.clone()));
                return InputAction::AgentDiscoveryPending {
                    name: agent_name.to_string(),
                    then_submit,
                };
            }
            return InputAction::UnknownAgent(agent_name.to_string());
        }
        state.input_buffer.clear();
        return InputAction::SetActiveAgent {
            name: agent_name.to_string(),
            then_submit,
        };
    }
    state.input_buffer.clear();

    // Check if this is a slash command
    if let Some(after_slash) = text.strip_prefix('/') {
        let cmd_name = after_slash
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();
        if !cmd_name.is_empty() {
            // Extract optional trailing argument (text after the command name,
            // trimmed; empty string → None).
            let raw_args = after_slash[cmd_name.len()..].trim();
            let args: Option<String> = if raw_args.is_empty() {
                None
            } else {
                Some(raw_args.to_string())
            };

            // Check if it's a built-in command
            if cmd_name == "new" {
                return InputAction::ExecuteCommand {
                    name: cmd_name,
                    args: None,
                };
            }
            // /ml: toggle multi-line mode (AC#3)
            // Covers: Sprint Change Proposal 2026-04-08
            if cmd_name == "ml" {
                return InputAction::ExecuteCommand {
                    name: cmd_name,
                    args: None,
                };
            }
            // /export [filename]: export conversation to markdown.
            // Covers: Story 4-4, AC11/AC12
            if cmd_name == "export" {
                return InputAction::ExecuteCommand {
                    name: cmd_name,
                    args,
                };
            }
            // /mode plan|normal|autoedit|yolo: switch permission mode (AC9)
            if cmd_name == "mode" {
                return InputAction::ExecuteCommand {
                    name: cmd_name,
                    args,
                };
            }
            // /plan on|off|toggle: enter/exit plan mode — Story 6-0d AC5
            if cmd_name == "plan" {
                return InputAction::ExecuteCommand {
                    name: cmd_name,
                    args,
                };
            }
            // /compact: summarize and compact conversation — Story 7.4 AC4
            if cmd_name == "compact" {
                return InputAction::ExecuteCommand {
                    name: cmd_name,
                    args,
                };
            }
            // /deactivate [name]: deactivate active skill(s) — Story 5-2 AC5
            if cmd_name == "deactivate" {
                return InputAction::ExecuteCommand {
                    name: cmd_name,
                    args,
                };
            }
            // Discovered skill name → activate via ExecuteCommand so the event loop
            // routes through `AskActivateSkill` (Story 5-2 AC8). Fall through to
            // user-defined-command SubmitWithContext if the name is NOT a skill.
            if state.skill_name_cache.contains(&cmd_name) {
                return InputAction::ExecuteCommand {
                    name: cmd_name,
                    args,
                };
            }
            // User-defined command: submit with command context
            let command_args = if raw_args.is_empty() {
                None
            } else {
                Some(raw_args.to_string())
            };
            state.resolved_mentions.clear();
            return InputAction::SubmitWithContext {
                text: String::new(),
                command: Some(cmd_name),
                command_args,
            };
        }
    }

    // If there are resolved file mentions, use SubmitWithContext so the event loop
    // can read state.resolved_mentions before clearing them.
    if !state.resolved_mentions.is_empty() {
        return InputAction::SubmitWithContext {
            text,
            command: None,
            command_args: None,
        };
    }

    InputAction::SubmitMessage(text)
}

/// Ensure the cursor's row is visible within the input box scroll window.
fn ensure_cursor_visible(state: &mut TuiState) {
    let (cursor_row, _) = input_box::cursor_to_row_col(&state.input_buffer, state.cursor_position);
    let max_visible = input_box::MAX_INPUT_LINES;
    if cursor_row >= state.input_scroll_offset + max_visible {
        state.input_scroll_offset = cursor_row + 1 - max_visible;
    } else if cursor_row < state.input_scroll_offset {
        state.input_scroll_offset = cursor_row;
    }
}

/// Handle keys while autocomplete popup is active.
// Covers: UX-DR75
fn handle_autocomplete_key(state: &mut TuiState, key: DomainKey) -> InputAction {
    match key {
        // Up/Down navigate the suggestion list
        DomainKey::Up => {
            state.autocomplete.navigate(Direction::Up);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Down => {
            state.autocomplete.navigate(Direction::Down);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        // Tab or Enter selects the current suggestion
        DomainKey::Tab | DomainKey::Enter => {
            let action = if let Some(suggestion) = state.autocomplete.selected().cloned() {
                apply_autocomplete_selection(state, &suggestion)
            } else {
                None
            };
            state.autocomplete.dismiss();
            state.needs_redraw = true;
            action.unwrap_or(InputAction::Consumed)
        }
        // Esc dismisses the popup
        DomainKey::Esc => {
            state.autocomplete.dismiss();
            state.needs_redraw = true;
            InputAction::Consumed
        }
        // Backspace: if cursor goes back to/before trigger position, dismiss
        DomainKey::Backspace => {
            if state.cursor_position == state.autocomplete.trigger_position.saturating_add(1) {
                // Cursor is exactly one past trigger — remove the trigger character and dismiss
                if state.cursor_position > 0 {
                    state.cursor_position -= 1;
                    let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
                    state.input_buffer.remove(byte_pos);
                }
                state.autocomplete.dismiss();
                state.needs_redraw = true;
                InputAction::Consumed
            } else if state.cursor_position <= state.autocomplete.trigger_position {
                // Cursor somehow at or before trigger — just dismiss without editing
                state.autocomplete.dismiss();
                state.needs_redraw = true;
                InputAction::Consumed
            } else {
                // Normal backspace within filter text
                if state.cursor_position > 0 {
                    state.cursor_position -= 1;
                    let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
                    state.input_buffer.remove(byte_pos);
                    // Update filter text
                    let trigger = state.autocomplete.trigger_position;
                    let filter: String = state
                        .input_buffer
                        .chars()
                        .skip(trigger + 1)
                        .take(state.cursor_position.saturating_sub(trigger + 1))
                        .collect();
                    if state.autocomplete.kind == AutocompleteKind::AgentMention
                        && !filter.starts_with("Agents/")
                    {
                        state.autocomplete.kind = AutocompleteKind::FileMention;
                    }
                    let filter_text = if state.autocomplete.kind == AutocompleteKind::AgentMention {
                        filter
                            .strip_prefix("Agents/")
                            .unwrap_or(&filter)
                            .to_string()
                    } else {
                        filter
                    };
                    state.autocomplete.filter_text = filter_text;
                    state.needs_redraw = true;
                }
                InputAction::Consumed
            }
        }
        DomainKey::CtrlC => {
            state.autocomplete.dismiss();
            InputAction::CancelOrQuit
        }
        // All other special keys dismiss autocomplete and fall through to normal handling
        _ => {
            state.autocomplete.dismiss();
            state.needs_redraw = true;
            InputAction::Ignored
        }
    }
}

/// Apply the selected autocomplete suggestion to the input buffer.
fn apply_autocomplete_selection(
    state: &mut TuiState,
    suggestion: &crate::domain::models::autocomplete::AutocompleteSuggestion,
) -> Option<InputAction> {
    use crate::adapters::tui::state::ResolvedMention;
    use crate::domain::models::autocomplete::AutocompleteSuggestion;

    let trigger = state.autocomplete.trigger_position;

    match suggestion {
        AutocompleteSuggestion::SlashCommand { name, .. } => {
            // Replace everything from trigger to cursor with "/<name>"
            let before: String = state.input_buffer.chars().take(trigger).collect();
            let after: String = state
                .input_buffer
                .chars()
                .skip(state.cursor_position)
                .collect();
            state.input_buffer = format!("{}/{}{}", before, name, after);
            state.cursor_position = trigger + 1 + name.chars().count();
            None
        }
        AutocompleteSuggestion::Skill { name, .. } => {
            // Story 5-2 AC8: insert /{name} with trailing space so user can type arguments
            state.input_buffer = format!("/{} ", name);
            state.cursor_position = state.input_buffer.chars().count();
            None
        }
        AutocompleteSuggestion::AgentMention { name, .. } => {
            let before: String = state.input_buffer.chars().take(trigger).collect();
            let after: String = state
                .input_buffer
                .chars()
                .skip(state.cursor_position)
                .collect();
            state.input_buffer = format!("{}@Agents/{}{}", before, name, after);
            state.cursor_position = trigger + 8 + name.chars().count();
            None
        }
        AutocompleteSuggestion::FilePath { path, .. } => {
            // Replace everything from trigger to cursor with "@<path>"
            let before: String = state.input_buffer.chars().take(trigger).collect();
            let after: String = state
                .input_buffer
                .chars()
                .skip(state.cursor_position)
                .collect();
            state.input_buffer = format!("{}@{}{}", before, path, after);
            state.cursor_position = trigger + 1 + path.chars().count();
            // Track resolved mention for file context attachment at send time (deduplicate)
            if !state.resolved_mentions.iter().any(|m| m.path == *path) {
                state
                    .resolved_mentions
                    .push(ResolvedMention { path: path.clone() });
            }
            None
        }
    }
}

/// Handle special keys while command palette is active.
// Covers: UX-DR18
fn handle_command_palette_key(state: &mut TuiState, key: DomainKey) -> InputAction {
    match key {
        DomainKey::Up => {
            state.command_palette.navigate(Direction::Up);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Down => {
            state.command_palette.navigate(Direction::Down);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Enter => {
            // Execute selected entry's action
            let action = state.command_palette.execute_selected();
            let prev = state.command_palette.dismiss();
            if let Some(focus) = prev {
                state.focus = focus;
            }
            state.needs_redraw = true;
            if let Some(palette_action) = action {
                return dispatch_palette_action(state, palette_action);
            }
            InputAction::Consumed
        }
        DomainKey::Esc => {
            state.focus = state.command_palette.dismiss().unwrap_or(FocusState::Input);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Backspace => {
            if !state.command_palette.filter_text.is_empty() {
                state.command_palette.filter_text.pop();
                // Reset scope if prefix character was deleted
                if state.command_palette.filter_text.is_empty() {
                    state.command_palette.current_scope = None;
                }
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }
        DomainKey::Tab => InputAction::Consumed, // No-op
        DomainKey::CtrlC => {
            let prev = state.command_palette.dismiss();
            state.focus = prev.unwrap_or(FocusState::Input);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        _ => InputAction::Consumed,
    }
}

/// Handle character keys while the help overlay is active.
// Covers: FR108, UX-DR94
fn handle_help_overlay_char(state: &mut TuiState, c: char) -> InputAction {
    match c {
        // j / Down → scroll toward bottom (increment offset)
        'j' => {
            state.help_overlay.scroll_offset = state.help_overlay.scroll_offset.saturating_add(1);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        // k / Up → scroll toward top (decrement offset)
        'k' => {
            state.help_overlay.scroll_offset = state.help_overlay.scroll_offset.saturating_sub(1);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        // G → scroll to bottom (large sentinel; render fn clamps to max)
        'G' => {
            state.help_overlay.scroll_offset = usize::MAX / 2;
            state.needs_redraw = true;
            InputAction::Consumed
        }
        // g → scroll to top
        'g' => {
            state.help_overlay.scroll_offset = 0;
            state.needs_redraw = true;
            InputAction::Consumed
        }
        // ? → toggle off (dismiss)
        '?' => {
            state.focus = state.help_overlay.close();
            state.needs_redraw = true;
            InputAction::Consumed
        }
        _ => InputAction::Consumed,
    }
}

/// Handle special keys while the help overlay is active.
// Covers: FR108, UX-DR94
fn handle_help_overlay_key(state: &mut TuiState, key: DomainKey) -> InputAction {
    match key {
        DomainKey::Esc => {
            state.focus = state.help_overlay.close();
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Down => {
            state.help_overlay.scroll_offset = state.help_overlay.scroll_offset.saturating_add(1);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Up => {
            state.help_overlay.scroll_offset = state.help_overlay.scroll_offset.saturating_sub(1);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        // Ctrl+C: pass through to cancel streaming — help overlay is passive, not interactive
        DomainKey::CtrlC => {
            state.focus = state.help_overlay.close();
            state.needs_redraw = true;
            InputAction::CancelOrQuit
        }
        _ => InputAction::Consumed,
    }
}

/// Handle special keys while the model selector overlay is active.
/// Story 7.2 AC1, AC3, AC5.
fn handle_model_selector_key(state: &mut TuiState, key: DomainKey) -> InputAction {
    match key {
        DomainKey::Left => {
            state.model_selector.navigate_provider(Direction::Left);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Right => {
            state.model_selector.navigate_provider(Direction::Right);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Up => {
            if state.model_selector.search_active
                && !state.model_selector.filtered_indices.is_empty()
            {
                if state.model_selector.selected_model == 0 {
                    state.model_selector.selected_model =
                        state.model_selector.filtered_indices.len() - 1;
                } else {
                    state.model_selector.selected_model -= 1;
                }
            } else if !state.model_selector.search_active {
                state.model_selector.navigate_model(Direction::Up);
            }
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Down => {
            if state.model_selector.search_active
                && !state.model_selector.filtered_indices.is_empty()
            {
                state.model_selector.selected_model = (state.model_selector.selected_model + 1)
                    % state.model_selector.filtered_indices.len();
            } else if !state.model_selector.search_active {
                state.model_selector.navigate_model(Direction::Down);
            }
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Enter => {
            if state.model_selector.pending_context_warning.is_some() {
                return InputAction::Consumed;
            }
            if let Some((provider_id, model)) = state.model_selector.selected() {
                return InputAction::SwitchModelProvider {
                    provider_id: Some(provider_id.to_string()),
                    model_id: model.model_id.clone(),
                };
            }
            InputAction::Consumed
        }
        DomainKey::Esc => {
            if state.model_selector.search_active {
                state.model_selector.search_active = false;
                state.model_selector.search_query.clear();
                state.model_selector.filtered_indices.clear();
                state.needs_redraw = true;
                return InputAction::Consumed;
            }
            state.focus = state.model_selector.dismiss().unwrap_or(FocusState::Input);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Backspace => {
            if state.model_selector.search_active {
                state.model_selector.search_query.pop();
                state.model_selector.recompute_filter();
                state.needs_redraw = true;
                return InputAction::Consumed;
            }
            InputAction::Ignored
        }
        DomainKey::CtrlC => {
            state.focus = state.model_selector.dismiss().unwrap_or(FocusState::Input);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        _ => InputAction::Consumed,
    }
}

/// Handle special keys while the usage panel overlay is active (Story 7.5 AC3).
/// Esc/Ctrl+C dismisses; Up/Down cycles the visual `selected_section` highlight.
fn handle_usage_panel_key(state: &mut TuiState, key: DomainKey) -> InputAction {
    match key {
        DomainKey::Esc | DomainKey::CtrlC => {
            state.focus = state.usage_panel.dismiss().unwrap_or(FocusState::Input);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Up => {
            if state.usage_panel.selected_section > 0 {
                state.usage_panel.selected_section -= 1;
            } else {
                state.usage_panel.selected_section = 3;
            }
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Down => {
            state.usage_panel.selected_section = (state.usage_panel.selected_section + 1) % 4;
            state.needs_redraw = true;
            InputAction::Consumed
        }
        _ => InputAction::Consumed,
    }
}

/// Handle char keys while the model selector overlay is active.
/// Story 7.2 AC1 (vim-style h/l/j/k), AC5 (y/n context-warning confirmation),
/// Story 7.6 AC9 (keyword search).
fn handle_model_selector_char(state: &mut TuiState, c: char) -> InputAction {
    if state.model_selector.pending_context_warning.is_some() {
        return match c.to_ascii_lowercase() {
            'y' => {
                // Story 7.4: the 7.2 advisory-warning seam is now wired to real compaction.
                let warning = state.model_selector.pending_context_warning.take();
                if let Some(w) = warning {
                    return InputAction::CompactThenSwitchModel {
                        provider_id: w.provider_id,
                        model_id: w.model_id,
                    };
                }
                InputAction::Consumed
            }
            'n' => {
                state.model_selector.pending_context_warning = None;
                state.needs_redraw = true;
                InputAction::Consumed
            }
            _ => InputAction::Consumed,
        };
    }

    // '/' activates search mode unconditionally (Preflight Consensus #7)
    if c == '/' && !state.model_selector.search_active {
        state.model_selector.search_active = true;
        state.model_selector.search_query.clear();
        state.model_selector.recompute_filter();
        state.needs_redraw = true;
        return InputAction::Consumed;
    }

    if state.model_selector.search_active {
        if c.is_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | ' ' | ':' | '+') {
            state.model_selector.search_query.push(c);
            state.model_selector.recompute_filter();
            state.needs_redraw = true;
            return InputAction::Consumed;
        }
        // Ignore other chars while searching
        return InputAction::Consumed;
    }

    match c.to_ascii_lowercase() {
        'h' => {
            state.model_selector.navigate_provider(Direction::Left);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        'l' => {
            state.model_selector.navigate_provider(Direction::Right);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        'j' => {
            state.model_selector.navigate_model(Direction::Down);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        'k' => {
            state.model_selector.navigate_model(Direction::Up);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        _ => InputAction::Consumed,
    }
}

/// Dispatch a palette action to the appropriate handler.
fn dispatch_palette_action(
    state: &mut TuiState,
    action: crate::domain::models::palette::PaletteAction,
) -> InputAction {
    use crate::domain::models::palette::PaletteAction;

    match action {
        PaletteAction::ExecuteCommand(name, args) => {
            // Story 6.4: route task-control palette commands to dedicated InputAction variants
            if name == "cancel-plan" {
                InputAction::TaskCancelPlan
            } else if name == "resume-all-tasks" {
                InputAction::TaskResumeAll
            } else if name == "reorder-task" {
                let task_n = (state.task_panel_state.selected_index + 1) as u32;
                InputAction::TaskReorderEnter(task_n)
            } else {
                InputAction::ExecuteCommand { name, args }
            }
        }
        PaletteAction::InsertMention(path) => {
            // Insert @path at cursor position, return to input focus
            let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
            let mention = format!("@{}", path);
            state.input_buffer.insert_str(byte_pos, &mention);
            state.cursor_position += mention.chars().count();
            state.focus = FocusState::Input;
            state.needs_redraw = true;
            InputAction::Consumed
        }
        PaletteAction::SwitchModel { provider_id, model_id } => InputAction::SwitchModelProvider {
            provider_id: Some(provider_id),
            model_id,
        },
        PaletteAction::SwitchProfile(_) | PaletteAction::OpenPanel(_) => InputAction::Consumed,
        PaletteAction::ShowVersion => {
            // Display version info as a FeedbackBlock in the chat pane
            // Covers: FR109
            let version = crate::adapters::tui::version_info::version_string();
            let block = crate::domain::models::FeedbackBlock {
                id: "version-info".to_string(),
                level: crate::domain::models::FeedbackLevel::Info,
                message: version,
                actions: Vec::new(),
            };
            state.feedback_blocks.insert(block.id.clone(), block);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        PaletteAction::NewTab => InputAction::NewTab,
        PaletteAction::CloseTab => InputAction::CloseTab,
        PaletteAction::ToggleSidebar => InputAction::ToggleSidebar,
        PaletteAction::DeleteAllConversations => InputAction::DeleteAllConversations,
        PaletteAction::PasteImageFromClipboard => InputAction::RequestClipboardPaste,
        PaletteAction::Noop => {
            // Show "Not yet available" feedback
            let block = crate::domain::models::FeedbackBlock {
                id: "palette-noop".to_string(),
                level: crate::domain::models::FeedbackLevel::Info,
                message: "Not yet available".to_string(),
                actions: Vec::new(),
            };
            state.feedback_blocks.insert(block.id.clone(), block);
            state.needs_redraw = true;
            InputAction::Consumed
        }
    }
}

/// Handle keys while reverse search overlay is active.
// Covers: UX-DR74
fn handle_reverse_search_key(state: &mut TuiState, key: DomainKey) -> InputAction {
    match key {
        DomainKey::Enter => {
            // Select current match and populate input
            if let Some(selected) = state
                .reverse_search
                .matches
                .get(state.reverse_search.selected_match)
            {
                state.input_buffer = selected.1.clone();
                state.cursor_position = state.input_buffer.chars().count();
            }
            state.reverse_search.active = false;
            state.focus = FocusState::Input;
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Esc => {
            state.reverse_search.active = false;
            state.focus = FocusState::Input;
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Up | DomainKey::CtrlR => {
            // Cycle to next match
            if !state.reverse_search.matches.is_empty() {
                state.reverse_search.selected_match =
                    (state.reverse_search.selected_match + 1) % state.reverse_search.matches.len();
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }
        DomainKey::Down => {
            // Cycle to previous match
            if !state.reverse_search.matches.is_empty() {
                if state.reverse_search.selected_match == 0 {
                    state.reverse_search.selected_match = state.reverse_search.matches.len() - 1;
                } else {
                    state.reverse_search.selected_match -= 1;
                }
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }
        DomainKey::Backspace => {
            state.reverse_search.query.pop();
            update_reverse_search_matches(state);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::CtrlC => InputAction::CancelOrQuit,
        _ => InputAction::Consumed,
    }
}

/// Update reverse search matches from current query.
fn update_reverse_search_matches(state: &mut TuiState) {
    let old_selected = state.reverse_search.selected_match;
    let results = state.input_history.search(&state.reverse_search.query);
    state.reverse_search.matches = results
        .into_iter()
        .map(|(i, s)| (i, s.to_string()))
        .collect();
    let new_len = state.reverse_search.matches.len();
    state.reverse_search.selected_match = old_selected.min(new_len.saturating_sub(1));
}

/// Handle special keys while the within-conversation search overlay is active.
///
/// Sub-state aware (Story 4-4 AC3):
/// - `Typing`: Enter commits (if matches > 0), Backspace pops query,
///   Ctrl+U clears, Esc closes.
/// - `Navigating`: Backspace returns to Typing (and pops), Ctrl+U clears and
///   returns to Typing, Enter is a no-op, Esc closes.
///
/// The printable-char path is in `handle_char`'s `FocusState::Overlay(Search)`
/// arm — that's where `n` / `N` navigation and "return to Typing on any
/// printable" live.
// Covers: Story 4-4 AC3, AC4 (UX-DR86)
fn handle_search_overlay_key(state: &mut TuiState, key: DomainKey) -> InputAction {
    use crate::adapters::tui::state::SearchSubstate;
    match (state.search_state.substate, key) {
        // Esc closes the overlay regardless of sub-state (AC4).
        (_, DomainKey::Esc) => InputAction::CloseSearch,

        // Enter in Typing: commit query → Navigating (if matches exist).
        // Enter in Navigating: no-op (already committed).
        (SearchSubstate::Typing, DomainKey::Enter) => InputAction::SearchCommit,
        (SearchSubstate::Navigating, DomainKey::Enter) => InputAction::Consumed,

        // Backspace in Typing: pop query tail, re-scan.
        // Backspace in Navigating: return to Typing and pop (refine).
        (SearchSubstate::Typing, DomainKey::Backspace) => {
            state.search_state.query.pop();
            state.needs_redraw = true;
            InputAction::SearchQueryChanged
        }
        (SearchSubstate::Navigating, DomainKey::Backspace) => {
            state.search_state.substate = SearchSubstate::Typing;
            state.search_state.query.pop();
            state.needs_redraw = true;
            InputAction::SearchReturnToTyping
        }

        // Ctrl+U clears query in either sub-state, returns to Typing.
        (_, DomainKey::CtrlU) => {
            state.search_state.query.clear();
            state.search_state.matches.clear();
            state.search_state.focused_match_index = 0;
            state.search_state.substate = SearchSubstate::Typing;
            state.needs_redraw = true;
            InputAction::SearchClear
        }

        // All other keys consumed as no-ops while the overlay is active.
        _ => InputAction::Consumed,
    }
}

/// Convert a crossterm key event into a domain input event.
/// This is the ONLY place where crossterm types are mapped to domain types.
// Covers: FR16, UX-DR76, UX-DR74
pub fn convert_crossterm_event(
    event: &crossterm::event::Event,
    mouse_cfg: &crate::domain::models::MouseConfig,
) -> Option<DomainInputEvent> {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

    match event {
        // Mouse wheel events. Story 16.8, AC4.
        Event::Mouse(MouseEvent {
            kind, modifiers, ..
        }) => {
            let wheel_lines = mouse_cfg.wheel_lines.max(1);
            match (kind, modifiers.contains(KeyModifiers::SHIFT)) {
                (MouseEventKind::ScrollUp, true) => Some(DomainInputEvent::MouseScroll(
                    crate::domain::models::view_state::ScrollDelta::HalfPageUp,
                )),
                (MouseEventKind::ScrollDown, true) => Some(DomainInputEvent::MouseScroll(
                    crate::domain::models::view_state::ScrollDelta::HalfPageDown,
                )),
                (MouseEventKind::ScrollUp, false) => Some(DomainInputEvent::MouseScroll(
                    crate::domain::models::view_state::ScrollDelta::WheelUp(wheel_lines),
                )),
                (MouseEventKind::ScrollDown, false) => Some(DomainInputEvent::MouseScroll(
                    crate::domain::models::view_state::ScrollDelta::WheelDown(wheel_lines),
                )),
                _ => None, // ScrollLeft/ScrollRight ignored
            }
        }
        Event::Key(KeyEvent {
            code, modifiers, ..
        }) => {
            // Ctrl+C → mapped to DomainKey::CtrlC
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('c') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlC));
            }
            // Ctrl+D → scroll half page down (Chat focus). Story 16.8, AC1.
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('d') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlD));
            }
            // Ctrl+E → toggle multi-line mode
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('e') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlE));
            }
            // Ctrl+F → within-conversation search (Story 4-4 AC1, UX-DR86)
            // NOTE: S16.8 narrow-override — in Chat focus this emits ScrollFullPageDown
            // via handle_special_key; convert_crossterm_event still maps to CtrlF.
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('f') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlF));
            }
            // Ctrl+B → scroll full page up (Chat focus). Story 16.8, AC1.
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('b') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlB));
            }
            // Ctrl+H → toggle history sidebar
            // Covers: FR107, UX-DR20
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('h') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlH));
            }
            // Ctrl+K → feedback-action chord-prefix leader key.
            // UX-DR-GLOBAL-CHORD-PREFIX authorizes Ctrl+K as the global chord prefix.
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('k') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlK));
            }
            // Alt+M → toggle multi-line mode (VS Code terminal alternative to Ctrl+E)
            // Covers: Sprint Change Proposal 2026-04-08, UX-DR76 amendment
            if *modifiers == KeyModifiers::ALT && *code == KeyCode::Char('m') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::AltM));
            }
            // Alt+V → paste image (or text) from the system clipboard.
            // We use Alt+V rather than Ctrl+V because terminal emulators intercept
            // Ctrl+V / Ctrl+Shift+V themselves for their own paste operation.
            if *modifiers == KeyModifiers::ALT && *code == KeyCode::Char('v') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::AltV));
            }
            // Ctrl+P → command palette
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('p') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlP));
            }
            // Ctrl+R → reverse search
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('r') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlR));
            }
            // Ctrl+X → which-key chords
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('x') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlX));
            }
            // Ctrl+T → new tab
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('t') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlT));
            }
            // Ctrl+U → clear search query in Search overlay (Story 4-4, standard readline)
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('u') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlU));
            }

            match code {
                KeyCode::Char(c) => {
                    let c = if modifiers.contains(KeyModifiers::SHIFT)
                        && !modifiers.contains(KeyModifiers::CONTROL)
                        && !modifiers.contains(KeyModifiers::ALT)
                    {
                        c.to_ascii_uppercase()
                    } else {
                        *c
                    };
                    Some(DomainInputEvent::KeyPress(c))
                }
                KeyCode::Enter => {
                    if modifiers.contains(KeyModifiers::SHIFT) {
                        Some(DomainInputEvent::SpecialKey(DomainKey::ShiftEnter))
                    } else if modifiers.contains(KeyModifiers::ALT) {
                        // Alt+Enter: VS Code terminal alternative to Shift+Enter
                        // Covers: Sprint Change Proposal 2026-04-08, UX-DR76 amendment
                        Some(DomainInputEvent::SpecialKey(DomainKey::AltEnter))
                    } else if modifiers.contains(KeyModifiers::CONTROL) {
                        Some(DomainInputEvent::SpecialKey(DomainKey::CtrlEnter))
                    } else {
                        Some(DomainInputEvent::SpecialKey(DomainKey::Enter))
                    }
                }
                KeyCode::Esc => Some(DomainInputEvent::SpecialKey(DomainKey::Esc)),
                KeyCode::Backspace => Some(DomainInputEvent::SpecialKey(DomainKey::Backspace)),
                KeyCode::Delete => Some(DomainInputEvent::SpecialKey(DomainKey::Delete)),
                KeyCode::Up => Some(DomainInputEvent::SpecialKey(DomainKey::Up)),
                KeyCode::Down => Some(DomainInputEvent::SpecialKey(DomainKey::Down)),
                KeyCode::Left => Some(DomainInputEvent::SpecialKey(DomainKey::Left)),
                KeyCode::Right => Some(DomainInputEvent::SpecialKey(DomainKey::Right)),
                KeyCode::Home => Some(DomainInputEvent::SpecialKey(DomainKey::Home)),
                KeyCode::End => Some(DomainInputEvent::SpecialKey(DomainKey::End)),
                KeyCode::Tab => Some(DomainInputEvent::SpecialKey(DomainKey::Tab)),
                KeyCode::BackTab => Some(DomainInputEvent::SpecialKey(DomainKey::ShiftTab)),
                _ => None,
            }
        }
        Event::Paste(data) => {
            // Try to detect base64-encoded image data (common from xclip -o | base64).
            // If it decodes to valid image magic bytes, treat as image paste.
            use base64::Engine;
            let trimmed = data.trim();
            if !trimmed.is_empty() {
                if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(trimmed) {
                    if crate::adapters::tui::image::detect_image_format(&decoded).is_ok() {
                        return Some(DomainInputEvent::ImagePaste(decoded));
                    }
                }
            }
            // Not an image — treat as text paste
            tracing::debug!("Paste data is not a recognized base64 image, treating as text");
            Some(DomainInputEvent::Paste(data.clone()))
        }
        Event::Resize(w, h) => Some(DomainInputEvent::Resize(*w, *h)),
        Event::FocusGained => Some(DomainInputEvent::FocusGained),
        Event::FocusLost => Some(DomainInputEvent::FocusLost),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn alt_key(c: char) -> crossterm::event::Event {
        crossterm::event::Event::Key(KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::ALT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    fn ctrl_key(c: char) -> crossterm::event::Event {
        crossterm::event::Event::Key(KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    // ── convert_crossterm_event ──────────────────────────────────────────────

    fn make_state() -> TuiState {
        TuiState::new(80, 24)
    }

    #[test]
    fn alt_v_maps_to_alt_v_domain_key() {
        let event = alt_key('v');
        assert!(matches!(
            convert_crossterm_event(&event, &crate::domain::models::MouseConfig::default()),
            Some(DomainInputEvent::SpecialKey(DomainKey::AltV))
        ));
    }

    #[test]
    fn alt_m_maps_to_alt_m_domain_key() {
        // Regression: ensure AltM still works after AltV addition
        let event = alt_key('m');
        assert!(matches!(
            convert_crossterm_event(&event, &crate::domain::models::MouseConfig::default()),
            Some(DomainInputEvent::SpecialKey(DomainKey::AltM))
        ));
    }

    #[test]
    fn ctrl_v_does_not_map_to_alt_v() {
        // Ctrl+V must not accidentally trigger clipboard paste (terminals intercept it)
        let event = ctrl_key('v');
        let result =
            convert_crossterm_event(&event, &crate::domain::models::MouseConfig::default());
        assert!(!matches!(
            result,
            Some(DomainInputEvent::SpecialKey(DomainKey::AltV))
        ));
    }

    // ── handle_special_key via handle_input ─────────────────────────────────

    #[test]
    fn alt_v_returns_request_clipboard_paste_in_input_focus() {
        let mut state = make_state();
        state.focus = FocusState::Input;
        let event = DomainInputEvent::SpecialKey(DomainKey::AltV);
        let action = handle_input(&mut state, &event);
        assert_eq!(action, InputAction::RequestClipboardPaste);
    }

    #[test]
    fn alt_v_returns_request_clipboard_paste_in_chat_focus() {
        // Alt+V works regardless of focus — clipboard paste is a global action
        let mut state = make_state();
        state.focus = FocusState::Chat;
        let event = DomainInputEvent::SpecialKey(DomainKey::AltV);
        let action = handle_input(&mut state, &event);
        assert_eq!(action, InputAction::RequestClipboardPaste);
    }

    // ── dispatch_palette_action ──────────────────────────────────────────────

    #[test]
    fn palette_paste_image_dispatches_request_clipboard_paste() {
        let mut state = make_state();
        let action = dispatch_palette_action(
            &mut state,
            crate::domain::models::palette::PaletteAction::PasteImageFromClipboard,
        );
        assert_eq!(action, InputAction::RequestClipboardPaste);
    }

    // ── Story 4-4: Within-Conversation Search dispatch ───────────────────────

    use crate::adapters::tui::state::SearchSubstate;

    fn open_search(state: &mut TuiState) {
        // S16.8 narrow-override: Ctrl+F in Chat focus emits ScrollFullPageDown.
        // To open search, use Input focus (or send Ctrl+F with Chat→open_search helper).
        state.focus = FocusState::Input;
        state.input_buffer.clear();
        let evt = ctrl_key('f');
        if let Some(DomainInputEvent::SpecialKey(key)) =
            convert_crossterm_event(&evt, &crate::domain::models::MouseConfig::default())
        {
            handle_input(state, &DomainInputEvent::SpecialKey(key));
        }
    }

    #[test]
    fn ctrl_f_in_chat_now_emits_scroll_full_page_down() {
        // S16.8 narrow-override: Ctrl+F in Chat focus → ScrollFullPageDown.
        // Search overlay is still accessible via Ctrl+F in Input focus.
        let mut state = make_state();
        state.focus = FocusState::Chat;
        let evt = ctrl_key('f');
        if let Some(DomainInputEvent::SpecialKey(key)) =
            convert_crossterm_event(&evt, &crate::domain::models::MouseConfig::default())
        {
            let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(key));
            assert_eq!(action, InputAction::ScrollFullPageDown);
        } else {
            panic!("Ctrl+F must produce a domain event");
        }
    }

    #[test]
    fn ctrl_f_in_chat_opens_search_overlay_in_typing_substate() {
        let mut state = make_state();
        open_search(&mut state);
        assert_eq!(state.focus, FocusState::Overlay(OverlayType::Search));
        assert!(state.search_state.active);
        assert_eq!(state.search_state.substate, SearchSubstate::Typing);
        assert_eq!(state.search_state.query, "");
    }

    #[test]
    fn ctrl_f_in_input_focus_opens_search() {
        // S16.8: Ctrl+F from Input (empty buffer) still opens search.
        // This test replaces the old ctrl_f_in_chat_... test path.
        let mut state = make_state();
        state.focus = FocusState::Input;
        state.input_buffer.clear();
        let evt = ctrl_key('f');
        if let Some(DomainInputEvent::SpecialKey(key)) =
            convert_crossterm_event(&evt, &crate::domain::models::MouseConfig::default())
        {
            let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(key));
            assert_eq!(action, InputAction::OpenSearch);
        } else {
            panic!("Ctrl+F must produce a domain event");
        }
    }

    #[test]
    fn ctrl_f_from_input_with_empty_buffer_opens_search() {
        // AC1 (party-mode Fix 16): Ctrl+F opens the search overlay from
        // either `Chat` OR `Input` focus when the input buffer is empty.
        let mut state = make_state();
        state.focus = FocusState::Input;
        state.input_buffer.clear();
        let evt = ctrl_key('f');
        if let Some(DomainInputEvent::SpecialKey(key)) =
            convert_crossterm_event(&evt, &crate::domain::models::MouseConfig::default())
        {
            handle_input(&mut state, &DomainInputEvent::SpecialKey(key));
        }
        assert_eq!(state.focus, FocusState::Overlay(OverlayType::Search));
        assert!(state.search_state.active);
    }

    #[test]
    fn ctrl_f_from_input_with_pending_text_does_not_open_search() {
        // AC1 (party-mode Fix 16): Ctrl+F is consumed as Ignored when the
        // user has pending text in the input buffer — they may be composing
        // a message that will include the letter `f`.
        let mut state = make_state();
        state.focus = FocusState::Input;
        state.input_buffer = "hello".to_string();
        let evt = ctrl_key('f');
        if let Some(DomainInputEvent::SpecialKey(key)) =
            convert_crossterm_event(&evt, &crate::domain::models::MouseConfig::default())
        {
            handle_input(&mut state, &DomainInputEvent::SpecialKey(key));
        }
        assert_ne!(state.focus, FocusState::Overlay(OverlayType::Search));
        assert!(!state.search_state.active);
    }

    #[test]
    fn typing_n_in_query_does_not_navigate_critical_collision_fix() {
        // The critical Story 4-4 spec bug: typing `n` while building a query
        // must NOT trigger navigation. It must append to the query string
        // because we are in the Typing sub-state.
        let mut state = make_state();
        open_search(&mut state);
        // Type "nginx" — every char (including 'n') must land in the query.
        for c in "nginx".chars() {
            handle_input(&mut state, &DomainInputEvent::KeyPress(c));
        }
        assert_eq!(state.search_state.query, "nginx");
        assert_eq!(state.search_state.substate, SearchSubstate::Typing);
    }

    #[test]
    fn enter_on_zero_matches_stays_in_typing() {
        let mut state = make_state();
        open_search(&mut state);
        handle_input(&mut state, &DomainInputEvent::KeyPress('q'));
        // matches is still empty (find_matches runs in event_loop, not handle_input)
        assert!(state.search_state.matches.is_empty());
        let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
        assert_eq!(action, InputAction::SearchCommit);
        // Substate unchanged — event loop decides whether to transition based on matches.
        assert_eq!(state.search_state.substate, SearchSubstate::Typing);
    }

    #[test]
    fn n_in_navigating_substate_returns_search_next() {
        let mut state = make_state();
        open_search(&mut state);
        state.search_state.substate = SearchSubstate::Navigating;
        let action = handle_input(&mut state, &DomainInputEvent::KeyPress('n'));
        assert_eq!(action, InputAction::SearchNext);
    }

    #[test]
    fn shift_n_in_navigating_substate_returns_search_prev() {
        let mut state = make_state();
        open_search(&mut state);
        state.search_state.substate = SearchSubstate::Navigating;
        let action = handle_input(&mut state, &DomainInputEvent::KeyPress('N'));
        assert_eq!(action, InputAction::SearchPrev);
    }

    #[test]
    fn printable_char_in_navigating_returns_to_typing_and_appends() {
        let mut state = make_state();
        open_search(&mut state);
        state.search_state.query = "foo".to_string();
        state.search_state.substate = SearchSubstate::Navigating;
        // Typing 'x' while in Navigating should return to Typing AND append 'x'
        // so the user can refine without pressing Esc.
        let action = handle_input(&mut state, &DomainInputEvent::KeyPress('x'));
        assert_eq!(action, InputAction::SearchReturnToTyping);
        assert_eq!(state.search_state.query, "foox");
        assert_eq!(state.search_state.substate, SearchSubstate::Typing);
    }

    #[test]
    fn ctrl_u_clears_query_and_stays_in_typing() {
        let mut state = make_state();
        open_search(&mut state);
        state.search_state.query = "something".to_string();
        state.search_state.substate = SearchSubstate::Navigating;
        let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlU));
        assert_eq!(action, InputAction::SearchClear);
        assert_eq!(state.search_state.query, "");
        assert_eq!(state.search_state.substate, SearchSubstate::Typing);
    }

    #[test]
    fn backspace_in_typing_pops_query_and_requests_rescan() {
        let mut state = make_state();
        open_search(&mut state);
        state.search_state.query = "hello".to_string();
        let action = handle_input(
            &mut state,
            &DomainInputEvent::SpecialKey(DomainKey::Backspace),
        );
        assert_eq!(action, InputAction::SearchQueryChanged);
        assert_eq!(state.search_state.query, "hell");
    }

    #[test]
    fn backspace_in_navigating_returns_to_typing_and_pops() {
        let mut state = make_state();
        open_search(&mut state);
        state.search_state.query = "hello".to_string();
        state.search_state.substate = SearchSubstate::Navigating;
        let action = handle_input(
            &mut state,
            &DomainInputEvent::SpecialKey(DomainKey::Backspace),
        );
        assert_eq!(action, InputAction::SearchReturnToTyping);
        assert_eq!(state.search_state.query, "hell");
        assert_eq!(state.search_state.substate, SearchSubstate::Typing);
    }

    #[test]
    fn esc_in_search_overlay_returns_close_search() {
        let mut state = make_state();
        open_search(&mut state);
        let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
        assert_eq!(action, InputAction::CloseSearch);
    }

    #[test]
    fn enter_in_typing_with_empty_query_still_returns_search_commit() {
        // Even on empty query, handle_input returns SearchCommit — the event
        // loop decides whether to actually transition substate based on
        // matches. This separation keeps handle_input pure (no conversation).
        let mut state = make_state();
        open_search(&mut state);
        let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
        assert_eq!(action, InputAction::SearchCommit);
    }

    // ── Story 4-4: Bookmark dispatch ─────────────────────────────────────────

    #[test]
    fn m_in_chat_focus_returns_toggle_bookmark() {
        let mut state = make_state();
        state.focus = FocusState::Chat;
        let action = handle_input(&mut state, &DomainInputEvent::KeyPress('m'));
        assert_eq!(action, InputAction::ToggleBookmark);
    }

    #[test]
    fn quote_in_chat_focus_returns_open_bookmark_list() {
        let mut state = make_state();
        state.focus = FocusState::Chat;
        let action = handle_input(&mut state, &DomainInputEvent::KeyPress('\''));
        assert_eq!(action, InputAction::OpenBookmarkList);
    }

    #[test]
    fn m_outside_chat_focus_does_not_toggle_bookmark() {
        // Input focus appends 'm' to buffer; does not emit ToggleBookmark.
        let mut state = make_state();
        state.focus = FocusState::Input;
        let action = handle_input(&mut state, &DomainInputEvent::KeyPress('m'));
        assert_ne!(action, InputAction::ToggleBookmark);
    }

    fn open_bookmark_list(state: &mut TuiState) {
        state.focus = FocusState::Overlay(OverlayType::BookmarkList);
        state.bookmark_list_selected = 0;
        // Mirror count for the clamp path — tests assume at least 5 entries
        // so j/Down advance freely within [0, 4].
        state.bookmark_list_count = 5;
    }

    #[test]
    fn d_in_bookmark_list_returns_delete() {
        let mut state = make_state();
        open_bookmark_list(&mut state);
        let action = handle_input(&mut state, &DomainInputEvent::KeyPress('d'));
        assert_eq!(action, InputAction::DeleteBookmark);
    }

    #[test]
    fn delete_key_in_bookmark_list_returns_delete() {
        let mut state = make_state();
        open_bookmark_list(&mut state);
        let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Delete));
        assert_eq!(action, InputAction::DeleteBookmark);
    }

    #[test]
    fn backspace_in_bookmark_list_returns_delete() {
        let mut state = make_state();
        open_bookmark_list(&mut state);
        let action = handle_input(
            &mut state,
            &DomainInputEvent::SpecialKey(DomainKey::Backspace),
        );
        assert_eq!(action, InputAction::DeleteBookmark);
    }

    #[test]
    fn u_in_bookmark_list_returns_undo() {
        let mut state = make_state();
        open_bookmark_list(&mut state);
        let action = handle_input(&mut state, &DomainInputEvent::KeyPress('u'));
        assert_eq!(action, InputAction::UndoBookmarkDelete);
    }

    #[test]
    fn enter_in_bookmark_list_returns_jump() {
        let mut state = make_state();
        open_bookmark_list(&mut state);
        let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
        assert_eq!(action, InputAction::JumpToBookmark);
    }

    #[test]
    fn esc_in_bookmark_list_returns_close() {
        let mut state = make_state();
        open_bookmark_list(&mut state);
        let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
        assert_eq!(action, InputAction::CloseBookmarkList);
    }

    #[test]
    fn j_in_bookmark_list_advances_selection() {
        let mut state = make_state();
        open_bookmark_list(&mut state);
        state.bookmark_list_selected = 0;
        let _ = handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
        assert_eq!(state.bookmark_list_selected, 1);
        let _ = handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
        assert_eq!(state.bookmark_list_selected, 2);
    }

    #[test]
    fn k_in_bookmark_list_reverses_selection() {
        let mut state = make_state();
        open_bookmark_list(&mut state);
        state.bookmark_list_selected = 3;
        let _ = handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
        assert_eq!(state.bookmark_list_selected, 2);
        // Saturating at 0.
        state.bookmark_list_selected = 0;
        let _ = handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
        assert_eq!(state.bookmark_list_selected, 0);
    }

    #[test]
    fn arrow_keys_in_bookmark_list_also_navigate() {
        let mut state = make_state();
        open_bookmark_list(&mut state);
        state.bookmark_list_selected = 1;
        let _ = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Down));
        assert_eq!(state.bookmark_list_selected, 2);
        let _ = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Up));
        assert_eq!(state.bookmark_list_selected, 1);
    }

    // ── Story 5.4: Agent dispatch tests ─────────────────────────────────────

    #[test]
    fn test_submit_at_agents_name_emits_set_active_agent() {
        let mut state = make_state();
        state.agent_registry = crate::adapters::agent_registry::AgentRegistry::from_agents(vec![
            crate::domain::models::AgentDef {
                name: "code-reviewer".to_string(),
                description: "Reviews code".to_string(),
                file: std::path::PathBuf::from("/tmp/.claude/agents/code-reviewer.md"),
                allowed_tools: None,
                exclude_tools: None,
                model: None,
            },
        ]);
        state.input_buffer = "@Agents/code-reviewer".to_string();
        state.cursor_position = state.input_buffer.chars().count();
        let action = submit_message(&mut state);
        assert!(
            matches!(&action, InputAction::SetActiveAgent { name, then_submit: None } if name == "code-reviewer"),
            "Expected SetActiveAgent for code-reviewer, got {:?}",
            action
        );
    }

    #[test]
    fn test_submit_at_agents_default_emits_clear_active_agent() {
        let mut state = make_state();
        state.input_buffer = "@Agents/default".to_string();
        state.cursor_position = state.input_buffer.chars().count();
        let action = submit_message(&mut state);
        assert!(
            matches!(action, InputAction::ClearActiveAgent { then_submit: None }),
            "Expected ClearActiveAgent, got {:?}",
            action
        );
    }

    #[test]
    fn test_submit_at_agents_with_trailing_text_emits_both() {
        let mut state = make_state();
        state.agent_registry = crate::adapters::agent_registry::AgentRegistry::from_agents(vec![
            crate::domain::models::AgentDef {
                name: "code-reviewer".to_string(),
                description: "Reviews code".to_string(),
                file: std::path::PathBuf::from("/tmp/.claude/agents/code-reviewer.md"),
                allowed_tools: None,
                exclude_tools: None,
                model: None,
            },
        ]);
        state.input_buffer = "@Agents/code-reviewer review src/auth.rs".to_string();
        state.cursor_position = state.input_buffer.chars().count();
        let action = submit_message(&mut state);
        assert!(
            matches!(&action, InputAction::SetActiveAgent { name, then_submit: Some(text) } if name == "code-reviewer" && text == "review src/auth.rs"),
            "Expected SetActiveAgent with trailing text, got {:?}",
            action
        );
    }

    #[test]
    fn test_submit_at_agents_unknown_name_emits_unknown_agent() {
        use crate::adapters::agent_registry::AgentRegistry;
        let mut state = make_state();
        state.agent_registry = AgentRegistry::from_agents(vec![]);
        state.input_buffer = "@Agents/no-such-agent".to_string();
        state.cursor_position = state.input_buffer.chars().count();
        let action = submit_message(&mut state);
        assert!(
            matches!(action, InputAction::UnknownAgent(ref name) if name == "no-such-agent"),
            "Expected UnknownAgent, got {:?}",
            action
        );
        // Buffer should be preserved for user correction
        assert_eq!(state.input_buffer, "@Agents/no-such-agent");
    }

    #[test]
    fn test_submit_at_agents_undiscovered_registry_queues_pending() {
        use crate::adapters::agent_registry::AgentRegistry;
        let mut state = make_state();
        // Default registry from make_state is from_agents(vec![]) which is discovered=true.
        // Replace with undiscovered registry.
        state.agent_registry = AgentRegistry::new();
        state.input_buffer = "@Agents/foo".to_string();
        state.cursor_position = state.input_buffer.chars().count();
        let action = submit_message(&mut state);
        assert!(
            matches!(
                &action,
                InputAction::AgentDiscoveryPending { name, then_submit: None } if name == "foo"
            ),
            "Expected AgentDiscoveryPending, got {:?}",
            action
        );
        assert_eq!(
            state.pending_agent_activation,
            Some(("foo".to_string(), None))
        );
    }

    #[test]
    fn test_at_agents_mid_buffer_is_literal_text() {
        let mut state = make_state();
        state.input_buffer = "say @Agents/foo".to_string();
        state.cursor_position = state.input_buffer.chars().count();
        let action = submit_message(&mut state);
        assert!(
            matches!(action, InputAction::SubmitMessage { .. }),
            "Expected normal SubmitMessage when @Agents/ is mid-buffer, got {:?}",
            action
        );
    }

    #[test]
    fn test_typing_at_agents_slash_switches_autocomplete_kind() {
        let mut state = make_state();
        // Type '@'
        let _ = handle_input(&mut state, &DomainInputEvent::KeyPress('@'));
        assert_eq!(state.autocomplete.kind, AutocompleteKind::FileMention);
        // Type 'Agents/'
        for c in "Agents/".chars() {
            let _ = handle_input(&mut state, &DomainInputEvent::KeyPress(c));
        }
        assert_eq!(state.autocomplete.kind, AutocompleteKind::AgentMention);
        assert_eq!(state.autocomplete.filter_text, "");
    }

    #[test]
    fn test_backspace_past_slash_returns_to_file_mention() {
        let mut state = make_state();
        // Type '@Agents/'
        let _ = handle_input(&mut state, &DomainInputEvent::KeyPress('@'));
        for c in "Agents/".chars() {
            let _ = handle_input(&mut state, &DomainInputEvent::KeyPress(c));
        }
        assert_eq!(state.autocomplete.kind, AutocompleteKind::AgentMention);
        // Backspace once to delete '/'
        let _ = handle_input(
            &mut state,
            &DomainInputEvent::SpecialKey(DomainKey::Backspace),
        );
        assert_eq!(state.autocomplete.kind, AutocompleteKind::FileMention);
    }

    // ── Story 16.5.5: Feedback Action Dispatch Arbiter ────────────────────────

    #[test]
    fn bare_c_in_input_focus_with_feedback_inserts_text_not_compact() {
        let mut state = make_state();
        state.focus = FocusState::Input;
        state.active_feedback_id = Some("fb-1".to_string());
        let action = handle_char(&mut state, 'c');
        assert_eq!(action, InputAction::Consumed);
        assert!(state.input_buffer.contains('c'));
    }

    #[test]
    fn chord_ctrl_k_c_with_feedback_dispatches_compact() {
        let mut state = make_state();
        state.focus = FocusState::Input;
        state.active_feedback_id = Some("fb-1".to_string());
        // Press Ctrl+K to set chord leader
        let ctrl_k = DomainInputEvent::SpecialKey(DomainKey::CtrlK);
        let action1 = handle_input(&mut state, &ctrl_k);
        assert_eq!(action1, InputAction::Consumed);
        assert!(state.chord_leader_active);
        // Press 'c' — chord dispatch fires through
        let action2 = handle_char(&mut state, 'c');
        assert_eq!(action2, InputAction::FeedbackCompact);
        assert!(!state.chord_leader_active);
        assert!(
            !state.input_buffer.contains('c'),
            "chord 'c' must not leak into input buffer"
        );
    }

    #[test]
    fn chord_dispatch_pierces_focus() {
        for focus in [
            FocusState::Input,
            FocusState::Chat,
            FocusState::Sidebar {
                panel: crate::domain::models::PanelType::History,
                selected: 0,
            },
        ] {
            let mut state = make_state();
            state.focus = focus.clone();
            state.active_feedback_id = Some("fb-1".to_string());
            let ctrl_k = DomainInputEvent::SpecialKey(DomainKey::CtrlK);
            handle_input(&mut state, &ctrl_k);
            let action = handle_char(&mut state, 'c');
            assert_eq!(
                action,
                InputAction::FeedbackCompact,
                "chord dispatch should fire in {:?} focus",
                focus
            );
        }
    }

    #[test]
    fn chat_focus_bare_c_without_feedback_still_copies() {
        let mut state = make_state();
        state.focus = FocusState::Chat;
        // No active feedback — bare 'c' should still copy to clipboard
        assert!(state.active_feedback_id.is_none());
        let action = handle_char(&mut state, 'c');
        assert_eq!(action, InputAction::CopyToClipboard(String::new()));
    }

    #[test]
    fn ctrl_k_maps_to_ctrl_k_domain_key() {
        let event = ctrl_key('k');
        assert!(matches!(
            convert_crossterm_event(&event, &crate::domain::models::MouseConfig::default()),
            Some(DomainInputEvent::SpecialKey(DomainKey::CtrlK))
        ));
    }

    #[test]
    fn chord_leader_then_invalid_key_is_consumed() {
        let mut state = make_state();
        state.focus = FocusState::Input;
        state.active_feedback_id = Some("fb-1".to_string());
        let ctrl_k = DomainInputEvent::SpecialKey(DomainKey::CtrlK);
        handle_input(&mut state, &ctrl_k);
        // 'z' is not a valid feedback key
        let action = handle_char(&mut state, 'z');
        assert_eq!(action, InputAction::Consumed);
        assert!(!state.chord_leader_active);
    }

    // ── Story 16.6: Vim keymap unit tests (Task 11) ──

    #[test]
    fn z_prefix_chord_dispatches_z_actions() {
        let mut state = make_state();
        state.focus = FocusState::Chat;
        // Press 'z' leader
        let action = handle_char(&mut state, 'z');
        assert_eq!(action, InputAction::Consumed);
        assert!(state.pending_z);

        // za → FoldToggleAtFocus
        assert_eq!(handle_char(&mut state, 'a'), InputAction::FoldToggleAtFocus);
        assert!(!state.pending_z);

        // Reset and test other chords
        state.focus = FocusState::Chat;
        state.pending_z = false;
        assert_eq!(handle_char(&mut state, 'z'), InputAction::Consumed);
        assert_eq!(handle_char(&mut state, 'c'), InputAction::CollapseFocus);
        assert!(!state.pending_z);

        state.focus = FocusState::Chat;
        state.pending_z = false;
        assert_eq!(handle_char(&mut state, 'z'), InputAction::Consumed);
        assert_eq!(handle_char(&mut state, 'o'), InputAction::ExpandFocus);
        assert!(!state.pending_z);

        state.focus = FocusState::Chat;
        state.pending_z = false;
        assert_eq!(handle_char(&mut state, 'z'), InputAction::Consumed);
        assert_eq!(handle_char(&mut state, 'M'), InputAction::CollapseAllTurns);
        assert!(!state.pending_z);

        state.focus = FocusState::Chat;
        state.pending_z = false;
        assert_eq!(handle_char(&mut state, 'z'), InputAction::Consumed);
        assert_eq!(handle_char(&mut state, 'R'), InputAction::ExpandAllTurns);
        assert!(!state.pending_z);

        state.focus = FocusState::Chat;
        state.pending_z = false;
        assert_eq!(handle_char(&mut state, 'z'), InputAction::Consumed);
        assert_eq!(handle_char(&mut state, 's'), InputAction::ToggleSummaryTier);
        assert!(!state.pending_z);

        state.focus = FocusState::Chat;
        state.pending_z = false;
        assert_eq!(handle_char(&mut state, 'z'), InputAction::Consumed);
        assert_eq!(handle_char(&mut state, 'z'), InputAction::RecenterAnchor);
        assert!(!state.pending_z);
    }

    #[test]
    fn z_prefix_resets_on_invalid_followup() {
        let mut state = make_state();
        state.focus = FocusState::Chat;
        handle_char(&mut state, 'z');
        assert!(state.pending_z);
        // z followed by 'j' is consumed and cancels chord
        let action = handle_char(&mut state, 'j');
        assert_eq!(action, InputAction::Consumed);
        assert!(!state.pending_z);
    }

    #[test]
    fn z_prefix_resets_on_focus_change_via_special_key() {
        let mut state = make_state();
        state.focus = FocusState::Chat;
        handle_char(&mut state, 'z');
        assert!(state.pending_z);
        // ESC should reset
        let esc = DomainInputEvent::SpecialKey(DomainKey::Esc);
        handle_input(&mut state, &esc);
        assert!(!state.pending_z);
        assert!(state.pending_bracket.is_none());
    }

    #[test]
    fn bracket_prefix_dispatches_jump_actions() {
        let mut state = make_state();
        state.focus = FocusState::Chat;
        // ]]
        assert_eq!(handle_char(&mut state, ']'), InputAction::Consumed);
        assert_eq!(state.pending_bracket, Some(']'));
        assert_eq!(
            handle_char(&mut state, ']'),
            InputAction::JumpProseAnchor(Direction::Down)
        );
        assert!(state.pending_bracket.is_none());

        // [[
        assert_eq!(handle_char(&mut state, '['), InputAction::Consumed);
        assert_eq!(state.pending_bracket, Some('['));
        assert_eq!(
            handle_char(&mut state, '['),
            InputAction::JumpProseAnchor(Direction::Up)
        );
        assert!(state.pending_bracket.is_none());
    }

    #[test]
    fn bracket_prefix_resets_on_invalid_followup() {
        let mut state = make_state();
        state.focus = FocusState::Chat;
        handle_char(&mut state, ']');
        assert_eq!(state.pending_bracket, Some(']'));
        // ] followed by 'a' cancels
        let action = handle_char(&mut state, 'a');
        assert_eq!(action, InputAction::Consumed);
        assert!(state.pending_bracket.is_none());
    }

    #[test]
    fn tab_in_chat_focus_emits_cycle_invocation() {
        let mut state = make_state();
        state.focus = FocusState::Chat;
        let tab = DomainInputEvent::SpecialKey(DomainKey::Tab);
        let action = handle_input(&mut state, &tab);
        assert_eq!(action, InputAction::CycleInvocationInFocusedTurn);
    }

    #[test]
    fn g_capital_in_chat_focus_emits_scroll_to_bottom() {
        // S16.8: G handler now emits ScrollToBottom InputAction instead of
        // direct mutation. The event-loop dispatcher handles the actual scroll
        // (via dispatch_view_scroll) and mode-aware Pinned no-op (AC3).
        let mut state = make_state();
        state.focus = FocusState::Chat;
        let action = handle_char(&mut state, 'G');
        assert_eq!(action, InputAction::ScrollToBottom);
        assert!(state.needs_redraw, "G must request redraw");
    }

    #[test]
    fn rbracket_capital_p_chord_emits_jump_to_latest_prose_anchor() {
        // S16.8 preflight rebinding (2026-05-03): the S16.6 G→JumpToLatestProseAnchor
        // binding moved to the ]P chord (bracket-prefix family). Bracket leader has
        // no first-key side effect, so ]P produces no flicker (unlike a hypothetical
        // gp chord where single-g would fire ScrollToTop first). See ADR-16-03.
        let mut state = make_state();
        state.focus = FocusState::Chat;
        // First press: ] — sets pending_bracket = Some(']'), no scroll, no flicker
        let first = handle_char(&mut state, ']');
        assert_eq!(first, InputAction::Consumed);
        assert_eq!(state.pending_bracket, Some(']'));
        // Second press: P (capital) — dispatches JumpToLatestProseAnchor + clears pending_bracket
        let second = handle_char(&mut state, 'P');
        assert_eq!(second, InputAction::JumpToLatestProseAnchor);
        assert!(
            state.pending_bracket.is_none(),
            "]P chord must clear pending_bracket"
        );
    }

    #[test]
    fn rbracket_lowercase_p_does_not_dispatch_jump_to_latest_prose_anchor() {
        // ]p (lowercase) is NOT the prose-anchor binding — only ]P (capital) dispatches.
        // Lowercase keystroke falls through the chord handler's catch-all → Consumed,
        // pending_bracket cleared. Prevents accidental dispatch on shift-key fumble.
        let mut state = make_state();
        state.focus = FocusState::Chat;
        handle_char(&mut state, ']');
        assert_eq!(state.pending_bracket, Some(']'));
        let action = handle_char(&mut state, 'p');
        assert_eq!(action, InputAction::Consumed);
        assert!(
            state.pending_bracket.is_none(),
            "invalid bracket chord must clear pending_bracket"
        );
    }

    #[test]
    fn vim_keys_outside_chat_focus_are_inert() {
        let mut state = make_state();
        state.focus = FocusState::Input;
        // 'z' in Input focus falls through to char input and is consumed as normal text
        let _action = handle_char(&mut state, 'z');
        // But pending_z must NOT be set (vim chords only activate in Chat focus)
        assert!(!state.pending_z);
    }
}
