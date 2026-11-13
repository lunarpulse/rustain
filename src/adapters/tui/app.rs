use crate::adapters::tui::handlers::{login::LoginAction, profile_switch};
use crate::adapters::tui::keymap::table::GlobalCmd;
use crate::adapters::tui::keymap::table_chat::ChatCmd;
use crate::adapters::tui::keymap::table_input::{FieldCmd, InputCmd};
use crate::adapters::tui::keymap::table_nav::{BookmarkCmd, SearchCmd};
use crate::adapters::tui::keymap::table_overlay::OverlayCmd;
use crate::adapters::tui::keymap::table_panels::PanelCmd;
use crate::adapters::tui::keymap::text_edit;
use crate::adapters::tui::keymap::{CommandId, Context, Host, Key, Resolved, resolve};
use crate::adapters::tui::presentation::text_field::TextField;
use crate::adapters::tui::presentation::{self, Surface, reducer};
use crate::adapters::tui::state::{Direction, TuiState};
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::widgets::chat_pane::virtual_scroll::find_next_boundary;
use crate::adapters::tui::widgets::input_box;
use crate::domain::events::{DomainInputEvent, DomainKey};
use crate::domain::models::FocusState;
use crate::domain::models::autocomplete::AutocompleteKind;
use crate::domain::models::palette::PaletteAction;
use crate::domain::services::slash_command::parse_slash_command;

/// Action returned by handle_input to tell the event loop what to do.
/// app.rs is a pure input→action mapper; the event loop owns all side effects.
#[derive(Debug, PartialEq, Eq)]
pub enum InputAction {
    /// Event handled, no further action needed.
    Consumed,
    /// Inspect the render-focused tool; the runtime resolves the live conversation.
    InspectTool,
    /// Resolve a same-turn step against canonical sources before the next paint.
    StepTool {
        forward: bool,
    },
    /// Move the open block viewer to a same-turn neighbour, resolved against the
    /// live canonical parts before the next paint.
    StepBlock {
        forward: bool,
    },
    /// Copy the current tool tab from fresh owning canonical sources, not cells.
    CopyToolSource,
    /// Copy the open block's exact source from the owning canonical parts.
    CopyBlockSource,
    /// Event not handled by this focus mode.
    Ignored,
    ThreadPage {
        earlier: bool,
    },
    /// `x` on a sent thread (local host): the `/team retract <peer> <item>` preview.
    ThreadRetract {
        peer: String,
        item: String,
    },
    /// Enter pressed with this text (buffer already cleared by handle_input).
    SubmitMessage(String),
    /// User wants to exit.
    Quit,
    /// The S-QUIT decision frame was explicitly confirmed with `y`: the host
    /// performs its existing exit exactly once (Story 21.2 AC7).
    QuitConfirmed,
    /// A notification-log effect, applied by the host with its live conversation facts.
    NotificationLog(crate::adapters::tui::notification_log::LogOutcome),
    /// Ctrl+C: cancel streaming if active, otherwise quit.
    CancelOrQuit,
    /// Permission frame: user pressed y.
    PermissionAllow,
    /// Permission frame: user pressed n.
    PermissionDeny,
    /// Permission frame: user pressed a.
    PermissionAlwaysAllow,
    /// Permission frame: user pressed s (session allow).
    PermissionSessionAllow,
    /// Permission frame: Enter in the Deny + feedback field (the text rides in the
    /// resolved payload).
    FeedbackInputSubmit,
    /// Story 8.5 AC-6 — apply a session-scoped adapter override.
    ApplyAdapterOverride {
        port: crate::domain::models::PortDimension,
        adapter: String,
    },
    /// Story 8.5 AC-7 — clear a session-scoped adapter override.
    ClearAdapterOverride {
        port: crate::domain::models::PortDimension,
    },
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
    /// Story 10.5: Delegation card — user pressed d (delegate to agent).
    DelegateTask,
    /// Story 10.5: Delegation card — user pressed l (run task locally).
    RunTaskLocally,
    /// Story 10.5: Delegation card — user pressed Esc (cancel plan at this task).
    DelegationCardCancel,
    /// Story 11.2a: Memory-consolidation card — user pressed y (promote all).
    ConsolidateAcceptAll,
    /// Story 11.2a: Memory-consolidation card — user pressed n/Esc (decline all).
    ConsolidateDeclineAll,
    /// Story 11.4a: `/memory forget` card — user pressed y (forget selected matches).
    ForgetAcceptAll,
    /// Story 11.4a: `/memory forget` card — user pressed n/Esc (cancel).
    ForgetDeclineAll,
    /// Story 18.3a-e: confirm the pending patch apply.
    ApplyCardAccept,
    /// Story 18.3a-e: decline the pending patch apply.
    ApplyCardDecline,
    /// Story 18.4b (AC3): confirm the pending `/peer add` and pin the key.
    ///
    /// ⛔ Deliberately separate from [`Self::ApplyCardAccept`]: sharing the apply
    /// card's mode-blind key table would give a card that paints no `y` an
    /// unpainted `y` that rebinds a pin.
    PeerAddConfirm,
    /// Story 18.4b (AC3): cancel the pending `/peer add`. Writes nothing.
    PeerAddDecline,
    /// Story 19.16f (AC4): accept the ARMED `/team retract` card — the only
    /// path to the cross-host write. Never produced by a disarmed card.
    TeamRetractConfirm,
    /// Story 19.16f (AC4): cancel the `/team retract` card. Sends nothing.
    TeamRetractDecline,
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
    /// Open a passive proposal after switching to its originating conversation.
    OpenDecisionTab {
        conversation_id: String,
        key: crate::adapters::tui::decision_queue::DecisionKey,
    },
    /// `Ctrl+X h` / the palette: open the History window on the session index.
    OpenHistory,
    /// `Enter` in the History window: open the conversation with this id (the host loads it
    /// into a tab, or switches to its tab, and closes the window).
    OpenHistoryEntry(String),
    /// `d` in the History window: ask to delete the conversation with this id (a decision;
    /// never a direct delete). The id is the one that was selected, not a row index.
    DeleteHistoryEntry(String),
    /// Mission Control was opened on this tab by a chord (`Ctrl+X r`): the frame is already
    /// on the stack; the event loop runs the tab's source read (the durable room fold).
    OpenMissionControl(crate::adapters::tui::mission_control::McTab),
    /// `Ctrl+X l` (local): read the A2A log and open it as Mission Control's Messages tab
    /// (the same read `/team log` runs).
    OpenMessages,
    /// `Enter` on an Artifacts row: open the artifact's window (the same read
    /// `/artifact show <id>` runs). Carries the full artifact id.
    OpenArtifactWindow(String),
    /// Export the exact transparency report snapshot the Messages tab was read from.
    ExportTransparency,
    /// `Enter` on a Tasks row: open the task's window (Story 21.7 A8). The host reads the task
    /// from the **active** conversation and refuses a ref of another one.
    OpenTaskWindow(crate::adapters::tui::mission_control::tasks::TaskRef),
    /// Story 6.4 / 21.7 A11: Pause/Resume a task (Tasks tab `p`, task window `p`). The ref names
    /// the conversation, plan and task; the host resolves it in **that** conversation and
    /// refuses it when it is not the active writable tab's.
    TaskPause(crate::adapters::tui::mission_control::tasks::TaskRef),
    /// Skip a task (Tasks tab `s`, task window `s`); a decision confirms a cascade.
    TaskSkip(crate::adapters::tui::mission_control::tasks::TaskRef),
    /// Retry a failed task (task window `r`).
    TaskRetry(crate::adapters::tui::mission_control::tasks::TaskRef),
    /// Edit a failed task (task window `e`).
    TaskEdit(crate::adapters::tui::mission_control::tasks::TaskRef),
    /// Cancel the plan the ref names (Tasks tab `x`): raises the confirmation decision.
    TaskCancelPlan(crate::adapters::tui::mission_control::tasks::TaskRef),
    /// Palette `!cancel-plan`: cancel the active conversation's executing plan (a decision).
    TaskCancelActivePlan,
    /// Story 6.4: Resume all paused tasks (palette `!resume-all-tasks`).
    TaskResumeAll,
    /// Story 6.4: Enter reorder mode for a pending task (palette `!reorder-task` over the
    /// Tasks tab's selection).
    TaskReorderEnter(crate::adapters::tui::mission_control::tasks::TaskRef),
    /// Story 6.4: Move reorder target up/down.
    TaskReorderMove(Direction),
    /// Story 6.4: Commit reorder (Enter).
    TaskReorderCommit,
    /// Story 6.4: Cancel reorder (Esc).
    TaskReorderCancel,
    /// `Enter` on an Agents row: open the agent's window (the host refreshes its transcript).
    OpenAgentWindow(crate::domain::models::AgentId),
    /// `p` on an agent: pause it when running, resume it when suspended (one `Op`).
    AgentPauseResume(crate::domain::models::AgentId),
    /// `x` on an agent: ask to kill it (raises the confirmation decision; never a direct op).
    AgentKill(crate::domain::models::AgentId),
    /// `Ctrl+X a` / the palette: open the Adapters window with a fresh health snapshot.
    OpenAdapters,
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
    /// Initialize a fresh Search frame (Ctrl+F from empty Input).
    // Covers: Story 4-4, AC1 (UX-DR86)
    OpenSearch,
    /// Back from Search: restore the owning Stage's exact pre-search view.
    // Covers: Story 4-4, AC4
    CloseSearch,
    /// Query string changed — event loop should re-run `find_matches`
    /// and apply the calm-jump rule. Fired for printable chars and Backspace
    /// in the Typing sub-state.
    // Covers: Story 4-4, AC2
    SearchQueryChanged,
    /// Enter commits Typing with matches, or closes Navigating at its position.
    // Covers: Story 4-4, AC3
    SearchCommit,
    /// `n` in Navigating — advance focused match (wraps at end).
    // Covers: Story 4-4, AC3
    SearchNext,
    /// `N` (Shift+n) in Navigating — reverse focused match (wraps at start).
    // Covers: Story 4-4, AC3
    SearchPrev,
    /// A content edit in Navigating returns to Typing and rescans. The owning
    /// TextField has already applied it at its caret; motions and unchanged
    /// edits return `Consumed` instead, without rescanning or navigating.
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
    /// Open the cross-conversation search overlay (`/` in the History window).
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
    /// Explicit line-count scrolling from mouse input or a compact proposal Review.
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
    /// Sign-in overlay request the event loop's login handler performs.
    Login(LoginAction),
    /// Open the profile switcher overlay (Story 8.4 AC-1).
    OpenProfileSwitcher,
    /// Profile switch requested via palette or CLI — bypasses the modal list, goes
    /// straight to preview card pre-selected on the target profile.
    ProfileSwitchRequested(String),
    /// User confirmed the transition preview — execute the swap.
    ConfirmProfileSwitch(String),
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
    /// A rail key of the interim decision pane (Story 21.1 S2 AC9): the effect
    /// was already resolved once by the pane; the host runs it.
    Decision(crate::adapters::tui::decision_queue::DecisionEffect),
    /// `Ctrl+X q`: focus the oldest waiting decision (Story 21.1 S2 AC10).
    FocusDecisions,
    /// Story 14.3a: rerun the spoke at this slot.
    WaveRerunSpoke(usize),
    /// Story 14.3a: drill into spoke body at this slot.
    WaveDrillSpoke(usize),
    /// Story 14.3a: open wave overlay (expand wave strip).
    OpenWaveOverlay,
    /// Story 14.3a: close wave overlay.
    CloseWaveOverlay,
    /// Story 14.3a: open diverge view (d key).
    OpenDivergeView,
    /// Story 14.3a: close diverge view (s key, back to wave overlay).
    CloseDivergeView,
    /// Story 14.3c: dismiss the completed/cancelled fan-out wave display (Esc
    /// in Chat focus when a wave is present, no overlay/diverge open).
    DismissWave,
    /// Story 14.3a: confirm spawn gate (Enter in gate card).
    SpawnGateConfirm,
}

impl InputAction {
    pub fn is_mutation(&self) -> bool {
        // A decision answer carries its payload (a reply, an event): dropping it on a
        // read-only tab would lose the answer after its item resolved (6B-01).
        matches!(
            self,
            InputAction::SubmitMessage(_) | InputAction::SubmitWithContext { .. }
        )
    }
}

/// Preserve raw key ownership before conversion discards modifiers and kind: the decision
/// frame's deciding keys and the thread frame's writer letters require a plain press.
pub fn frame_ignores_raw_key(state: &TuiState, event: &crossterm::event::Event) -> bool {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
    // Conversion loses kind and modifiers. Only the permission's deciding keys
    // (and the feedback field's Send) require a plain press.
    if state
        .modal_stack
        .top()
        .is_some_and(|frame| frame.surface == Surface::Decision)
        && let Event::Key(key) = event
    {
        use crate::adapters::tui::keymap::Context as C;
        let context = crate::adapters::tui::decision_queue::frame_context(state);
        // Only the deciding keys of a frame require a plain press: its letters, `Enter`
        // (a field's Send, the idle question's Answer, the spawn gate's Confirm) and,
        // for a question, `Ctrl+D` (Press only; it carries Ctrl by design).
        let letters: &[char] = match context {
            C::Permission => &['y', 's', 'a', 'n', 'f', 'r'],
            C::SkillTrust => &['y', 'n', 'i'],
            C::PlanApproval => &['y', 'a', 'n', 'e'],
            C::PlanDecision => &['y', 'e', 'n'],
            C::SkipCascade => &['s', 'a', 'n'],
            C::Delegation => &['d', 'l', 'x'],
            C::SpawnGate => &['t', 'n'],
            C::DecisionConfirm => &['y', 'n'],
            C::Forget | C::ExportOverwrite => &['y', 'n'],
            C::Rewind => &['y', 'f', 'n'],
            C::ReplyArrived => &['x'],
            C::Ticket => &['d', 'x', 'e'],
            C::TeamProposal => &['y', 'r', 'n'],
            _ => &[],
        };
        let enter_decides = matches!(
            context,
            C::DecisionCompose
                | C::Question
                | C::QuestionIdle
                | C::SpawnGate
                | C::ReplyArrived
                | C::Ticket
        );
        if matches!(context, C::Question | C::QuestionIdle)
            && key.code == KeyCode::Char('d')
            && key.modifiers == KeyModifiers::CONTROL
        {
            return key.kind != KeyEventKind::Press;
        }
        if key.code == KeyCode::Char('x') && key.modifiers == KeyModifiers::CONTROL {
            return key.kind != KeyEventKind::Press;
        }
        let deciding = match key.code {
            KeyCode::Char(c) => letters.contains(&c.to_ascii_lowercase()),
            KeyCode::Enter => enter_decides,
            _ => false,
        };
        if deciding {
            return key.kind != KeyEventKind::Press || !key.modifiers.is_empty();
        }
    }
    if state
        .modal_stack
        .top()
        .is_some_and(|frame| frame.surface == Surface::ThreadInspector)
        && let Event::Key(key) = event
    {
        return crate::adapters::tui::thread_inspector::ignores_raw_key(state, key);
    }
    if let Event::Key(key) = event
        && (crate::adapters::tui::mission_control::keys::ignores_raw_key(state, key)
            || crate::adapters::tui::artifact_window::ignores_raw_key(state, key)
            || crate::adapters::tui::agent_window::ignores_raw_key(state, key)
            || crate::adapters::tui::task_window::ignores_raw_key(state, key)
            || crate::adapters::tui::adapters_window::ignores_raw_key(state, key)
            || crate::adapters::tui::history_window::ignores_raw_key(state, key)
            || crate::adapters::tui::tour::ignores_raw_key(state, key))
    {
        return true;
    }
    false
}

/// Preserve the raw event's key ownership before dispatching its converted form.
pub fn handle_operator_input(
    state: &mut TuiState,
    raw_event: &crossterm::event::Event,
    domain_event: &DomainInputEvent,
) -> Option<InputAction> {
    if frame_ignores_raw_key(state, raw_event) {
        None
    } else {
        Some(handle_input(state, domain_event))
    }
}

/// Trusted auth-handler collaborator: settle Login intent, then the remaining legacy
/// owners and the decision queue. Keep this reviewed synchronous boundary; handlers
/// never acquire stack authority.
pub(crate) fn advance_deferred_permission(state: &mut TuiState) {
    if reducer::settle_login(state) {
        crate::adapters::tui::decision_queue::settle(state);
    }
}

/// Handle a domain input event by updating TUI state.
/// Returns an InputAction telling the event loop what to do.
pub fn handle_input(state: &mut TuiState, event: &DomainInputEvent) -> InputAction {
    with_card_focus_reconcile(state, |state| handle_input_inner(state, event))
}

/// One local key boundary, including the runtime's fact-aware S-LOG/S-QUIT dispatchers.
pub fn with_card_focus_reconcile<T>(
    state: &mut TuiState,
    dispatch: impl FnOnce(&mut TuiState) -> T,
) -> T {
    let owner_before = (
        state.modal_stack.top().map(|frame| frame.id),
        state.pending_leader,
    );
    let action = dispatch(state);
    finish_input_boundary(state, owner_before);
    action
}

/// Runtime-only interceptors may decline the event. A declined route must not
/// settle borrowed focus before the real key dispatcher sees the user's choice.
pub fn try_with_card_focus_reconcile<T>(
    state: &mut TuiState,
    dispatch: impl FnOnce(&mut TuiState) -> Option<T>,
) -> Option<T> {
    let owner_before = (
        state.modal_stack.top().map(|frame| frame.id),
        state.pending_leader,
    );
    let action = dispatch(state)?;
    finish_input_boundary(state, owner_before);
    Some(action)
}

fn finish_input_boundary(state: &mut TuiState, owner_before: (Option<u64>, Option<Context>)) {
    crate::adapters::tui::decision_queue::settle(state);
    if owner_before
        != (
            state.modal_stack.top().map(|frame| frame.id),
            state.pending_leader,
        )
    {
        state.current_hint = crate::adapters::tui::hints::contextual_hint(
            &state.focus,
            state.session_count,
            state.hint_fade_sessions(),
            state.is_vscode,
            &state.modal_stack,
            state.pending_leader,
            local_avail_ctx(state).stage_target,
        );
    }
}

fn handle_input_inner(state: &mut TuiState, event: &DomainInputEvent) -> InputAction {
    if let Some(effect) = crate::adapters::tui::presentation::route(
        state,
        event,
        Host::Local,
        &crate::adapters::tui::notification_log::LogFacts::default(),
    ) {
        return effect.into_action();
    }
    // Alt+C/L/U belong to the Login frame only (handled by its boundary); elsewhere
    // they are consumed no-ops, never the plain letter (Story 21.3 AC12, PC-13).
    let action = dispatch_input(state, event);
    if state.focus == FocusState::Input
        && !state.reverse_search.is_open()
        && matches!(
            event,
            DomainInputEvent::KeyPress(_)
                | DomainInputEvent::Paste(_)
                | DomainInputEvent::Resize(_, _)
        )
    {
        // Typing, paste and resize move the caret or the composer width: keep
        // the visual-row scroll window on the caret (Story 21.11). Special keys
        // keep the window themselves (navigation arms) or restore an exact
        // saved one (Esc closing a popup-owned slash edit, Story 21.2).
        ensure_cursor_visible(state);
    }
    action
}

/// The same ownership predicate qualifies dispatch and the generated menu hint.
pub(crate) fn ctrl_x_opens_leader(state: &TuiState) -> bool {
    state.pending_leader.is_none()
        && match state.modal_stack.top() {
            Some(frame) => frame.surface != Surface::QuitConfirm,
            None => true,
        }
}

/// Facts the registry's availability rows read in the local host (A5, A10).
pub(crate) fn local_avail_ctx(state: &TuiState) -> crate::adapters::tui::keymap::AvailCtx {
    let mut ctx = crate::adapters::tui::keymap::AvailCtx::local(state.terminal_width);
    ctx.leader_available = ctrl_x_opens_leader(state);
    ctx.frame_origin = !state.modal_stack.is_empty();
    ctx.stage_target = state.focus == FocusState::Chat
        && state.modal_stack.is_empty()
        && state.focused_item_id.is_some();
    ctx
}

/// The status row's rightmost tail for the local host (A10): the pending
/// sequence prefix, else `^X menu`, hidden while `Ctrl+X` is unavailable.
pub fn status_tail(state: &TuiState) -> crate::adapters::tui::keymap::render::StatusTail {
    use crate::adapters::tui::keymap::render::{PendingPrefix, StatusTail};
    if state
        .modal_stack
        .top()
        .is_some_and(|frame| frame.surface == Surface::QuitConfirm)
    {
        return StatusTail::Hidden;
    }
    if state.pending_leader.is_some() {
        StatusTail::Pending(PendingPrefix::Leader)
    } else if state.pending_z {
        StatusTail::Pending(PendingPrefix::Z)
    } else if state.pending_bracket == Some('[') {
        StatusTail::Pending(PendingPrefix::BracketOpen)
    } else if state.pending_bracket == Some(']') {
        StatusTail::Pending(PendingPrefix::BracketClose)
    } else if local_avail_ctx(state).leader_available {
        StatusTail::Menu
    } else {
        StatusTail::Hidden
    }
}

/// Name of the context the leader was opened from, for the no-binding notice.
fn leader_origin_name(origin: Context) -> &'static str {
    origin.name()
}

/// Retain feedback only for a frame-origin menu; base-origin dismissal is unchanged.
pub(crate) fn refuse_leader(
    state: &mut TuiState,
    origin: Context,
    command: Option<crate::adapters::tui::keymap::CommandId>,
    source: crate::adapters::tui::notifications::NoticeSource,
    message: std::borrow::Cow<'static, str>,
) {
    if state.modal_stack.is_empty() {
        crate::adapters::tui::notifications::info(state, source, message.into_owned());
    } else {
        crate::adapters::tui::notifications::info(state, source, message.as_ref());
        state.pending_leader = Some(origin);
        state.leader_feedback =
            Some(crate::adapters::tui::state::LeaderFeedback { command, message });
    }
}

/// One key of the open `Ctrl+X` leader, resolved through the registry (AC2).
/// Esc/Ctrl+C cancel silently; a key without a row posts one info notice; a
/// row that cannot run now answers with its remedy; the origin focus is
/// always restored.
pub(crate) fn handle_leader_key(
    state: &mut TuiState,
    key: crate::adapters::tui::keymap::Key,
) -> InputAction {
    use crate::adapters::tui::keymap::table::LeaderCmd;
    use crate::adapters::tui::keymap::{CommandId, Host, LeaderStep, leader_step};
    use crate::adapters::tui::notifications::{self, NoticeSource};
    let origin = state.pending_leader.take().unwrap_or(Context::Input);
    state.leader_feedback = None;
    state.needs_redraw = true;
    let avail = local_avail_ctx(state);
    let step = leader_step(Host::Local, key, &avail, leader_origin_name(origin));
    let row = match step {
        LeaderStep::Cancel => return InputAction::Consumed,
        LeaderStep::NoBinding(text) => {
            refuse_leader(state, origin, None, NoticeSource::Chord, text.into());
            return InputAction::Consumed;
        }
        LeaderStep::Unavailable(row, remedy) => {
            refuse_leader(
                state,
                origin,
                Some(row.command),
                NoticeSource::Chord,
                remedy.into(),
            );
            return InputAction::Consumed;
        }
        LeaderStep::Run(row) => row,
    };
    let CommandId::Leader(command) = row.command else {
        return InputAction::Consumed;
    };
    match command {
        LeaderCmd::Profile => InputAction::OpenProfileSwitcher,
        LeaderCmd::Model => InputAction::OpenModelSelector,
        LeaderCmd::SignIn => InputAction::Login(LoginAction::Open {
            provider: None,
            origin: crate::adapters::tui::state::LoginOrigin::Chord,
        }),
        LeaderCmd::Adapters => InputAction::OpenAdapters,
        LeaderCmd::Agents => open_agents(state),
        // The A2A log is Mission Control's Messages tab: the host reads the journal and opens it.
        LeaderCmd::A2aLog => InputAction::OpenMessages,
        LeaderCmd::Room => {
            use crate::adapters::tui::mission_control::McTab;
            presentation::reducer::open_mission_control(state, McTab::Room);
            InputAction::OpenMissionControl(McTab::Room)
        }
        LeaderCmd::Artifacts => {
            use crate::adapters::tui::mission_control::McTab;
            presentation::reducer::open_mission_control(state, McTab::Artifacts);
            InputAction::OpenMissionControl(McTab::Artifacts)
        }
        LeaderCmd::Tasks => open_tasks(state),
        LeaderCmd::History => InputAction::OpenHistory,
        LeaderCmd::Usage => InputAction::OpenUsagePanel,
        // OD-2 (A16): the registry row gated the chord on the width; the preference is only a flag.
        LeaderCmd::Monitor => {
            state.monitor = !state.monitor;
            state.needs_redraw = true;
            InputAction::Consumed
        }
        LeaderCmd::Help => {
            presentation::reducer::open(state, Surface::Help);
            InputAction::Consumed
        }
        LeaderCmd::Notifications => {
            crate::adapters::tui::notification_log::open(state);
            InputAction::Consumed
        }
        LeaderCmd::ClearToasts => {
            state.notifications.clear_toasts();
            InputAction::Consumed
        }
        LeaderCmd::Decisions => InputAction::FocusDecisions,
        // Portable multi-line send: only from the originating Input (AC12 of 21-2).
        LeaderCmd::Send if origin == Context::Input => {
            if state.input_buffer.is_empty() {
                InputAction::Consumed
            } else {
                submit_message(state)
            }
        }
        LeaderCmd::Send => {
            let text = crate::adapters::tui::keymap::resolve::no_binding_text(
                key,
                leader_origin_name(origin),
            );
            notifications::info(state, NoticeSource::Chord, text);
            InputAction::Consumed
        }
        // The shared boundary preserves a pending menu before returning a quit effect.
        LeaderCmd::QuitRequest => InputAction::Quit,
        // Attached-only: the local registry has no such row.
        LeaderCmd::Detach | LeaderCmd::Cancel => InputAction::Consumed,
    }
}

/// `Ctrl+X s` and the palette's Agents entry: Mission Control opens on the Agents tab at once;
/// the host then runs the tab's source (the subagent roster).
fn open_agents(state: &mut TuiState) -> InputAction {
    use crate::adapters::tui::mission_control::McTab;
    presentation::reducer::open_mission_control(state, McTab::Agents);
    InputAction::OpenMissionControl(McTab::Agents)
}

/// `Ctrl+X t` and the palette's Tasks entry: Mission Control opens on the Tasks tab at once;
/// the host's next paint builds the rows from the active conversation (`tasks::sync`).
fn open_tasks(state: &mut TuiState) -> InputAction {
    use crate::adapters::tui::mission_control::McTab;
    presentation::reducer::open_mission_control(state, McTab::Tasks);
    InputAction::OpenMissionControl(McTab::Tasks)
}

/// A palette entry that names a stack surface.
fn open_surface(
    state: &mut TuiState,
    target: crate::domain::models::palette::SurfaceTarget,
) -> InputAction {
    use crate::domain::models::palette::SurfaceTarget;
    match target {
        SurfaceTarget::Agents => open_agents(state),
        SurfaceTarget::Tasks => open_tasks(state),
        SurfaceTarget::Adapters => InputAction::OpenAdapters,
        SurfaceTarget::History => InputAction::OpenHistory,
    }
}

/// The palette's `Reorder task`: the reorder lives in Mission Control's Tasks tab (A17), so it
/// acts on the task selected there. With the Tasks tab not on top it opens the tab and says how
/// to choose the task instead of guessing one.
fn reorder_selected_task(state: &mut TuiState) -> InputAction {
    let selected = crate::adapters::tui::mission_control::is_top(state)
        .then(|| crate::adapters::tui::mission_control::tasks::selected_ref(state))
        .flatten();
    if let Some(task) = selected {
        return InputAction::TaskReorderEnter(task);
    }
    let action = open_tasks(state);
    crate::adapters::tui::notifications::info(
        state,
        crate::adapters::tui::notifications::NoticeSource::Palette,
        "Select a pending task, then choose Reorder task again",
    );
    action
}

fn dispatch_input(state: &mut TuiState, event: &DomainInputEvent) -> InputAction {
    // Cancel pending vim chords on any event that isn't a character key.
    if !matches!(event, DomainInputEvent::KeyPress(_)) {
        state.pending_z = false;
        state.pending_bracket = None;
        state.pending_g = false;
    }
    match event {
        DomainInputEvent::KeyPress(c) => {
            if !c.is_control() {
                state.last_printable_at = Some(state.notifications.now());
            }
            handle_char(state, *c)
        }
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
            if matches!(state.focus, FocusState::Input) && !state.reverse_search.is_open() {
                let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
                state.input_buffer.insert_str(byte_pos, text);
                // A paste is authored text, not part of the popup's provisional edit.
                state.autocomplete.slash_rollback = None;
                state.cursor_position += text.chars().count();
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }
    }
}

// ── registry dispatch (Story 21.3 AC10) ───────────────────────────────────────
// Every non-card key of the local host resolves through `keymap::resolve` and runs
// through one `execute`. The inline cards (FR-21-3-b / A9) and the login overlay keep
// their own arms.

/// A frame or routed reverse-search popup owns character keys.
fn char_overlay_context(state: &TuiState) -> Option<Context> {
    state
        .modal_stack
        .top()
        .map(|frame| presentation::frame_context(state, frame.surface))
        .or_else(|| {
            state
                .reverse_search
                .is_open()
                .then_some(Context::ReverseSearch)
        })
}

/// The context of the key target once no card owns it, top surface first.
pub(crate) fn focus_context(state: &TuiState) -> Option<Context> {
    if state.pending_leader.is_some() {
        return Some(Context::Leader);
    }
    if let Some(frame) = state.modal_stack.top() {
        return Some(presentation::frame_context(state, frame.surface));
    }
    if state.autocomplete.is_open() {
        return Some(Context::Autocomplete);
    }
    if state.reverse_search.is_open() {
        return Some(Context::ReverseSearch);
    }
    Some(match &state.focus {
        FocusState::Input => Context::Input,
        FocusState::Chat if state.wave_run.is_some() => Context::ChatWave,
        FocusState::Chat => Context::Chat,
    })
}

/// One key in `context`: its row's command runs through `execute`; a command that
/// declines in the current state hands the key to its row's parent context; a key
/// without a row takes the context's typing or default arm.
fn dispatch(state: &mut TuiState, context: Context, key: Key, host: Host) -> InputAction {
    if matches!(context, Context::Chat | Context::ChatWave)
        && let Key::Char(c) = key
    {
        if let Some(done) = chat_sequence(state, context, c, host) {
            return done;
        }
        if state.pending_g && c != 'g' {
            state.pending_g = false;
        }
    }
    let mut from = context;
    loop {
        match resolve(from, host, &[key]) {
            Resolved::Hit(row) => {
                if let Some(action) = execute(state, context, row.command, &[key], host) {
                    return action;
                }
                match row.context.parents().first() {
                    Some(parent) => from = *parent,
                    None => return rowless(state, context, key),
                }
            }
            // `z`, `[` and `]` open a Chat sequence.
            Resolved::Pending => {
                match key {
                    Key::Char('z') => state.pending_z = true,
                    Key::Char(leader) => state.pending_bracket = Some(leader),
                    _ => {}
                }
                state.needs_redraw = true;
                return InputAction::Consumed;
            }
            Resolved::Miss => return rowless(state, context, key),
        }
    }
}

/// The second key of a vim `z` / `[` / `]` sequence: the row of both keys runs, any
/// other key cancels the sequence.
fn chat_sequence(
    state: &mut TuiState,
    context: Context,
    c: char,
    host: Host,
) -> Option<InputAction> {
    let first = if state.pending_z {
        state.pending_z = false;
        'z'
    } else {
        state.pending_bracket.take()?
    };
    state.needs_redraw = true;
    let keys = [Key::Char(first), Key::Char(c)];
    Some(match resolve(context, host, &keys) {
        Resolved::Hit(row) => {
            execute(state, context, row.command, &keys, host).unwrap_or(InputAction::Consumed)
        }
        _ => InputAction::Consumed,
    })
}

/// A key without a row: typing where the context is a text field, else its default
/// (overlays consume every other key, the other contexts ignore it).
fn rowless(state: &mut TuiState, context: Context, key: Key) -> InputAction {
    let ignores = matches!(
        context,
        Context::Global | Context::Input | Context::Chat | Context::ChatWave
    );
    let Key::Char(c) = key else {
        if context == Context::Autocomplete {
            // Any other special key dismisses the popup and then acts normally.
            state.autocomplete.dismiss();
            state.needs_redraw = true;
            return dispatch(state, Context::Input, key, Host::Local);
        }
        return if ignores {
            InputAction::Ignored
        } else {
            InputAction::Consumed
        };
    };
    match context {
        context @ (Context::CommandPalette
        | Context::ReverseSearch
        | Context::SearchTyping
        | Context::SearchNavigating
        | Context::CrossSearch
        | Context::InspectorSearch
        | Context::ThreadReply) => {
            edit_query(state, context, |field| field.insert_char(c)).expect("query context")
        }
        Context::ModelSelector => model_char(state, None, c).unwrap_or(InputAction::Consumed),
        Context::Input | Context::Autocomplete => type_char(state, c, None),
        _ if ignores => InputAction::Ignored,
        _ => InputAction::Consumed,
    }
}

/// The one local executor (AC10): a resolved command runs against the state. `None`:
/// the command declines in the current state and the key goes to the row's parent.
fn execute(
    state: &mut TuiState,
    context: Context,
    command: CommandId,
    keys: &[Key],
    host: Host,
) -> Option<InputAction> {
    let key = keys[keys.len() - 1];
    Some(match command {
        CommandId::Global(cmd) => exec_global(state, cmd),
        CommandId::Input(cmd) => exec_input(state, cmd, key),
        CommandId::Field(cmd) => exec_field(state, context, cmd),
        CommandId::Chat(cmd) => exec_chat(state, cmd, keys),
        CommandId::Search(cmd) => exec_search(cmd),
        CommandId::Bookmark(cmd) => exec_bookmark(state, cmd),
        CommandId::Panel(cmd) => crate::adapters::tui::handlers::wave_overlay::command(state, cmd),
        CommandId::Overlay(cmd) => exec_overlay(state, context, cmd, key, host),
        CommandId::Viewer(_) | CommandId::Tool(_) | CommandId::Block(_) | CommandId::Body(_) => {
            match state.modal_stack.top().map(|frame| frame.surface) {
                Some(Surface::BlockViewer(_)) => {
                    crate::adapters::tui::block_viewer::on_key(state, key, host)
                }
                _ => crate::adapters::tui::tool_inspector::on_key(state, key, host),
            }
        }
        CommandId::Leader(_)
        | CommandId::Permission(_)
        | CommandId::Card(_)
        | CommandId::Thread(_)
        | CommandId::Mission(_)
        | CommandId::Tour(_) => InputAction::Consumed,
    })
}

fn exec_global(state: &mut TuiState, cmd: GlobalCmd) -> InputAction {
    match cmd {
        GlobalCmd::OpenLeader if ctrl_x_opens_leader(state) => open_leader(state),
        GlobalCmd::OpenLeader => InputAction::Ignored,
        GlobalCmd::QuitRequest => InputAction::Quit,
    }
}

/// Start an untimed leader without borrowing or replacing base/frame focus.
pub(crate) fn open_leader(state: &mut TuiState) -> InputAction {
    let origin = state
        .modal_stack
        .top()
        .map(|frame| presentation::frame_context(state, frame.surface))
        .unwrap_or(match state.focus {
            FocusState::Input => Context::Input,
            _ => Context::Chat,
        });
    if state.autocomplete.is_open() {
        state.autocomplete.dismiss();
    }
    state.reverse_search.dismiss();
    state.pending_leader = Some(origin);
    state.leader_feedback = None;
    state.needs_redraw = true;
    InputAction::Consumed
}

/// Open the palette, retaining any frame caller and closing only routed popups.
pub(crate) fn open_palette(state: &mut TuiState) -> InputAction {
    if state.autocomplete.is_open() {
        state.autocomplete.dismiss();
    }
    state.reverse_search.dismiss();
    presentation::reducer::open(state, Surface::CommandPalette);
    InputAction::Consumed
}

/// Shared frame commands; callers have already established top ownership.
pub(crate) fn dispatch_frame_key(
    state: &mut TuiState,
    context: Context,
    key: Key,
    host: Host,
) -> InputAction {
    dispatch(state, context, key, host)
}

pub(crate) fn exec_input(state: &mut TuiState, cmd: InputCmd, key: Key) -> InputAction {
    match cmd {
        InputCmd::ReturnToChat => {
            // Esc never submits: unsent text (a recalled history entry included) is kept.
            reducer::set_base(state, FocusState::Chat);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        InputCmd::SubmitOrNewline => {
            if state.multiline_mode {
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
        InputCmd::Send if !state.input_buffer.is_empty() => submit_message(state),
        InputCmd::Send => InputAction::Consumed,
        InputCmd::InsertNewline => {
            insert_newline(state);
            InputAction::Consumed
        }
        InputCmd::ToggleMultiline => {
            state.multiline_mode = !state.multiline_mode;
            state.needs_redraw = true;
            InputAction::Consumed
        }
        InputCmd::PasteClipboard => InputAction::RequestClipboardPaste,
        InputCmd::OpenPalette => open_palette(state),
        InputCmd::ReverseSearch => {
            if state.autocomplete.is_open() {
                state.autocomplete.dismiss();
            }
            state.reverse_search.reset();
            state.needs_redraw = true;
            InputAction::Consumed
        }
        InputCmd::OpenSearch if state.input_buffer.is_empty() => {
            presentation::reducer::open(state, Surface::Search);
            InputAction::OpenSearch
        }
        InputCmd::OpenSearch => {
            // Feedback instead of a silent no-op: the search needs an empty composer.
            crate::adapters::tui::notifications::info(
                state,
                crate::adapters::tui::notifications::NoticeSource::Input,
                format!(
                    "Search needs an empty input: clear it, then press {}",
                    crate::adapters::tui::keymap::render::shortcut_for(CommandId::Input(
                        InputCmd::OpenSearch
                    ))
                    .unwrap_or_default()
                ),
            );
            state.needs_redraw = true;
            InputAction::Consumed
        }
        InputCmd::NewTab => InputAction::NewTab,
        InputCmd::CancelOrQuit => InputAction::CancelOrQuit,
        InputCmd::LineEnd => {
            // Logical line (unbounded width), not the wrapped visual row End uses.
            let geometry = input_box::InputGeometry::new(&state.input_buffer, usize::MAX);
            let (row, _col) = geometry.cursor(&state.input_buffer, state.cursor_position);
            state.cursor_position = geometry.cursor_at(&state.input_buffer, row, usize::MAX);
            ensure_cursor_visible(state);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        InputCmd::LineStart => {
            state.cursor_position =
                text_edit::line_start(&state.input_buffer, state.cursor_position);
            ensure_cursor_visible(state);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        InputCmd::DeleteBack
        | InputCmd::DeleteWordBack
        | InputCmd::KillToLineStart
        | InputCmd::KillToLineEnd => {
            let edit = match cmd {
                InputCmd::DeleteWordBack => text_edit::kill_word_back,
                InputCmd::KillToLineStart => text_edit::kill_to_line_start,
                InputCmd::KillToLineEnd => text_edit::kill_to_line_end,
                _ => text_edit::delete_back,
            };
            state.cursor_position = edit(&mut state.input_buffer, state.cursor_position);
            ensure_cursor_visible(state);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        InputCmd::DeleteForward => {
            let total_chars = state.input_buffer.chars().count();
            if state.cursor_position < total_chars {
                let from = geom::snap_boundary(&state.input_buffer, state.cursor_position);
                let to = geom::next_boundary(&state.input_buffer, from);
                let (start, end) = (
                    char_to_byte(&state.input_buffer, from),
                    char_to_byte(&state.input_buffer, to),
                );
                state.input_buffer.replace_range(start..end, "");
                state.cursor_position = from;
                ensure_cursor_visible(state);
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }
        InputCmd::CursorLeft => {
            state.cursor_position = geom::prev_boundary(&state.input_buffer, state.cursor_position);
            ensure_cursor_visible(state);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        InputCmd::CursorRight => {
            if state.cursor_position < state.input_buffer.chars().count() {
                state.cursor_position =
                    geom::next_boundary(&state.input_buffer, state.cursor_position);
                ensure_cursor_visible(state);
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }
        InputCmd::CursorHome | InputCmd::CursorEnd => {
            // Start / end of the current visual row.
            let geometry = composer_geometry(state);
            let (row, _col) = geometry.cursor(&state.input_buffer, state.cursor_position);
            let col = if cmd == InputCmd::CursorHome {
                0
            } else {
                usize::MAX
            };
            state.cursor_position = geometry.cursor_at(&state.input_buffer, row, col);
            ensure_cursor_visible(state);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        InputCmd::UpOrHistoryPrev => {
            let geometry = composer_geometry(state);
            let has_newline = state.input_buffer.contains('\n');
            let (row, col) = geometry.cursor(&state.input_buffer, state.cursor_position);

            if row > 0 && !state.input_history.is_navigating() {
                // Move cursor up one visual row (only if not already navigating history)
                state.cursor_position = geometry.cursor_at(&state.input_buffer, row - 1, col);
                ensure_cursor_visible(state);
                state.needs_redraw = true;
            } else if state.input_buffer.is_empty()
                || state.input_history.is_navigating()
                || !has_newline
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
        InputCmd::DownOrHistoryNext => {
            let geometry = composer_geometry(state);
            let has_multiline_content = geometry.row_count() > 1;
            let (row, col) = geometry.cursor(&state.input_buffer, state.cursor_position);

            if has_multiline_content
                && row + 1 < geometry.row_count()
                && !state.input_history.is_navigating()
            {
                // Move cursor down one visual row (only if not already navigating history)
                state.cursor_position = geometry.cursor_at(&state.input_buffer, row + 1, col);
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
        InputCmd::CycleMode => InputAction::CycleMode,
        InputCmd::OpenSlashPopup | InputCmd::OpenMentionPopup => match key {
            Key::Char(c) => type_char(state, c, Some(cmd)),
            _ => InputAction::Consumed,
        },
        // Attached-only rows: the local host has none of them.
        InputCmd::TranscriptPageUp | InputCmd::TranscriptPageDown => InputAction::Consumed,
    }
}

/// Edit the owning query, or retain tail semantics for an unmigrated field.
fn exec_field(state: &mut TuiState, context: Context, cmd: FieldCmd) -> InputAction {
    if let Some(action) = edit_query(state, context, |field| field.edit(cmd)) {
        return action;
    }
    let key = match cmd {
        FieldCmd::DeleteWordBack => DomainKey::CtrlW,
        FieldCmd::Clear => DomainKey::CtrlU,
        FieldCmd::DeleteBack => DomainKey::CtrlH,
        _ => return InputAction::Consumed,
    };
    match context {
        Context::ModelSelector => {
            if state.model_selector.search_active {
                edit_field(&mut state.model_selector.search_query, key);
                state.model_selector.recompute_filter();
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }
        _ => InputAction::Consumed,
    }
}

fn exec_chat(state: &mut TuiState, cmd: ChatCmd, keys: &[Key]) -> InputAction {
    match cmd {
        ChatCmd::FocusToggle | ChatCmd::FocusInput => {
            reducer::set_base(state, FocusState::Input);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        ChatCmd::Quit => InputAction::Quit,
        ChatCmd::OpenHelp => {
            presentation::reducer::open(state, Surface::Help);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        ChatCmd::OpenPalette => open_palette(state),
        ChatCmd::CancelOrQuit => InputAction::CancelOrQuit,
        ChatCmd::ScrollHalfPageDown => InputAction::ScrollHalfPageDown,
        ChatCmd::ScrollHalfPageUp => InputAction::ScrollHalfPageUp,
        ChatCmd::ScrollPageDown => InputAction::ScrollFullPageDown,
        ChatCmd::ScrollPageUp => InputAction::ScrollFullPageUp,
        ChatCmd::ScrollLineDown => {
            state.needs_redraw = true;
            InputAction::ScrollLineDown
        }
        ChatCmd::ScrollLineUp => {
            state.needs_redraw = true;
            InputAction::ScrollLineUp
        }
        ChatCmd::ScrollToBottom => {
            state.needs_redraw = true;
            InputAction::ScrollToBottom
        }
        // A single `g` jumps to the top and arms `gg`; the second `g` is idempotent.
        ChatCmd::ScrollToTop if keys.len() == 1 => {
            let was_gg = state.pending_g;
            state.pending_g = !was_gg;
            state.needs_redraw = true;
            if was_gg {
                InputAction::Consumed
            } else {
                InputAction::ScrollToTop
            }
        }
        ChatCmd::ScrollToTop => InputAction::ScrollToTop,
        ChatCmd::BlockJumpNext => block_jump(state, false, Direction::Down),
        ChatCmd::BlockJumpPrev => block_jump(state, false, Direction::Up),
        ChatCmd::UserMessagePrev => block_jump(state, true, Direction::Up),
        ChatCmd::UserMessageNext => block_jump(state, true, Direction::Down),
        ChatCmd::CycleInvocation => InputAction::CycleInvocationInFocusedTurn,
        ChatCmd::PrevTab => InputAction::SwitchToPrevTab,
        ChatCmd::NewTab => InputAction::NewTab,
        ChatCmd::SwitchToTab(n) => InputAction::SwitchToTab(usize::from(n)),
        ChatCmd::PasteClipboard => InputAction::RequestClipboardPaste,
        ChatCmd::Inspect => {
            if state.focused_item_id.is_some() {
                InputAction::InspectTool
            } else {
                InputAction::Consumed
            }
        }
        ChatCmd::Copy => InputAction::CopyToClipboard(String::new()),
        ChatCmd::ForkAtMessage => InputAction::ForkAtMessage,
        ChatCmd::RewindAtMessage => InputAction::RewindAtMessage,
        ChatCmd::ToggleBookmark => InputAction::ToggleBookmark,
        ChatCmd::OpenBookmarkList => InputAction::OpenBookmarkList,
        ChatCmd::FoldToggle => InputAction::FoldToggleAtFocus,
        ChatCmd::FoldCollapse => InputAction::CollapseFocus,
        ChatCmd::FoldExpand => InputAction::ExpandFocus,
        ChatCmd::CollapseAllTurns => InputAction::CollapseAllTurns,
        ChatCmd::ExpandAllTurns => InputAction::ExpandAllTurns,
        ChatCmd::ToggleSummaryTier => InputAction::ToggleSummaryTier,
        ChatCmd::RecenterAnchor => InputAction::RecenterAnchor,
        ChatCmd::NextProseAnchor => InputAction::JumpProseAnchor(Direction::Down),
        ChatCmd::PrevProseAnchor => InputAction::JumpProseAnchor(Direction::Up),
        ChatCmd::LatestProseAnchor => InputAction::JumpToLatestProseAnchor,
    }
}

/// `J` `K` `{` `}`: jump to the next or previous content block or user message.
fn block_jump(state: &mut TuiState, user_messages: bool, dir: Direction) -> InputAction {
    let boundaries = if user_messages {
        &state.user_message_boundaries
    } else {
        &state.block_boundaries
    };
    let down = matches!(dir, Direction::Down);
    match find_next_boundary(
        state.scroll_snapshot,
        boundaries,
        dir,
        state.total_content_height,
        state.viewport_height as usize,
    ) {
        Some(offset) => {
            state.needs_redraw = true;
            InputAction::BlockJump {
                offset,
                auto_scroll: down && offset == 0,
            }
        }
        None => InputAction::Consumed,
    }
}

fn exec_search(cmd: SearchCmd) -> InputAction {
    match cmd {
        SearchCmd::Close => InputAction::CloseSearch,
        SearchCmd::Commit => InputAction::SearchCommit,
        SearchCmd::NextMatch => InputAction::SearchNext,
        SearchCmd::PrevMatch => InputAction::SearchPrev,
    }
}

/// Edit only the owning field; cursor-only changes redraw but never rescan.
fn edit_query(
    state: &mut TuiState,
    context: Context,
    edit: impl FnOnce(&mut TextField) -> bool,
) -> Option<InputAction> {
    use crate::adapters::tui::state::SearchSubstate;
    let changed = match context {
        Context::CommandPalette => {
            let changed = edit(&mut state.command_palette.filter_text);
            if changed {
                state.command_palette.current_scope = state
                    .command_palette
                    .filter_text
                    .as_str()
                    .chars()
                    .next()
                    .and_then(crate::adapters::palette_registry::PaletteRegistry::scope_for_prefix);
            }
            changed
        }
        Context::ReverseSearch => edit(&mut state.reverse_search.query),
        Context::SearchTyping | Context::SearchNavigating => {
            state.search_state.substate = SearchSubstate::Typing;
            edit(&mut state.search_state.query)
        }
        Context::CrossSearch => edit(&mut state.cross_search.query),
        Context::ThreadReply => edit(&mut state.thread_inspector.as_mut()?.reply.as_mut()?.field),
        Context::InspectorSearch => {
            let changed = edit(&mut presentation::inspector_search_mut(state)?.query);
            if changed {
                match state.modal_stack.top().map(|frame| frame.surface) {
                    Some(Surface::Help) => state.help_overlay.filtered_scroll_offset = 0,
                    Some(Surface::NotificationLog) => {
                        crate::adapters::tui::notification_log::reconcile_search(state);
                    }
                    Some(Surface::ToolInspector(_) | Surface::BlockViewer(_)) => {}
                    Some(Surface::ThreadInspector) => {
                        crate::adapters::tui::thread_inspector::search_changed(state);
                    }
                    Some(Surface::MissionControl) => {
                        crate::adapters::tui::mission_control::search_changed(state);
                    }
                    Some(Surface::ArtifactWindow) => {
                        crate::adapters::tui::artifact_window::search_changed(state);
                    }
                    Some(Surface::AgentWindow) => {
                        crate::adapters::tui::agent_window::search_changed(state);
                    }
                    _ => unreachable!("only inspector frames lend the child field"),
                }
            }
            changed
        }
        _ => return None,
    };
    state.needs_redraw = true;
    Some(if !changed {
        InputAction::Consumed
    } else {
        match context {
            Context::ReverseSearch => {
                update_reverse_search_matches(state);
                InputAction::Consumed
            }
            Context::SearchNavigating => InputAction::SearchReturnToTyping,
            Context::SearchTyping => InputAction::SearchQueryChanged,
            Context::CrossSearch => InputAction::CrossSearchQueryChanged,
            _ => InputAction::Consumed,
        }
    })
}

/// The borrowed paste seam edits only fields actually offered by this host.
pub(crate) fn paste_query(state: &mut TuiState, text: &str, host: Host) -> Option<InputAction> {
    let context = focus_context(state)?;
    if matches!(context, Context::DecisionCompose | Context::Question) {
        // The frame's own field (never the composer): control characters become spaces.
        crate::adapters::tui::decision_queue::paste(state, text);
        state.needs_redraw = true;
        return Some(InputAction::Consumed);
    }
    if host == Host::Attached
        && !matches!(
            context,
            Context::CommandPalette | Context::InspectorSearch | Context::ThreadReply
        )
    {
        return None;
    }
    edit_query(state, context, |field| field.insert(text))
}

fn exec_bookmark(state: &mut TuiState, cmd: BookmarkCmd) -> InputAction {
    match cmd {
        BookmarkCmd::SelectNext => {
            let last = state.bookmark_list_count.saturating_sub(1);
            state.bookmark_list_selected = state.bookmark_list_selected.saturating_add(1).min(last);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        BookmarkCmd::SelectPrev => {
            state.bookmark_list_selected = state.bookmark_list_selected.saturating_sub(1);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        BookmarkCmd::Delete => InputAction::DeleteBookmark,
        BookmarkCmd::Undo => InputAction::UndoBookmarkDelete,
        BookmarkCmd::Jump => InputAction::JumpToBookmark,
        BookmarkCmd::Close => {
            crate::adapters::tui::handlers::bookmark::close(state);
            InputAction::CloseBookmarkList
        }
    }
}
fn exec_overlay(
    state: &mut TuiState,
    context: Context,
    cmd: OverlayCmd,
    key: Key,
    host: Host,
) -> InputAction {
    match context {
        Context::Help => exec_help(state, cmd),
        Context::CommandPalette => exec_palette(state, cmd, host),
        Context::ModelSelector => exec_model_selector(state, cmd, key),
        Context::ProfileSwitcher => exec_profile(state, cmd),
        Context::UsagePanel => exec_usage(state, cmd),
        Context::CrossSearch => exec_cross_search(state, cmd),
        Context::InspectorSearch => exec_inspector_search(state, cmd),
        Context::ReverseSearch => exec_reverse_search(state, cmd),
        Context::Autocomplete => exec_autocomplete(state, cmd),
        // The host-neutral frames (S-QUIT, S-LOG, decisions, the thread window) and the
        // login overlay have their own owners.
        _ => InputAction::Consumed,
    }
}

fn filtered_help_scroll_limit(state: &TuiState) -> usize {
    let area = ratatui::layout::Rect::new(0, 0, state.terminal_width, state.terminal_height);
    // Tab count moves the Stage's top, not its bottom; only the modal region
    // is needed here. Attached callers lend their actual composer for this call.
    let rows = crate::adapters::tui::layout::compute_layout(
        area,
        &state.theme,
        &state.input_buffer,
        1,
        state.monitor,
    )
    .map(|layout| {
        presentation::geometry::frame_rect(presentation::geometry::modal_region(&layout, area))
            .height
            .saturating_sub(3) // Two borders and the retained query row.
    })
    .unwrap_or(0);
    crate::adapters::tui::widgets::help_overlay::filtered_row_count(&state.help_overlay.search)
        .saturating_sub(usize::from(rows))
}

fn exec_help(state: &mut TuiState, cmd: OverlayCmd) -> InputAction {
    let filtered_limit = (!state.help_overlay.search.query.is_empty()
        && matches!(
            cmd,
            OverlayCmd::HelpDown | OverlayCmd::HelpUp | OverlayCmd::HelpBottom
        ))
    .then(|| filtered_help_scroll_limit(state));
    match cmd {
        OverlayCmd::InspectorSearch => return exec_inspector_search(state, cmd),
        OverlayCmd::HelpDown => {
            let offset = state.help_overlay.list_scroll_mut();
            *offset = filtered_limit.map_or_else(
                || offset.saturating_add(1),
                |max| (*offset).min(max).saturating_add(1).min(max),
            );
        }
        OverlayCmd::HelpUp => {
            let offset = state.help_overlay.list_scroll_mut();
            *offset = filtered_limit.map_or_else(
                || offset.saturating_sub(1),
                |max| (*offset).min(max).saturating_sub(1),
            );
        }
        // Preserve the legacy unfiltered sentinel; filtered positions are bounded.
        OverlayCmd::HelpBottom => {
            *state.help_overlay.list_scroll_mut() = filtered_limit.unwrap_or(usize::MAX / 2);
        }
        OverlayCmd::HelpTop => *state.help_overlay.list_scroll_mut() = 0,
        OverlayCmd::HelpClose => presentation::reducer::close(state, Surface::Help),
        OverlayCmd::QuitRequest => return InputAction::Quit,
        _ => {}
    }
    state.needs_redraw = true;
    InputAction::Consumed
}

fn exec_inspector_search(state: &mut TuiState, cmd: OverlayCmd) -> InputAction {
    if cmd == OverlayCmd::QuitRequest {
        return InputAction::Quit;
    }
    if state
        .modal_stack
        .top()
        .is_some_and(|frame| frame.surface == Surface::ThreadInspector)
    {
        match cmd {
            OverlayCmd::InspectorAccept => {
                crate::adapters::tui::thread_inspector::accept_search(state)
            }
            OverlayCmd::InspectorBack => {
                if let Some(thread) = state.thread_inspector.as_mut() {
                    thread.search.back();
                }
                crate::adapters::tui::thread_inspector::search_changed(state);
            }
            _ => {}
        }
        state.needs_redraw = true;
        return InputAction::Consumed;
    }
    if let Some(
        surface @ (Surface::MissionControl | Surface::ArtifactWindow | Surface::AgentWindow),
    ) = state.modal_stack.top().map(|frame| frame.surface)
    {
        match (surface, cmd) {
            (Surface::MissionControl, OverlayCmd::InspectorAccept) => {
                crate::adapters::tui::mission_control::accept_search(state);
            }
            (Surface::MissionControl, OverlayCmd::InspectorBack) => {
                crate::adapters::tui::mission_control::clear_search(state);
            }
            (Surface::AgentWindow, OverlayCmd::InspectorAccept) => {
                crate::adapters::tui::agent_window::accept_search(state);
            }
            (Surface::AgentWindow, OverlayCmd::InspectorBack) => {
                crate::adapters::tui::agent_window::clear_search(state);
            }
            (_, OverlayCmd::InspectorAccept) => {
                crate::adapters::tui::artifact_window::accept_search(state);
            }
            (_, OverlayCmd::InspectorBack) => {
                crate::adapters::tui::artifact_window::clear_search(state);
            }
            _ => {}
        }
        state.needs_redraw = true;
        return InputAction::Consumed;
    }
    if let Some(Surface::ToolInspector(key)) = state.modal_stack.top().map(|frame| frame.surface) {
        let tool = state
            .tool_inspectors
            .get_mut(&key.0)
            .expect("frame owns tool search");
        if tool.missing {
            return InputAction::Consumed;
        }
        match cmd {
            OverlayCmd::InspectorSearch => tool.search.editor.open(),
            OverlayCmd::InspectorAccept => tool.accept_search(),
            OverlayCmd::InspectorBack => tool.clear_search(),
            _ => return InputAction::Consumed,
        }
        state.needs_redraw = true;
        return InputAction::Consumed;
    }
    if let Some(Surface::BlockViewer(key)) = state.modal_stack.top().map(|frame| frame.surface) {
        let block = state
            .block_viewers
            .get_mut(&key.0)
            .expect("frame owns block search");
        if block.missing {
            return InputAction::Consumed;
        }
        match cmd {
            OverlayCmd::InspectorSearch => block.search.editor.open(),
            OverlayCmd::InspectorAccept => block.accept_search(&state.theme),
            OverlayCmd::InspectorBack => block.clear_search(),
            _ => return InputAction::Consumed,
        }
        state.needs_redraw = true;
        return InputAction::Consumed;
    }
    let Some(search) = presentation::inspector_search_mut(state) else {
        return InputAction::Consumed;
    };
    match cmd {
        OverlayCmd::InspectorSearch => search.open(),
        OverlayCmd::InspectorAccept => search.accept(),
        OverlayCmd::InspectorBack => search.back(),
        _ => return InputAction::Consumed,
    }
    if cmd == OverlayCmd::InspectorBack {
        match state.modal_stack.top().map(|frame| frame.surface) {
            Some(Surface::Help) => state.help_overlay.filtered_scroll_offset = 0,
            Some(Surface::NotificationLog) => {
                crate::adapters::tui::notification_log::reconcile_search(state);
            }
            _ => unreachable!("only Log/Help lend the inspector field"),
        }
    }
    state.needs_redraw = true;
    InputAction::Consumed
}

fn exec_cross_search(state: &mut TuiState, cmd: OverlayCmd) -> InputAction {
    match cmd {
        OverlayCmd::CrossClose => InputAction::CloseCrossSearch,
        OverlayCmd::CrossOpen if !state.cross_search.results.is_empty() => {
            InputAction::OpenCrossSearchResult
        }
        OverlayCmd::CrossPrev => {
            state.cross_search.selected = state.cross_search.selected.saturating_sub(1);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        OverlayCmd::CrossNext => {
            if !state.cross_search.results.is_empty() {
                let last = state.cross_search.results.len() - 1;
                state.cross_search.selected = (state.cross_search.selected + 1).min(last);
            }
            state.needs_redraw = true;
            InputAction::Consumed
        }
        _ => InputAction::Consumed,
    }
}

fn exec_profile(state: &mut TuiState, cmd: OverlayCmd) -> InputAction {
    let previewing = state.profile_switcher.preview.is_some();
    match cmd {
        OverlayCmd::ProfileEnter | OverlayCmd::ProfileYes if previewing => {
            let name = state
                .profile_switcher
                .selected_profile()
                .map(|p| p.name.clone())
                .unwrap_or_default();
            state.needs_redraw = true;
            return InputAction::ConfirmProfileSwitch(name);
        }
        OverlayCmd::ProfileBack | OverlayCmd::ProfileNo if previewing => {
            state.profile_switcher.exit_preview();
            state.needs_redraw = true;
        }
        _ if previewing => {}
        OverlayCmd::ProfilePrev => {
            state.profile_switcher.navigate(-1);
            state.needs_redraw = true;
        }
        OverlayCmd::ProfileNext => {
            state.profile_switcher.navigate(1);
            state.needs_redraw = true;
        }
        OverlayCmd::ProfileEnter => profile_switch::preview(state, None),
        OverlayCmd::ProfileBack => presentation::reducer::close(state, Surface::ProfileSwitcher),
        _ => {}
    }
    InputAction::Consumed
}

/// Characters of the model selector (Story 7.2, 7.6): a pending context warning answers
/// only `y` / `n`, an active search types, `/` starts the search on a sendable
/// catalogue. `None`: the key is a navigation row.
fn model_char(state: &mut TuiState, command: Option<OverlayCmd>, c: char) -> Option<InputAction> {
    let selector = &mut state.model_selector;
    if selector.pending_context_warning.is_some() {
        return Some(match command {
            Some(OverlayCmd::ModelWarnYes) => match selector.pending_context_warning.take() {
                Some(w) => InputAction::CompactThenSwitchModel {
                    provider_id: w.provider_id,
                    model_id: w.model_id,
                },
                None => InputAction::Consumed,
            },
            Some(OverlayCmd::ModelWarnNo) => {
                selector.pending_context_warning = None;
                state.needs_redraw = true;
                InputAction::Consumed
            }
            _ => InputAction::Consumed,
        });
    }
    if selector.search_active {
        if c.is_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | ' ' | ':' | '+') {
            selector.search_query.push(c);
            selector.recompute_filter();
            state.needs_redraw = true;
        }
        return Some(InputAction::Consumed);
    }
    // A locked catalogue advertises only the arrows and Esc: typing starts no hidden search.
    let locked = selector
        .columns
        .get(selector.selected_provider)
        .is_some_and(|column| column.locked_hint.is_some());
    match command {
        Some(OverlayCmd::ModelSearch) => {
            if !locked {
                selector.search_active = true;
                selector.search_query.clear();
                selector.recompute_filter();
                state.needs_redraw = true;
            }
            Some(InputAction::Consumed)
        }
        Some(OverlayCmd::ModelWarnYes | OverlayCmd::ModelWarnNo) | None => {
            Some(InputAction::Consumed)
        }
        Some(_) => None,
    }
}

fn exec_model_selector(state: &mut TuiState, cmd: OverlayCmd, key: Key) -> InputAction {
    let typed = matches!(key, Key::Char(_));
    if let Key::Char(c) = key
        && let Some(done) = model_char(state, Some(cmd), c)
    {
        return done;
    }
    match cmd {
        OverlayCmd::ModelProviderPrev => {
            state.model_selector.navigate_provider(Direction::Left);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        OverlayCmd::ModelProviderNext => {
            state.model_selector.navigate_provider(Direction::Right);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        OverlayCmd::ModelPrev if typed => {
            state.model_selector.navigate_model(Direction::Up);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        OverlayCmd::ModelPrev => {
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
        OverlayCmd::ModelNext if typed => {
            state.model_selector.navigate_model(Direction::Down);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        OverlayCmd::ModelNext => {
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
        OverlayCmd::ModelSelect => {
            if state.model_selector.pending_context_warning.is_some() {
                return InputAction::Consumed;
            }
            if let Some((provider_id, model)) = state.model_selector.selected() {
                return InputAction::SwitchModelProvider {
                    provider_id: Some(provider_id.to_string()),
                    model_id: model.model_id.clone(),
                };
            }
            // A locked column that a sign-in can unlock starts that login; a
            // hint-only column (env/config) stays open.
            let column = state
                .model_selector
                .columns
                .get(state.model_selector.selected_provider)
                .filter(|column| column.locked_hint.is_some());
            if let Some(column) = column
                && let Some(provider) = state.model_selector.sign_in.get(&column.provider_id)
            {
                return InputAction::Login(LoginAction::Open {
                    provider: Some(provider.clone()),
                    origin: crate::adapters::tui::state::LoginOrigin::Picker {
                        column: column.provider_id.clone(),
                    },
                });
            }
            InputAction::Consumed
        }
        OverlayCmd::ModelBack => {
            if state.model_selector.search_active {
                state.model_selector.search_active = false;
                state.model_selector.search_query.clear();
                state.model_selector.filtered_indices.clear();
                state.needs_redraw = true;
                return InputAction::Consumed;
            }
            presentation::reducer::close(state, Surface::ModelSelector);
            InputAction::Consumed
        }
        OverlayCmd::ModelBackspace => {
            if state.model_selector.search_active {
                state.model_selector.search_query.pop();
                state.model_selector.recompute_filter();
                state.needs_redraw = true;
                return InputAction::Consumed;
            }
            InputAction::Ignored
        }
        _ => InputAction::Consumed,
    }
}

fn exec_reverse_search(state: &mut TuiState, cmd: OverlayCmd) -> InputAction {
    match cmd {
        OverlayCmd::RevPalette => open_palette(state),
        OverlayCmd::RevLeader => open_leader(state),
        _ => exec_reverse_search_keys(state, cmd),
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
    // The palette, help, model selector, usage panel, profile switcher and reverse
    // search own the character keys (registry contexts, AC10).
    if let Some(context) = char_overlay_context(state) {
        return dispatch(state, context, Key::Char(c), Host::Local);
    }

    match focus_context(state) {
        Some(context) => dispatch(state, context, Key::Char(c), Host::Local),
        None => InputAction::Ignored,
    }
}

/// A printable character typed into the composer. `command` is the popup-trigger row
/// (`/` or `@`) the key resolved to, if any; every other character is plain text.
pub(crate) fn type_char(state: &mut TuiState, c: char, command: Option<InputCmd>) -> InputAction {
    match state.focus {
        FocusState::Input => {
            // While autocomplete is open, intercept characters to update filter.
            // Exception: a space in SlashCommand mode terminates the command name
            // and begins arguments (e.g. `/export file.md`). Slash-command names
            // end at the first whitespace — matching the submission parser —
            // so dismissing here lets ENTER submit `/cmd args` as a whole.
            // Without this, the ENTER handler below (DomainKey::Enter arm) consumes
            // the submit and the command never dispatches.
            if let Some(kind) = state.autocomplete.kind {
                if kind == AutocompleteKind::SlashCommand && c == ' ' {
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
                    state.autocomplete.filter_text = if kind == AutocompleteKind::FileMention
                        && filter.to_lowercase() == "mcp/"
                    {
                        state.autocomplete.kind = Some(AutocompleteKind::McpMention);
                        String::new()
                    } else if kind == AutocompleteKind::FileMention
                        && filter.eq_ignore_ascii_case("a2a/")
                    {
                        state.autocomplete.kind = Some(AutocompleteKind::A2aMention);
                        String::new()
                    } else if kind == AutocompleteKind::FileMention && filter == "Agents/" {
                        state.autocomplete.kind = Some(AutocompleteKind::AgentMention);
                        String::new()
                    } else if kind == AutocompleteKind::AgentMention {
                        let after_slash = filter.strip_prefix("Agents/").unwrap_or(&filter);
                        after_slash.to_string()
                    } else if kind == AutocompleteKind::McpMention {
                        let filter_lower = filter.to_lowercase();
                        let after_slash =
                            filter_lower.strip_prefix("mcp/").unwrap_or(&filter_lower);
                        after_slash.to_string()
                    } else if kind == AutocompleteKind::A2aMention {
                        // Namespace token is case-insensitive; the suffix keeps
                        // the user's casing (peer/skill IDs are case-sensitive).
                        let after_slash = strip_a2a_namespace(&filter);
                        after_slash.to_string()
                    } else {
                        filter
                    };
                    state.needs_redraw = true;
                    return InputAction::Consumed;
                }
            }

            // Detect '/' at position 0 → trigger slash command autocomplete.
            // The popup owns this provisional edit: save the pre-trigger
            // composer so Esc can restore it (Story 21-2 A2).
            if command == Some(InputCmd::OpenSlashPopup)
                && state.cursor_position == 0
                && state.input_buffer.is_empty()
            {
                state
                    .autocomplete
                    .open(AutocompleteKind::SlashCommand, 0, Vec::new());
                state.autocomplete.own_slash_edit(
                    &state.input_buffer,
                    state.cursor_position,
                    state.input_scroll_offset,
                );
                let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
                state.input_buffer.insert(byte_pos, c);
                state.cursor_position += 1;
                // Suggestions will be populated by the event loop (lazy loading)
                state.needs_redraw = true;
                return InputAction::Consumed;
            }

            // Detect '@' anywhere → trigger file mention autocomplete
            if command == Some(InputCmd::OpenMentionPopup) {
                let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
                state.input_buffer.insert(byte_pos, c);
                let trigger_pos = state.cursor_position;
                state.cursor_position += 1;
                state
                    .autocomplete
                    .open(AutocompleteKind::FileMention, trigger_pos, Vec::new());
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
        _ => InputAction::Ignored,
    }
}

fn handle_special_key(state: &mut TuiState, key: DomainKey) -> InputAction {
    // Cancel pending vim chords on any special key.
    if state.pending_z || state.pending_bracket.is_some() || state.pending_g {
        state.pending_z = false;
        state.pending_bracket = None;
        state.pending_g = false;
        state.needs_redraw = true;
    }

    let k = Key::from_domain_key(key);

    // Routed Input popups retain their legacy key behavior until Bb2.
    let early = if state.autocomplete.is_open() && state.focus == FocusState::Input {
        Some(Context::Autocomplete)
    } else if state.reverse_search.is_open() {
        Some(Context::ReverseSearch)
    } else {
        None
    };
    if let Some(context) = early {
        return dispatch(state, context, k, Host::Local);
    }

    let context = focus_context(state);
    dispatch(state, context.unwrap_or(Context::Global), k, Host::Local)
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
/// Test seam: `submit_message` is the production parser that decides whether a
/// `/command` reaches its event-loop dispatch arm or falls through to the
/// user-defined-command path. Integration keystones MUST enter here — a
/// handler-only test stays green while the command is silently dead.
pub fn submit_message_for_test(state: &mut TuiState) -> InputAction {
    submit_message(state)
}

fn submit_message(state: &mut TuiState) -> InputAction {
    let text = state.input_buffer.clone();
    state.input_history.push(text.clone());
    state.input_history.reset_navigation();
    state.cursor_position = 0;
    state.input_scroll_offset = 0;
    state.autocomplete.dismiss();
    state.needs_redraw = true;
    // 21-17: the previous `/room role` and `/peer` views leave before this submission runs.
    crate::adapters::tui::views::retire_on_submit(&mut state.feedback_blocks);

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

    if let Some((cmd_name, args)) = parse_slash_command(&text) {
        // Built-ins: ONE authority (`builtin_commands`) decides which names the
        // local runtime dispatcher handles — the same table the catalogue
        // offers from — so a command that discovery shows is never silently
        // sent down the user-defined-command path (Story 21.2 AC1). Each such
        // name reaches its real `ExecuteCommand` arm; WITHOUT its entry a name
        // falls through to SubmitWithContext, resolves no command file and
        // dispatches an empty turn (the 14.3c `/fanout` failure class).
        // `/ml` (multi-line toggle), `/quit`, `/new` and `/clear` take no args.
        if let Some(builtin) = crate::adapters::builtin_commands::local_dispatch(cmd_name) {
            return InputAction::ExecuteCommand {
                name: cmd_name.to_owned(),
                args: if builtin.takes_args {
                    args.map(str::to_owned)
                } else {
                    None
                },
            };
        }
        // Discovered skill name → activate via ExecuteCommand so the event loop
        // routes through `AskActivateSkill` (Story 5-2 AC8). Fall through to
        // user-defined-command SubmitWithContext if the name is NOT a skill.
        if state.skill_name_cache.contains(cmd_name) {
            return InputAction::ExecuteCommand {
                name: cmd_name.to_owned(),
                args: args.map(str::to_owned),
            };
        }
        state.resolved_mentions.clear();
        return InputAction::SubmitWithContext {
            text: String::new(),
            command: Some(cmd_name.to_owned()),
            command_args: args.map(str::to_owned),
        };
    }

    // A plain-text message: the tour's first step is done (slash commands and `@Agents/`
    // returned above).
    crate::adapters::tui::tour::observe_submit(state);

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

/// The composer's width-aware projection at the width the layout actually
/// gives it (monitor-narrowed main column, not the terminal width).
fn composer_geometry(state: &TuiState) -> input_box::InputGeometry {
    let width = if state.terminal_width == 0 {
        u16::MAX
    } else {
        crate::adapters::tui::layout::composer_width(state.terminal_width, state.monitor)
    };
    input_box::InputGeometry::new(&state.input_buffer, input_box::inner_width(width))
}

/// Ensure the cursor's visual row is visible within the input box scroll window
/// (`input_scroll_offset` counts visual rows).
fn ensure_cursor_visible(state: &mut TuiState) {
    let geometry = composer_geometry(state);
    let (cursor_row, _) = geometry.cursor(&state.input_buffer, state.cursor_position);
    state.input_scroll_offset = geometry.scroll_to_show(
        cursor_row,
        state.input_scroll_offset,
        input_box::MAX_INPUT_LINES,
    );
}

/// Strip a case-insensitive `a2a/` namespace prefix from an A2A mention
/// filter, preserving the suffix verbatim (peer/skill IDs are case-sensitive).
/// Mirrors the `mcp/` namespace stripping in the filter/backspace paths.
fn strip_a2a_namespace(filter: &str) -> &str {
    match filter.get(..4) {
        Some(prefix) if prefix.eq_ignore_ascii_case("a2a/") => &filter[4..],
        _ => filter,
    }
}

/// Esc on an autocomplete popup: close it and, when a slash popup still owns
/// its provisional edit, restore the exact pre-trigger composer (buffer,
/// cursor, scroll). Host-neutral: the attached host reaches this through
/// `handle_input` like the local one. Never submits or changes focus.
pub(crate) fn cancel_autocomplete_popup(state: &mut TuiState) {
    let rollback = state
        .autocomplete
        .take_slash_rollback(&state.input_buffer, state.cursor_position);
    state.autocomplete.dismiss();
    if let Some(rollback) = rollback {
        state.input_buffer = rollback.buffer;
        state.cursor_position = rollback.cursor;
        state.input_scroll_offset = rollback.scroll;
    }
}

/// Handle keys while autocomplete popup is active.
// Covers: UX-DR75
fn exec_autocomplete(state: &mut TuiState, cmd: OverlayCmd) -> InputAction {
    match cmd {
        // Up/Down navigate the suggestion list
        OverlayCmd::AutoPrev => {
            state.autocomplete.navigate(Direction::Up);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        OverlayCmd::AutoNext => {
            state.autocomplete.navigate(Direction::Down);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        // Tab or Enter selects the current suggestion
        OverlayCmd::AutoAccept => {
            let action = if let Some(suggestion) = state.autocomplete.selected().cloned() {
                apply_autocomplete_selection(state, &suggestion)
            } else {
                None
            };
            state.autocomplete.dismiss();
            state.needs_redraw = true;
            action.unwrap_or(InputAction::Consumed)
        }
        // Esc closes only the popup. A slash popup that still owns its
        // provisional edit also restores the exact pre-trigger composer
        // (Story 21-2 A2/AC4); accepted/authored text is never erased.
        OverlayCmd::AutoCancel => {
            cancel_autocomplete_popup(state);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        // Backspace: if cursor goes back to/before trigger position, dismiss
        OverlayCmd::AutoBackspace => {
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
                    if state.autocomplete.kind == Some(AutocompleteKind::AgentMention)
                        && !filter.starts_with("Agents/")
                    {
                        state.autocomplete.kind = Some(AutocompleteKind::FileMention);
                    }
                    if state.autocomplete.kind == Some(AutocompleteKind::McpMention)
                        && !filter.to_lowercase().starts_with("mcp/")
                    {
                        state.autocomplete.kind = Some(AutocompleteKind::FileMention);
                    }
                    if state.autocomplete.kind == Some(AutocompleteKind::A2aMention)
                        && !filter.to_lowercase().starts_with("a2a/")
                    {
                        state.autocomplete.kind = Some(AutocompleteKind::FileMention);
                    }
                    let kind = state.autocomplete.kind;
                    let filter_text = if kind == Some(AutocompleteKind::AgentMention) {
                        filter
                            .strip_prefix("Agents/")
                            .unwrap_or(&filter)
                            .to_string()
                    } else if kind == Some(AutocompleteKind::McpMention) {
                        filter
                            .strip_prefix("MCP/")
                            .or_else(|| filter.strip_prefix("mcp/"))
                            .unwrap_or(&filter)
                            .to_string()
                    } else if kind == Some(AutocompleteKind::A2aMention) {
                        strip_a2a_namespace(&filter).to_string()
                    } else {
                        filter
                    };
                    state.autocomplete.filter_text = filter_text;
                    state.needs_redraw = true;
                }
                InputAction::Consumed
            }
        }
        OverlayCmd::QuitRequest => InputAction::Quit,
        _ => InputAction::Consumed,
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
                state.resolved_mentions.push(ResolvedMention {
                    path: path.clone(),
                    source: crate::domain::models::FileContextProvenance::UserProvided,
                });
            }
            None
        }
        AutocompleteSuggestion::McpTool { server, name, .. } => {
            // Insert canonical mcp__<server>__<tool> form per DG 2.4
            let canonical = format!("mcp__{}__{}", server, name);
            let before: String = state.input_buffer.chars().take(trigger).collect();
            let after: String = state
                .input_buffer
                .chars()
                .skip(state.cursor_position)
                .collect();
            state.input_buffer = format!("{}{}{}", before, canonical, after);
            state.cursor_position = trigger + canonical.chars().count();
            None
        }
        AutocompleteSuggestion::A2aAgent { peer, name, .. } => {
            // Story 19.13: insert the canonical `a2a__<peer>__<skill>` wire
            // name built through the CapabilityId bridge — never the raw peer
            // skill ID (a hostile peer exposing a skill named like a built-in
            // tool must not become a bare local-tool reference in the prompt).
            let id = crate::domain::models::CapabilityId {
                protocol: "a2a".to_string(),
                server: peer.clone(),
                tool: name.clone(),
            };
            let canonical = id.to_a2a_wire_name()?;
            let before: String = state.input_buffer.chars().take(trigger).collect();
            let after: String = state
                .input_buffer
                .chars()
                .skip(state.cursor_position)
                .collect();
            state.input_buffer = format!("{}{}{}", before, canonical, after);
            state.cursor_position = trigger + canonical.chars().count();
            None
        }
    }
}

/// Edit an append-only text field (A4): the cursor sits at the end, so `Ctrl+W`
/// kills the last word, `Ctrl+U` clears the last line, and `Backspace` / `Ctrl+H`
/// delete the last grapheme. Every deletion is grapheme-safe.
fn edit_field(text: &mut String, key: DomainKey) {
    let end = text.chars().count();
    match key {
        DomainKey::CtrlW => text_edit::kill_word_back(text, end),
        DomainKey::CtrlU => text_edit::kill_to_line_start(text, end),
        _ => text_edit::delete_back(text, end),
    };
}

/// Handle special keys while command palette is active.
// Covers: UX-DR18
fn exec_palette(state: &mut TuiState, cmd: OverlayCmd, host: Host) -> InputAction {
    match cmd {
        OverlayCmd::PaletteUp => {
            state.command_palette.navigate(Direction::Up);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        OverlayCmd::PaletteDown => {
            state.command_palette.navigate(Direction::Down);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        OverlayCmd::PaletteRun => {
            let mut avail = local_avail_ctx(state);
            avail.host = host;
            avail.frame_origin = state.modal_stack.len() > 1;
            if let Some(entry) = state.command_palette.selected()
                && let crate::adapters::tui::keymap::Availability::Unavailable(remedy) =
                    crate::adapters::tui::keymap::table::palette_availability(&entry.action, &avail)
            {
                state.command_palette.refusal = Some(remedy);
                crate::adapters::tui::notifications::info(
                    state,
                    crate::adapters::tui::notifications::NoticeSource::Palette,
                    remedy,
                );
                return InputAction::Consumed;
            }
            let action = state.command_palette.execute_selected();
            if host == Host::Attached {
                presentation::reducer::close(state, Surface::CommandPalette);
            } else {
                match action.as_ref() {
                    Some(PaletteAction::SwitchProfile(_)) => {
                        reducer::handoff(state, Surface::CommandPalette, Surface::ProfileSwitcher);
                    }
                    Some(PaletteAction::ExecuteCommand(name, _)) if name == "models" => {
                        reducer::handoff(state, Surface::CommandPalette, Surface::ModelSelector);
                    }
                    // These handlers validate before deciding whether to open a
                    // sheet, then hand off or close the retained palette themselves.
                    Some(PaletteAction::SwitchModel { .. }) => {}
                    Some(PaletteAction::ExecuteCommand(name, _))
                        if matches!(name.as_str(), "login" | "logout" | "accounts") => {}
                    _ => presentation::reducer::close(state, Surface::CommandPalette),
                }
            }
            if let Some(palette_action) = action {
                return dispatch_palette_action(state, palette_action);
            }
            InputAction::Consumed
        }
        OverlayCmd::PaletteClose => {
            presentation::reducer::close(state, Surface::CommandPalette);
            InputAction::Consumed
        }
        OverlayCmd::QuitRequest => InputAction::Quit,
        _ => InputAction::Consumed,
    }
}

/// Handle special keys while the usage panel overlay is active (Story 7.5 AC3).
/// Esc/Ctrl+C dismisses; Up/Down cycles the visual `selected_section` highlight.
fn exec_usage(state: &mut TuiState, cmd: OverlayCmd) -> InputAction {
    match cmd {
        OverlayCmd::UsageClose => {
            presentation::reducer::close(state, Surface::UsagePanel);
            InputAction::Consumed
        }
        OverlayCmd::UsagePrev => {
            if state.usage_panel.selected_section > 0 {
                state.usage_panel.selected_section -= 1;
            } else {
                state.usage_panel.selected_section = 3;
            }
            state.needs_redraw = true;
            InputAction::Consumed
        }
        OverlayCmd::UsageNext => {
            state.usage_panel.selected_section = (state.usage_panel.selected_section + 1) % 4;
            state.needs_redraw = true;
            InputAction::Consumed
        }
        _ => InputAction::Consumed,
    }
}

/// Dispatch a palette action to the appropriate handler.
pub(crate) fn dispatch_palette_action(
    state: &mut TuiState,
    action: crate::domain::models::palette::PaletteAction,
) -> InputAction {
    match action {
        PaletteAction::ExecuteCommand(name, args) => {
            // Story 6.4: route task-control palette commands to dedicated InputAction variants
            if name == "cancel-plan" {
                InputAction::TaskCancelActivePlan
            } else if name == "resume-all-tasks" {
                InputAction::TaskResumeAll
            } else if name == "reorder-task" {
                reorder_selected_task(state)
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
            reducer::set_base(state, FocusState::Input);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        PaletteAction::SwitchModel {
            provider_id,
            model_id,
        } => InputAction::SwitchModelProvider {
            provider_id: Some(provider_id),
            model_id,
        },
        PaletteAction::SwitchProfile(name) => InputAction::ProfileSwitchRequested(name),
        PaletteAction::OpenSurface(target) => open_surface(state, target),
        PaletteAction::ShowVersion => {
            // Display version info as an Info notice (toast + Notification Log)
            // Covers: FR109
            let version = crate::adapters::tui::version_info::version_string();
            crate::adapters::tui::notifications::info(
                state,
                crate::adapters::tui::notifications::NoticeSource::Help,
                version,
            );
            state.needs_redraw = true;
            InputAction::Consumed
        }
        PaletteAction::NewTab => InputAction::NewTab,
        PaletteAction::CloseTab => InputAction::CloseTab,
        PaletteAction::DeleteAllConversations => InputAction::DeleteAllConversations,
        PaletteAction::PasteImageFromClipboard => InputAction::RequestClipboardPaste,
        PaletteAction::ApplyAdapterOverride { port, adapter } => {
            InputAction::ApplyAdapterOverride { port, adapter }
        }
        PaletteAction::Noop => {
            // Show "Not yet available" feedback
            crate::adapters::tui::notifications::info(
                state,
                crate::adapters::tui::notifications::NoticeSource::Palette,
                "Not yet available",
            );
            state.needs_redraw = true;
            InputAction::Consumed
        }
    }
}

/// Handle keys while reverse search overlay is active.
// Covers: UX-DR74
fn exec_reverse_search_keys(state: &mut TuiState, cmd: OverlayCmd) -> InputAction {
    match cmd {
        OverlayCmd::RevAccept => {
            // Select current match and populate input
            if let Some(selected) = state
                .reverse_search
                .matches
                .get(state.reverse_search.selected_match.unwrap_or(0))
            {
                state.input_buffer = selected.1.clone();
                state.cursor_position = state.input_buffer.chars().count();
            }
            state.reverse_search.dismiss();
            state.needs_redraw = true;
            InputAction::Consumed
        }
        OverlayCmd::RevClose => {
            state.reverse_search.dismiss();
            state.needs_redraw = true;
            InputAction::Consumed
        }
        OverlayCmd::RevOlder => {
            // Cycle to next match
            if !state.reverse_search.matches.is_empty() {
                state.reverse_search.selected_match = Some(
                    (state.reverse_search.selected_match.unwrap_or(0) + 1)
                        % state.reverse_search.matches.len(),
                );
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }
        OverlayCmd::RevNewer => {
            // Cycle to previous match
            if !state.reverse_search.matches.is_empty() {
                let selected = state.reverse_search.selected_match.unwrap_or(0);
                state.reverse_search.selected_match = Some(if selected == 0 {
                    state.reverse_search.matches.len() - 1
                } else {
                    selected - 1
                });
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }
        _ => InputAction::Consumed,
    }
}

/// Update reverse search matches from current query.
fn update_reverse_search_matches(state: &mut TuiState) {
    let old_selected = state.reverse_search.selected_match;
    let results = state
        .input_history
        .search(state.reverse_search.query.as_str());
    state.reverse_search.matches = results
        .into_iter()
        .map(|(i, s)| (i, s.to_string()))
        .collect();
    let new_len = state.reverse_search.matches.len();
    state.reverse_search.selected_match =
        Some(old_selected.unwrap_or(0).min(new_len.saturating_sub(1)));
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
            // Ctrl+E → end of the current line in Input (Story 21.2 AC12; no longer a toggle)
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
            // Ctrl+H → delete the character before the cursor, like Backspace (legacy
            // terminals send 0x08 for Backspace; 21-3 AC11).
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('h') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlH));
            }
            // Ctrl+K → delete to the end of the logical line in the composer (21-3 AC11);
            // a field with no cursor ignores it.
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('k') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlK));
            }
            // Alt+M → toggle multi-line mode (VS Code terminal friendly; `/ml` is the typed route)
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
            // Alt+C / Alt+L / Alt+U → login-overlay copy keys; in every other
            // focus `handle_input` consumes them as no-ops (never the letter).
            if *modifiers == KeyModifiers::ALT {
                match code {
                    KeyCode::Char('c') => {
                        return Some(DomainInputEvent::SpecialKey(DomainKey::AltC));
                    }
                    KeyCode::Char('l') => {
                        return Some(DomainInputEvent::SpecialKey(DomainKey::AltL));
                    }
                    KeyCode::Char('u') => {
                        return Some(DomainInputEvent::SpecialKey(DomainKey::AltU));
                    }
                    _ => {}
                }
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
            // Ctrl+Q → quit from any focus. Story 19.3 (Journey 0).
            // Deliberately NOT CtrlC's CancelOrQuit: Ctrl+C cancels a running
            // stream first; Ctrl+Q means quit.
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('q') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlQ));
            }
            // Ctrl+U → clear search query in Search overlay (Story 4-4, standard readline)
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('u') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlU));
            }
            // Ctrl+A / Ctrl+W: decoded so they never type their letter (Story 21.3 AC12).
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('a') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlA));
            }
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('w') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlW));
            }
            // Every other Ctrl/Alt-modified character is a consumed no-op key, never a
            // bare letter (AC12): Ctrl+J cannot scroll, Ctrl+Y cannot approve a plan. A
            // character held with more than one modifier is its own key (AC20):
            // Ctrl+Shift+C is not Ctrl+C, Alt+Shift+M is not Alt+M.
            if let KeyCode::Char(c) = code {
                let ctrl = modifiers.contains(KeyModifiers::CONTROL);
                let alt = modifiers.contains(KeyModifiers::ALT);
                let shift = modifiers.contains(KeyModifiers::SHIFT);
                let letter = c.to_ascii_lowercase();
                if (ctrl && (alt || shift)) || (alt && shift) {
                    let mask = u8::from(ctrl) | (u8::from(alt) << 1) | (u8::from(shift) << 2);
                    return Some(DomainInputEvent::SpecialKey(DomainKey::UnboundCombo(
                        letter, mask,
                    )));
                }
                if ctrl {
                    return Some(DomainInputEvent::SpecialKey(DomainKey::UnboundCtrl(letter)));
                }
                if alt {
                    return Some(DomainInputEvent::SpecialKey(DomainKey::UnboundAlt(letter)));
                }
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
                KeyCode::PageUp => Some(DomainInputEvent::SpecialKey(DomainKey::PageUp)),
                KeyCode::PageDown => Some(DomainInputEvent::SpecialKey(DomainKey::PageDown)),
                KeyCode::End => Some(DomainInputEvent::SpecialKey(DomainKey::End)),
                KeyCode::Tab => Some(DomainInputEvent::SpecialKey(DomainKey::Tab)),
                KeyCode::BackTab => Some(DomainInputEvent::SpecialKey(DomainKey::ShiftTab)),
                KeyCode::F(n) => Some(DomainInputEvent::SpecialKey(DomainKey::UnboundFn(*n))),
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

    #[test]
    fn story21_5_missing_tool_cannot_lend_search_child() {
        use crate::adapters::tui::tool_inspector::ToolTarget;
        let mut state = TuiState::new(80, 24);
        presentation::reducer::open_tool(
            &mut state,
            ToolTarget {
                conversation_id: "conversation".into(),
                tool_call_id: "call".into(),
            },
            "tool".into(),
            12,
        );
        let Surface::ToolInspector(key) = state.modal_stack.top().unwrap().surface else {
            panic!("real tool frame");
        };
        exec_inspector_search(&mut state, OverlayCmd::InspectorSearch);
        assert!(state.tool_inspectors[&key.0].search.editor.editing);
        state
            .tool_inspectors
            .get_mut(&key.0)
            .unwrap()
            .search
            .editor
            .editing = false;
        state.tool_inspectors.get_mut(&key.0).unwrap().missing = true;
        exec_inspector_search(&mut state, OverlayCmd::InspectorSearch);
        assert!(!state.tool_inspectors[&key.0].search.editor.editing);
    }

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

    fn route_conversation_decision(
        state: &mut TuiState,
        conversation_id: &str,
        event: DomainInputEvent,
    ) -> InputAction {
        let facts = crate::adapters::tui::notification_log::LogFacts {
            active_conversation_id: Some(conversation_id.into()),
            ..Default::default()
        };
        presentation::route(state, &event, Host::Local, &facts)
            .expect("decision frame owns the key")
            .into_action()
    }

    fn story21_6_raw_permission() -> TuiState {
        use crate::adapters::tui::decision_queue::{
            self, PermissionInput, PermissionPayload, Responder,
        };
        use crate::adapters::tui::notifications::NotificationStore;
        use crate::domain::clock::MockClock;
        use crate::domain::models::tool_call::{ApprovalSource, RequestId};
        use std::sync::Arc;
        let mut state = make_state();
        let clock = Arc::new(MockClock::new(std::time::Instant::now()));
        state.notifications = NotificationStore::from_config(
            &crate::domain::models::NotificationsConfig::default(),
            clock.clone(),
        );
        decision_queue::raise_permission(
            &mut state,
            PermissionPayload::new(
                RequestId::new(),
                ApprovalSource::ForegroundTurn {
                    conversation_id: "c1".into(),
                },
                "Bash".into(),
                crate::domain::models::ToolRisk::Elevated,
                PermissionInput::Ready(Arc::new(serde_json::json!({"command": "ok"}))),
            ),
            Responder::Local,
        );
        decision_queue::note_painted(&mut state);
        clock.advance(decision_queue::INERT_AFTER_PAINT);
        state
    }

    fn story21_6_raw_route(
        state: &mut TuiState,
        event: crossterm::event::Event,
    ) -> Option<presentation::RouteEffect> {
        if frame_ignores_raw_key(state, &event) {
            return None;
        }
        let input =
            convert_crossterm_event(&event, &crate::domain::models::MouseConfig::default())?;
        presentation::route(
            state,
            &input,
            crate::adapters::tui::keymap::Host::Local,
            &Default::default(),
        )
    }

    fn story21_6_raw_key(
        code: KeyCode,
        modifiers: KeyModifiers,
        kind: KeyEventKind,
    ) -> crossterm::event::Event {
        crossterm::event::Event::Key(KeyEvent {
            code,
            modifiers,
            kind,
            state: KeyEventState::NONE,
        })
    }

    #[test]
    fn story21_6_raw_new_decision_keys_require_plain_press() {
        use crate::adapters::tui::decision_queue::{self, Arrival, DecisionDraft, DecisionKind};
        for (kind, letters) in [
            (DecisionKind::Forget, "yn"),
            (DecisionKind::Rewind, "yfn"),
            (DecisionKind::ExportOverwrite, "yYnN"),
        ] {
            let mut state = make_state();
            decision_queue::raise(
                &mut state,
                DecisionDraft::new(kind, "raw guard"),
                Arrival::Requested,
            );
            for c in letters.chars() {
                for (modifiers, event_kind) in [
                    (KeyModifiers::NONE, KeyEventKind::Repeat),
                    (KeyModifiers::NONE, KeyEventKind::Release),
                    (KeyModifiers::ALT, KeyEventKind::Press),
                    (KeyModifiers::SHIFT, KeyEventKind::Press),
                    (KeyModifiers::CONTROL, KeyEventKind::Press),
                ] {
                    assert!(
                        frame_ignores_raw_key(
                            &state,
                            &story21_6_raw_key(KeyCode::Char(c), modifiers, event_kind)
                        ),
                        "{kind:?} {c:?} {modifiers:?} {event_kind:?}"
                    );
                }
                assert!(!frame_ignores_raw_key(
                    &state,
                    &story21_6_raw_key(KeyCode::Char(c), KeyModifiers::NONE, KeyEventKind::Press)
                ));
            }
            if kind == DecisionKind::Forget {
                for c in [' ', 'j', 'k'] {
                    assert!(!frame_ignores_raw_key(
                        &state,
                        &story21_6_raw_key(
                            KeyCode::Char(c),
                            KeyModifiers::NONE,
                            KeyEventKind::Repeat
                        )
                    ));
                }
            }
        }
    }

    #[test]
    fn story21_6_raw_permission_only_plain_press_decides() {
        for c in ['y', 's', 'a', 'n', 'f', 'r'] {
            for (modifiers, kind) in [
                (KeyModifiers::NONE, KeyEventKind::Repeat),
                (KeyModifiers::NONE, KeyEventKind::Release),
                (KeyModifiers::ALT, KeyEventKind::Press),
                (KeyModifiers::SHIFT, KeyEventKind::Press),
                (KeyModifiers::CONTROL, KeyEventKind::Press),
            ] {
                let mut state = story21_6_raw_permission();
                assert!(
                    story21_6_raw_route(
                        &mut state,
                        story21_6_raw_key(KeyCode::Char(c), modifiers, kind)
                    )
                    .is_none()
                );
                assert_eq!(state.decisions.len(), 1);
            }
        }
        let mut state = story21_6_raw_permission();
        assert!(matches!(
            story21_6_raw_route(
                &mut state,
                story21_6_raw_key(KeyCode::Char('y'), KeyModifiers::NONE, KeyEventKind::Press)
            ),
            Some(presentation::RouteEffect::Action(
                InputAction::PermissionAllow
            ))
        ));
        assert!(state.decisions.is_empty());
    }

    #[test]
    fn story21_6_raw_feedback_editing_and_global_keys_reach_router() {
        use crate::adapters::tui::decision_queue::DecisionSubject;
        let mut state = story21_6_raw_permission();
        story21_6_raw_route(
            &mut state,
            story21_6_raw_key(KeyCode::Char('f'), KeyModifiers::NONE, KeyEventKind::Press),
        );
        for c in "abc def".chars() {
            story21_6_raw_route(
                &mut state,
                story21_6_raw_key(KeyCode::Char(c), KeyModifiers::NONE, KeyEventKind::Press),
            );
        }
        for c in ['a', 'e', 'k', 'w', 'u', 'h'] {
            let event = ctrl_key(c);
            assert!(!frame_ignores_raw_key(&state, &event), "Ctrl+{c}");
            story21_6_raw_route(&mut state, event);
        }
        story21_6_raw_route(
            &mut state,
            story21_6_raw_key(KeyCode::Char('Z'), KeyModifiers::SHIFT, KeyEventKind::Press),
        );
        let DecisionSubject::Permission(payload) = &state.decisions.items().next().unwrap().subject
        else {
            panic!("permission")
        };
        assert_eq!(payload.feedback.as_ref().unwrap().as_str(), "Z");
        for (modifiers, kind) in [
            (KeyModifiers::NONE, KeyEventKind::Repeat),
            (KeyModifiers::NONE, KeyEventKind::Release),
            (KeyModifiers::SHIFT, KeyEventKind::Press),
        ] {
            assert!(
                story21_6_raw_route(
                    &mut state,
                    story21_6_raw_key(KeyCode::Enter, modifiers, kind)
                )
                .is_none()
            );
            assert_eq!(state.decisions.len(), 1);
        }
        for c in ['x', 'p', 'c', 'q'] {
            assert!(!frame_ignores_raw_key(&state, &ctrl_key(c)), "Ctrl+{c}");
        }
        for c in ['x', 'p'] {
            let mut plain = story21_6_raw_permission();
            assert!(
                story21_6_raw_route(&mut plain, ctrl_key(c)).is_some(),
                "Ctrl+{c} reaches permission routing"
            );
            let mut global = story21_6_raw_permission();
            story21_6_raw_route(
                &mut global,
                story21_6_raw_key(KeyCode::Char('f'), KeyModifiers::NONE, KeyEventKind::Press),
            );
            assert!(
                story21_6_raw_route(&mut global, ctrl_key(c)).is_some(),
                "Ctrl+{c} reaches routing"
            );
            assert_eq!(global.decisions.len(), 1);
        }
        assert!(matches!(
            story21_6_raw_route(
                &mut state,
                story21_6_raw_key(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Press)
            ),
            Some(presentation::RouteEffect::Action(
                InputAction::FeedbackInputSubmit
            ))
        ));
    }

    #[cfg(feature = "anthropic")]
    #[test]
    fn k07_model_selector_enter_keeps_locked_provider_open() {
        use crate::adapters::anthropic::{AnthropicAdapter, AuthMode};
        use crate::adapters::provider::ProviderRegistry;
        use crate::adapters::tui::state::model_selector_columns;
        use crate::domain::models::provider::{
            DiscoveryStatus, LockedProvider, ModelDescriptor, ProviderDescriptor,
        };
        use crate::domain::ports::{ProviderInfoPort, StreamingProvider};
        use std::sync::Arc;

        struct Info(ProviderRegistry);
        impl ProviderInfoPort for Info {
            fn active_delegate_id(&self) -> String {
                "anthropic".into()
            }
            fn get_model(&self, p: &str, m: &str) -> Option<ModelDescriptor> {
                self.0.get_model(p, m)
            }
            fn get_model_provider(&self, m: &str, p: Option<&str>) -> Option<String> {
                self.0.get_model_provider(m, p)
            }
            fn list_providers(&self) -> Vec<ProviderDescriptor> {
                self.0.list_providers()
            }
            fn list_models_by_provider(&self, p: &str) -> Vec<ModelDescriptor> {
                self.0.list_models_by_provider(p)
            }
            fn list_locked_providers(&self) -> Vec<LockedProvider> {
                self.0.list_locked()
            }
            fn discovery_status(&self, p: &str) -> Option<DiscoveryStatus> {
                self.0.discovery_status(p)
            }
            fn get_provider(&self, p: &str) -> Option<Arc<dyn StreamingProvider>> {
                self.0
                    .iter_provider_arcs()
                    .into_iter()
                    .find(|(id, _)| id == p)
                    .map(|(_, provider)| provider)
            }
            fn set_active_provider(
                &self,
                _: &str,
            ) -> Result<(), crate::domain::errors::ProviderError> {
                Ok(())
            }
            fn now_unix(&self) -> i64 {
                0
            }
            fn today_start_unix_ms(&self) -> i64 {
                0
            }
        }

        let info = Info(ProviderRegistry::new());
        info.0.register(Box::new(
            AnthropicAdapter::new(
                AuthMode::ApiKey("picker-test-key".into()),
                "claude-test".into(),
                None,
            )
            .unwrap(),
        ));
        let available = info.0.list_models_by_provider("anthropic")[0].clone();
        let mut public_model = available.clone();
        public_model.provider_id = "locked".into();
        public_model.model_id = "public-catalog-model".into();
        info.0.register_locked(LockedProvider {
            provider_id: "locked".into(),
            display_name: "A Locked Provider".into(),
            sign_in_hint: "Run `rustain auth login locked`".into(),
            models: vec![public_model],
        });
        info.0
            .set_discovery_status("anthropic", DiscoveryStatus::Ok);
        let columns = model_selector_columns(&info);
        assert_eq!(
            columns
                .iter()
                .map(|c| c.provider_id.as_str())
                .collect::<Vec<_>>(),
            ["anthropic", "locked"]
        );
        assert!(columns[0].locked_hint.is_none());
        assert_eq!(columns[0].discovery, Some(DiscoveryStatus::Ok));
        assert!(
            columns[1]
                .locked_hint
                .as_deref()
                .unwrap()
                .contains("rustain auth login locked")
        );

        let mut state = make_state();
        state
            .model_selector
            .initialize(columns, "locked", "public-catalog-model");
        presentation::reducer::open(&mut state, Surface::ModelSelector);
        assert!(state.model_selector.selected().is_none());
        assert_eq!(
            handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter)),
            InputAction::Consumed
        );
        assert!(presentation::is_open(&state, Surface::ModelSelector));
        assert_eq!(state.focus, FocusState::Input);

        state.model_selector.navigate_provider(Direction::Left);
        assert_eq!(
            handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter)),
            InputAction::SwitchModelProvider {
                provider_id: Some("anthropic".into()),
                model_id: available.model_id,
            }
        );

        info.get_provider("anthropic")
            .unwrap()
            .set_discovered_models(Vec::new());
        info.0
            .set_discovery_status("anthropic", DiscoveryStatus::Empty);
        info.0.register_locked(LockedProvider {
            provider_id: "a-locked".into(),
            display_name: "Z Locked Provider".into(),
            sign_in_hint: "Run `rustain auth login a-locked`".into(),
            models: Vec::new(),
        });
        let refreshed = model_selector_columns(&info);
        assert_eq!(
            refreshed
                .iter()
                .map(|c| c.provider_id.as_str())
                .collect::<Vec<_>>(),
            ["anthropic", "locked", "a-locked"]
        );
        assert!(refreshed[0].models.is_empty());
        assert_eq!(refreshed[0].discovery, Some(DiscoveryStatus::Empty));
        state.model_selector.refresh_columns(refreshed);
        assert_eq!(
            state.model_selector.columns[state.model_selector.selected_provider].provider_id,
            "anthropic"
        );
        assert!(state.model_selector.selected().is_none());
        assert_eq!(
            handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter)),
            InputAction::Consumed
        );
        assert!(presentation::is_open(&state, Surface::ModelSelector));

        // A locked column advertises only ←→ and Esc: '/' starts no hidden
        // search, so Esc closes the selector instead of clearing invisible input.
        state.model_selector.navigate_provider(Direction::Right);
        assert!(
            state.model_selector.columns[state.model_selector.selected_provider]
                .locked_hint
                .is_some()
        );
        for c in ['/', 'x'] {
            assert_eq!(
                handle_input(&mut state, &DomainInputEvent::KeyPress(c)),
                InputAction::Consumed
            );
        }
        assert!(!state.model_selector.search_active);
        let _ = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
        assert!(!presentation::is_open(&state, Surface::ModelSelector));
    }

    #[derive(Debug)]
    struct EmptyWaveHandle;

    impl crate::domain::ports::wave_handle::WaveHandle for EmptyWaveHandle {
        fn snapshot(&self) -> crate::domain::ports::wave_handle::WaveSnapshot {
            crate::domain::ports::wave_handle::WaveSnapshot {
                spoke_count: 0,
                completed: 0,
                honest_empty: Some(true),
                cancelled: false,
                outcome: crate::domain::models::orchestration::ForkJoinOutcome {
                    spokes: Vec::new(),
                    synthesis: crate::domain::models::orchestration::SynthesisView::default(),
                },
                resolve_count: 0,
                slots: Vec::new(),
                rerun_counts: Vec::new(),
            }
        }

        fn cancel(&self) {}

        fn drill(&self, _slot: usize) -> Option<crate::domain::ports::wave_handle::DrillBody> {
            None
        }

        fn drill_id(&self, _slot: usize) -> Option<crate::domain::models::orchestration::DrillId> {
            None
        }

        fn slots(&self) -> Vec<crate::domain::models::AgentId> {
            Vec::new()
        }

        fn rerun_count_for_slot(&self, _slot: usize) -> u8 {
            0
        }

        fn resolve_count(&self) -> usize {
            0
        }
    }

    /// 14.3c (AI-12.3): a RECORDING `WaveHandle` for the AC8/AC11 keystones —
    /// reports a configurable `completed` and records whether `cancel()` fired.
    /// Drives the push-vs-pull differential + the shared-token-hazard guard.
    #[derive(Debug)]
    struct SpyWaveHandle {
        completed: usize,
        cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl SpyWaveHandle {
        fn new(completed: usize) -> Self {
            Self {
                completed,
                cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }
        }
        /// Construct sharing an existing cancel flag — models the production CoW
        /// rerun where the prior + new handle SHARE one wave_cancel token.
        fn with_flag(
            completed: usize,
            cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
        ) -> Self {
            Self {
                completed,
                cancelled,
            }
        }
    }

    impl crate::domain::ports::wave_handle::WaveHandle for SpyWaveHandle {
        fn snapshot(&self) -> crate::domain::ports::wave_handle::WaveSnapshot {
            crate::domain::ports::wave_handle::WaveSnapshot {
                spoke_count: self.completed,
                completed: self.completed,
                honest_empty: Some(false),
                cancelled: self.cancelled.load(std::sync::atomic::Ordering::SeqCst),
                outcome: crate::domain::models::orchestration::ForkJoinOutcome {
                    spokes: Vec::new(),
                    synthesis: crate::domain::models::orchestration::SynthesisView::default(),
                },
                resolve_count: 0,
                slots: Vec::new(),
                rerun_counts: Vec::new(),
            }
        }
        fn cancel(&self) {
            self.cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        fn drill(&self, _slot: usize) -> Option<crate::domain::ports::wave_handle::DrillBody> {
            None
        }
        fn drill_id(&self, _slot: usize) -> Option<crate::domain::models::orchestration::DrillId> {
            None
        }
        fn slots(&self) -> Vec<crate::domain::models::AgentId> {
            Vec::new()
        }
        fn rerun_count_for_slot(&self, _slot: usize) -> u8 {
            0
        }
        fn resolve_count(&self) -> usize {
            0
        }
    }

    fn state_with_wave() -> TuiState {
        let mut state = make_state();
        state.focus = FocusState::Chat;
        state.wave_run = Some(std::sync::Arc::new(EmptyWaveHandle));
        state
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

    // ── Story 19.3: Ctrl+Q quits from any focus (Journey 0) ────────────────

    /// AC1 — Ctrl+Q translates at the crossterm boundary (the ONLY place
    /// crossterm types are mapped, FR16).
    #[test]
    fn ctrl_q_maps_to_ctrl_q_domain_key() {
        let event = ctrl_key('q');
        assert!(matches!(
            convert_crossterm_event(&event, &crate::domain::models::MouseConfig::default()),
            Some(DomainInputEvent::SpecialKey(DomainKey::CtrlQ))
        ));
    }

    /// AC1 — full front-door chain from the real crossterm event: Chat focus
    /// → Quit. Drives convert_crossterm_event → handle_input; never a
    /// hand-fabricated DomainInputEvent.
    #[test]
    fn ctrl_q_event_quits_from_chat_focus() {
        let mut state = make_state();
        state.focus = FocusState::Chat;
        let evt = ctrl_key('q');
        let converted =
            convert_crossterm_event(&evt, &crate::domain::models::MouseConfig::default())
                .expect("Ctrl+Q must produce a domain event");
        assert_eq!(handle_input(&mut state, &converted), InputAction::Quit);
    }

    /// AC1 — Input focus WITH TEXT TYPED still quits (Sam is mid-thought in
    /// the input box; the PRD's Journey 0 key must not insert a character).
    /// This is the AC1 mutant's tripwire: without the crossterm Ctrl+Q arm
    /// the event degrades to KeyPress('q') and inserts instead of quitting.
    #[test]
    fn ctrl_q_event_quits_from_input_focus_with_text_typed() {
        let mut state = make_state();
        state.focus = FocusState::Input;
        state.input_buffer = "half-written thought".to_string();
        state.cursor_position = state.input_buffer.chars().count();
        let evt = ctrl_key('q');
        let converted =
            convert_crossterm_event(&evt, &crate::domain::models::MouseConfig::default())
                .expect("Ctrl+Q must produce a domain event");
        assert_eq!(handle_input(&mut state, &converted), InputAction::Quit);
        assert_eq!(
            state.input_buffer, "half-written thought",
            "Ctrl+Q must not insert a character"
        );
    }

    /// AC1 positive control — plain `q` in Chat focus still quits (app.rs:1227).
    #[test]
    fn plain_q_in_chat_focus_still_quits() {
        let mut state = make_state();
        state.focus = FocusState::Chat;
        assert_eq!(
            handle_input(&mut state, &DomainInputEvent::KeyPress('q')),
            InputAction::Quit
        );
    }
    #[test]
    fn story19_19_review_consolidation_keys_belong_to_confirmation() {
        use crate::adapters::tui::state::{ThreadAccess, ThreadInspectorState, ThreadTarget};
        for (key, consolidation_action, forget_action) in [
            (
                'y',
                InputAction::ConsolidateAcceptAll,
                InputAction::ForgetAcceptAll,
            ),
            (
                'n',
                InputAction::ConsolidateDeclineAll,
                InputAction::ForgetDeclineAll,
            ),
        ] {
            let clock = std::sync::Arc::new(crate::domain::clock::MockClock::new(
                std::time::Instant::now(),
            ));
            let mut state = make_state();
            state.notifications =
                crate::adapters::tui::notifications::NotificationStore::from_config(
                    &crate::domain::models::NotificationsConfig::default(),
                    clock.clone(),
                );
            state.focus = FocusState::Chat;
            let address = crate::domain::models::ItemAddress::from_a2a_ingress(
                crate::domain::models::PeerId::from_public_key(&[91; 32]).unwrap(),
                crate::domain::models::ItemId::from_replay("review-opened"),
            );
            let mut inspector = ThreadInspectorState::new(
                ThreadTarget::Item(address.clone()),
                ThreadAccess::ReadOnly,
                crate::domain::models::SemanticMessageType::Unknown,
            );
            inspector.root_id = Some("review-root".into());
            inspector.first_non_pinned_ordinal = Some(3);
            inspector.last_non_pinned_ordinal = Some(7);
            inspector.omitted_before = 3;
            inspector.item_state = Some("received");
            inspector.viewer.top = 11;
            inspector.fit.set((60, 20));
            inspector.content = "reader body".into();
            presentation::reducer::open_thread(
                &mut state,
                inspector,
                crate::adapters::tui::thread_inspector::title(
                    crate::domain::models::SemanticMessageType::Unknown,
                ),
            );
            assert_eq!(
                state.modal_stack.top().unwrap().surface,
                Surface::ThreadInspector
            );
            use crate::adapters::tui::decision_queue::{
                self, PendingConsolidationCard, PendingForgetCard,
            };
            decision_queue::raise_consolidation(
                &mut state,
                PendingConsolidationCard {
                    conversation_id: "review-conversation".into(),
                    proposals: Vec::new(),
                },
            );
            assert!(decision_queue::consolidation_pending(
                &state,
                "review-conversation"
            ));
            assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Decision);
            let modified =
                story21_6_raw_key(KeyCode::Char(key), KeyModifiers::ALT, KeyEventKind::Press);
            assert!(frame_ignores_raw_key(&state, &modified));
            decision_queue::note_painted(&mut state);
            clock.advance(decision_queue::INERT_AFTER_PAINT);
            assert_eq!(
                route_conversation_decision(
                    &mut state,
                    "review-conversation",
                    DomainInputEvent::KeyPress(key)
                ),
                consolidation_action
            );
            assert!(!decision_queue::consolidation_pending(
                &state,
                "review-conversation"
            ));
            drop(state.decisions.take_resolved());
            let inspector = state.thread_inspector.as_ref().unwrap();
            let actual = inspector.item_address().unwrap();
            assert_eq!(
                actual, &address,
                "decision changed the pinned reader target"
            );
            assert_eq!(inspector.viewer.top, 11);
            // The frame never owned base focus, so the decision leaves it alone.
            assert_eq!(state.focus, FocusState::Chat);
            assert_eq!(
                state.modal_stack.top().unwrap().surface,
                Surface::ThreadInspector
            );
            decision_queue::raise_forget(
                &mut state,
                PendingForgetCard {
                    conversation_id: "review".into(),
                    candidates: Vec::new(),
                    focused_index: 0,
                },
            );
            assert!(decision_queue::forget_pending(&state, "review"));
            assert!(state.thread_inspector.is_some());
            // Story 21.8: the presenting sweep survives as `higher_confirmation_presenting`.
            assert!(decision_queue::higher_confirmation_presenting(&state));
            assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Decision);
            decision_queue::note_painted(&mut state);
            clock.advance(decision_queue::INERT_AFTER_PAINT);
            assert_eq!(
                route_conversation_decision(&mut state, "review", DomainInputEvent::KeyPress(key)),
                forget_action
            );
            assert!(!decision_queue::forget_pending(&state, "review"));
            assert_eq!(state.thread_inspector.as_ref().unwrap().viewer.top, 11);
        }
    }

    fn story21_8_base_focus(input: bool) -> FocusState {
        if input {
            FocusState::Input
        } else {
            FocusState::Chat
        }
    }

    /// A local-host state whose only frame is a real S-THREAD window (opened by the reducer).
    fn story21_8_thread_state(focus: FocusState) -> TuiState {
        use crate::adapters::tui::state::{ThreadAccess, ThreadInspectorState, ThreadTarget};
        let mut state = make_state();
        state.focus = focus;
        let address = crate::domain::models::ItemAddress::from_a2a_ingress(
            crate::domain::models::PeerId::from_public_key(&[44; 32]).unwrap(),
            crate::domain::models::ItemId::from_replay("thread-front-door"),
        );
        let inspector = ThreadInspectorState::new(
            ThreadTarget::Item(address),
            ThreadAccess::LocalOnly,
            crate::domain::models::SemanticMessageType::Unknown,
        );
        // What the last paint fitted: 100 body rows in a 10-row viewport.
        inspector.fit.set((100, 10));
        presentation::reducer::open_thread(
            &mut state,
            inspector,
            crate::adapters::tui::thread_inspector::title(
                crate::domain::models::SemanticMessageType::Unknown,
            ),
        );
        assert_eq!(
            state.modal_stack.top().unwrap().surface,
            Surface::ThreadInspector
        );
        state
    }

    /// The local front door: the raw pre-filter, then the converted event.
    fn story21_8_front_door(
        state: &mut TuiState,
        event: crossterm::event::Event,
    ) -> Option<InputAction> {
        let domain =
            convert_crossterm_event(&event, &crate::domain::models::MouseConfig::default())
                .expect("a key converts");
        handle_operator_input(state, &event, &domain)
    }

    fn story21_8_plain(c: char) -> crossterm::event::Event {
        story21_6_raw_key(KeyCode::Char(c), KeyModifiers::NONE, KeyEventKind::Press)
    }

    fn story21_8_special(code: KeyCode) -> crossterm::event::Event {
        story21_6_raw_key(code, KeyModifiers::NONE, KeyEventKind::Press)
    }

    fn story21_8_top(state: &TuiState) -> usize {
        state.thread_inspector.as_ref().unwrap().viewer.top
    }

    /// AC4/K: `Ctrl+X` opens the leader over the thread, `n` opens S-LOG above it, and `Esc`
    /// returns to the thread window with its slot and viewer intact.
    #[test]
    fn story21_8_local_leader_opens_log_over_thread_and_esc_returns() {
        let mut state = story21_8_thread_state(FocusState::Input);
        state.thread_inspector.as_mut().unwrap().viewer.top = 7;
        assert!(state.pending_leader.is_none());
        assert_eq!(
            story21_8_front_door(&mut state, ctrl_key('x')),
            Some(InputAction::Consumed)
        );
        assert!(state.pending_leader.is_some(), "Ctrl+X opens the leader");
        assert_eq!(
            state.modal_stack.top().unwrap().surface,
            Surface::ThreadInspector,
            "the leader is a pending prefix, not a frame"
        );
        assert_eq!(
            story21_8_front_door(&mut state, story21_8_plain('n')),
            Some(InputAction::Consumed)
        );
        assert!(state.pending_leader.is_none());
        assert_eq!(
            state.modal_stack.top().unwrap().surface,
            Surface::NotificationLog
        );
        assert!(state.thread_inspector.is_some(), "the thread stays beneath");
        story21_8_front_door(&mut state, story21_8_special(KeyCode::Esc));
        assert_eq!(
            state.modal_stack.top().unwrap().surface,
            Surface::ThreadInspector
        );
        assert_eq!(story21_8_top(&state), 7);
        assert_eq!(state.focus, FocusState::Input);
    }

    /// AC4: `Ctrl+P` opens the palette over the thread; `Ctrl+Q` raises the quit request.
    #[test]
    fn story21_8_local_palette_and_quit_work_over_thread() {
        let mut state = story21_8_thread_state(FocusState::Input);
        assert_eq!(
            story21_8_front_door(&mut state, ctrl_key('p')),
            Some(InputAction::Consumed)
        );
        assert_eq!(
            state.modal_stack.top().unwrap().surface,
            Surface::CommandPalette
        );
        assert!(state.thread_inspector.is_some());

        let mut state = story21_8_thread_state(FocusState::Chat);
        assert_eq!(
            story21_8_front_door(&mut state, ctrl_key('q')),
            Some(InputAction::Quit)
        );
        assert!(
            state.thread_inspector.is_some(),
            "the quit request keeps the window until the guard answers"
        );
        assert_eq!(
            state.modal_stack.top().unwrap().surface,
            Surface::ThreadInspector
        );
    }

    /// AC5: the body scroll keys move `viewer.top` by value; the sideways/toggle keys are
    /// consumed no-ops at the root of a thread.
    #[test]
    fn story21_8_local_scroll_keys_change_viewer_top_by_value() {
        let mut state = story21_8_thread_state(FocusState::Input);
        // Rows 100, viewport 10: the clamp is 90, half a page is 5, a page is 9.
        let table: [(crossterm::event::Event, usize, &str); 10] = [
            (story21_8_plain('j'), 21, "j"),
            (story21_8_plain('k'), 20, "k"),
            (story21_8_special(KeyCode::Down), 21, "Down"),
            (story21_8_special(KeyCode::Up), 20, "Up"),
            (ctrl_key('d'), 25, "Ctrl+D"),
            (ctrl_key('u'), 20, "Ctrl+U"),
            (story21_8_special(KeyCode::PageDown), 29, "PageDown"),
            (story21_8_special(KeyCode::PageUp), 20, "PageUp"),
            (story21_8_plain('g'), 0, "g"),
            (
                story21_6_raw_key(KeyCode::Char('G'), KeyModifiers::SHIFT, KeyEventKind::Press),
                90,
                "G",
            ),
        ];
        state.thread_inspector.as_mut().unwrap().viewer.top = 20;
        for (event, expected, name) in table {
            assert_eq!(
                story21_8_front_door(&mut state, event),
                Some(InputAction::Consumed),
                "{name} is consumed"
            );
            assert_eq!(story21_8_top(&state), expected, "{name}");
        }
        assert!(
            state.input_buffer.is_empty(),
            "no scroll key reaches the composer"
        );

        // `G` followed the tail: start the dead-key sweep from a fresh viewer.
        state.thread_inspector.as_mut().unwrap().viewer = Default::default();
        state.thread_inspector.as_mut().unwrap().viewer.top = 20;
        for (event, name) in [
            (story21_8_plain('h'), "h"),
            (story21_8_plain('0'), "0"),
            (
                story21_6_raw_key(KeyCode::Char('$'), KeyModifiers::SHIFT, KeyEventKind::Press),
                "$",
            ),
            (story21_8_plain('v'), "v"),
        ] {
            assert_eq!(
                story21_8_front_door(&mut state, event),
                Some(InputAction::Consumed),
                "{name} is consumed"
            );
            let inspector = state.thread_inspector.as_ref().unwrap();
            assert_eq!(inspector.viewer.top, 20, "{name} does not scroll");
            assert_eq!(inspector.viewer.h_off, 0, "{name} does not pan");
            assert!(!inspector.viewer.follow, "{name} does not follow");
        }
        assert_eq!(
            state.modal_stack.top().unwrap().surface,
            Surface::ThreadInspector
        );
    }

    /// AC5: `p`/`l` ask for a page only while a page is omitted on that side.
    #[test]
    fn story21_8_local_paging_keys_request_only_an_omitted_page() {
        let mut state = story21_8_thread_state(FocusState::Input);
        for c in ['l', 'p'] {
            assert_eq!(
                story21_8_front_door(&mut state, story21_8_plain(c)),
                Some(InputAction::Consumed),
                "{c} with nothing omitted"
            );
        }
        state.thread_inspector.as_mut().unwrap().omitted_after = 2;
        assert_eq!(
            story21_8_front_door(&mut state, story21_8_plain('l')),
            Some(InputAction::ThreadPage { earlier: false })
        );
        assert_eq!(
            story21_8_front_door(&mut state, story21_8_plain('p')),
            Some(InputAction::Consumed),
            "nothing omitted before"
        );
        let inspector = state.thread_inspector.as_mut().unwrap();
        inspector.omitted_after = 0;
        inspector.omitted_before = 1;
        assert_eq!(
            story21_8_front_door(&mut state, story21_8_plain('p')),
            Some(InputAction::ThreadPage { earlier: true })
        );
        assert_eq!(
            story21_8_front_door(&mut state, story21_8_plain('l')),
            Some(InputAction::Consumed),
            "nothing omitted after"
        );
    }

    /// PC-14: a Repeat, a Release or a modified writer letter never reaches the window's
    /// dispatcher; a registered chord or shifted printable resolves; unbound keys are consumed
    /// and never typed into the composer.
    #[test]
    fn story21_8_local_press_only_rule_guards_the_writer_letters() {
        let mut state = story21_8_thread_state(FocusState::Input);
        for c in "rdbaysn123u".chars() {
            for (modifiers, kind) in [
                (KeyModifiers::NONE, KeyEventKind::Repeat),
                (KeyModifiers::NONE, KeyEventKind::Release),
                (KeyModifiers::ALT, KeyEventKind::Press),
            ] {
                let event = story21_6_raw_key(KeyCode::Char(c), modifiers, kind);
                assert!(
                    frame_ignores_raw_key(&state, &event),
                    "{c:?} {modifiers:?} {kind:?}"
                );
                assert_eq!(story21_8_front_door(&mut state, event), None);
            }
            let plain = story21_8_plain(c);
            assert!(!frame_ignores_raw_key(&state, &plain), "plain press {c:?}");
            assert_eq!(
                story21_8_front_door(&mut state, plain),
                Some(InputAction::Consumed),
                "a plain writer letter is consumed by the local window"
            );
        }
        let shifted =
            story21_6_raw_key(KeyCode::Char('R'), KeyModifiers::SHIFT, KeyEventKind::Press);
        assert!(frame_ignores_raw_key(&state, &shifted));
        // Registered chords and shifted printables are not writer-letter guards.
        for event in [
            ctrl_key('d'),
            ctrl_key('u'),
            ctrl_key('c'),
            ctrl_key('x'),
            ctrl_key('p'),
            ctrl_key('q'),
            story21_6_raw_key(KeyCode::Char('G'), KeyModifiers::SHIFT, KeyEventKind::Press),
            story21_6_raw_key(KeyCode::Char('$'), KeyModifiers::SHIFT, KeyEventKind::Press),
        ] {
            assert!(!frame_ignores_raw_key(&state, &event), "{event:?}");
        }
        // A released Ctrl+D is still a Release: ignored; the Press resolves and scrolls.
        state.thread_inspector.as_mut().unwrap().viewer.top = 20;
        let released = story21_6_raw_key(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
            KeyEventKind::Release,
        );
        assert!(frame_ignores_raw_key(&state, &released));
        assert_eq!(story21_8_front_door(&mut state, released), None);
        assert_eq!(story21_8_top(&state), 20);
        assert_eq!(
            story21_8_front_door(&mut state, ctrl_key('d')),
            Some(InputAction::Consumed)
        );
        assert_eq!(story21_8_top(&state), 25);
        // Unbound keys: consumed, never the composer.
        state.input_buffer = "draft".into();
        for c in ['x', 'z', 'w'] {
            assert_eq!(
                story21_8_front_door(&mut state, story21_8_plain(c)),
                Some(InputAction::Consumed),
                "{c:?}"
            );
        }
        assert_eq!(state.input_buffer, "draft");
        assert_eq!(
            state.modal_stack.top().unwrap().surface,
            Surface::ThreadInspector
        );
    }

    /// AC4: `Esc` and `Ctrl+C` back exactly one frame and never change the base focus.
    #[test]
    fn story21_8_local_back_leaves_base_focus_unchanged() {
        for input in [true, false] {
            for event in [story21_8_special(KeyCode::Esc), ctrl_key('c')] {
                let mut state = story21_8_thread_state(story21_8_base_focus(input));
                assert_eq!(state.focus, story21_8_base_focus(input));
                state.input_buffer = "kept".into();
                assert_eq!(
                    story21_8_front_door(&mut state, event.clone()),
                    Some(InputAction::Consumed)
                );
                assert!(state.modal_stack.is_empty(), "{event:?} closes the window");
                assert!(state.thread_inspector.is_none(), "{event:?} frees the slot");
                assert_eq!(
                    state.focus,
                    story21_8_base_focus(input),
                    "{event:?} leaves the base focus"
                );
                assert_eq!(state.input_buffer, "kept");
            }
        }
    }

    /// AC2 expectation: the row either yields Quit or is a documented Skip
    /// whose reason the story's Completion Notes repeat verbatim (A4).
    #[derive(Debug)]
    enum CtrlQRowExpectation {
        Quit,
        Skip(&'static str),
    }

    /// AC2 — for every FocusState reachable in a headless test, Ctrl+Q either
    /// yields Quit or the row is an explicit Skip("reason") documented in the
    /// story's Completion Notes. Rows use the actual surface lifecycle:
    /// frames, pending leader and routed popups retain their production
    /// ownership. Autocomplete lives at Input, never in a focus overlay.
    #[test]
    fn ctrl_q_from_every_focus_state_either_quits_or_is_documented() {
        struct Row {
            label: &'static str,
            setup: Box<dyn Fn(&mut TuiState)>,
            expect: CtrlQRowExpectation,
        }
        fn confirmation(
            kind: crate::adapters::tui::decision_queue::DecisionKind,
        ) -> Box<dyn Fn(&mut TuiState)> {
            Box::new(move |s| {
                use crate::adapters::tui::decision_queue::{self, Arrival, DecisionDraft};
                decision_queue::raise(s, DecisionDraft::new(kind, "ctrl-q"), Arrival::Requested);
            })
        }

        let rows = vec![
            // ── Non-modal surfaces: the Journey 0 named surfaces quit ──
            Row {
                label: "Input (empty buffer)",
                setup: Box::new(|s| s.focus = FocusState::Input),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "Input (text typed)",
                setup: Box::new(|s| {
                    s.focus = FocusState::Input;
                    s.input_buffer = "draft".to_string();
                    s.cursor_position = 5;
                }),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "Chat",
                setup: Box::new(|s| s.focus = FocusState::Chat),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "History window",
                setup: Box::new(|s| {
                    crate::adapters::tui::history_window::open(
                        s,
                        &crate::domain::services::session_index::SessionIndex::new(),
                        0,
                    );
                }),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "Input with autocomplete popup active (production shape)",
                setup: Box::new(|s| {
                    s.focus = FocusState::Input;
                    handle_input(s, &DomainInputEvent::KeyPress('/'));
                }),
                expect: CtrlQRowExpectation::Quit,
            },
            // ── Cards whose special-key handling has NO interceptor: Ctrl+Q
            // reaches the global site and quits. Their y/n/e verbs are char
            // keys; quitting abandons the card without performing it.
            Row {
                label: "Confirmation(ArtifactApply) (no special-key interceptor)",
                setup: confirmation(
                    crate::adapters::tui::decision_queue::DecisionKind::ArtifactApply,
                ),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "Confirmation(PeerAdd) (no special-key interceptor)",
                setup: confirmation(crate::adapters::tui::decision_queue::DecisionKind::PeerAdd),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "Confirmation(TeamRetract) (Esc-only interceptor)",
                setup: confirmation(
                    crate::adapters::tui::decision_queue::DecisionKind::TeamRetract,
                ),
                expect: CtrlQRowExpectation::Quit,
            },
            // ── Overlays and cards that own the keyboard and consume the key (the
            // Skip reasons below), plus the surfaces whose own row quits.
            Row {
                label: "Overlay(CommandPalette) with palette open",
                setup: Box::new(|s| {
                    presentation::reducer::open(s, Surface::CommandPalette);
                }),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "Overlay(WhichKey) with which-key open",
                setup: Box::new(|s| {
                    open_leader(s);
                }),
                // A18: the leader stays intact beneath the guarded quit frame.
                expect: CtrlQRowExpectation::Quit,
            },
            // Help was a documented Skip until the code review (D2): the overlay
            // RENDERS the `Ctrl+Q — Quit (any focus)` binding, so swallowing the
            // key made the help text lie about itself. `handle_help_overlay_key`
            // now passes Ctrl+Q through exactly as it already passed Ctrl+C.
            Row {
                label: "Overlay(Help) (passes Ctrl+Q through, like Ctrl+C)",
                setup: Box::new(|s| {
                    presentation::reducer::open(s, Surface::Help);
                }),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "ModelSelector frame",
                setup: Box::new(|s| {
                    presentation::reducer::open(s, Surface::ModelSelector);
                }),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "Login frame (Ctrl+Q preserves the login beneath Quit)",
                setup: Box::new(|s| {
                    presentation::reducer::open(s, Surface::Login);
                }),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "ProfileSwitcher frame",
                setup: Box::new(|s| {
                    presentation::reducer::open(s, Surface::ProfileSwitcher);
                }),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "Input reverse search popup",
                setup: Box::new(|s| {
                    s.reverse_search.reset();
                    s.focus = FocusState::Input;
                }),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "Search frame",
                setup: Box::new(|s| {
                    presentation::reducer::open(s, Surface::Search);
                }),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "CrossSearch frame",
                setup: Box::new(|s| {
                    // CrossSearch opens above the History window it is searched from.
                    crate::adapters::tui::history_window::open(
                        s,
                        &crate::domain::services::session_index::SessionIndex::new(),
                        0,
                    );
                    presentation::reducer::open(s, Surface::CrossSearch);
                    assert_eq!(s.modal_stack.top().unwrap().surface, Surface::CrossSearch);
                }),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "BookmarkList frame",
                setup: Box::new(|s| {
                    presentation::reducer::open(s, Surface::BookmarkList);
                }),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "WaveOverlay frame",
                setup: Box::new(|s| {
                    presentation::reducer::open(s, Surface::WaveOverlay);
                }),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "UsagePanel frame",
                setup: Box::new(|s| {
                    presentation::reducer::open(s, Surface::UsagePanel);
                }),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "Mission Control Room tab",
                setup: Box::new(|s| {
                    presentation::reducer::open_mission_control(
                        s,
                        crate::adapters::tui::mission_control::McTab::Room,
                    );
                }),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "Mission Control Agents tab",
                setup: Box::new(|s| {
                    presentation::reducer::open_mission_control(
                        s,
                        crate::adapters::tui::mission_control::McTab::Agents,
                    );
                }),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "Mission Control Tasks tab (replaces the retired Sidebar Tasks row)",
                setup: Box::new(|s| {
                    presentation::reducer::open_mission_control(
                        s,
                        crate::adapters::tui::mission_control::McTab::Tasks,
                    );
                }),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "Task window (replaces the retired Chat task drill-down)",
                setup: Box::new(|s| {
                    use crate::domain::models::plan::{Plan, PlanStatus, PlanTask, PlanTaskStatus};
                    let task = PlanTask {
                        number: 1,
                        title: "Compile".to_owned(),
                        description: String::new(),
                        depends_on: vec![],
                        status: PlanTaskStatus::Failed,
                        started_at_ms: None,
                        completed_at_ms: None,
                        result: None,
                        error: Some("boom".to_owned()),
                        waiting_on: vec![],
                        delegated_to: None,
                        sub_tasks: vec![],
                    };
                    let plan = Plan {
                        id: "p1".to_owned(),
                        title: "Ship".to_owned(),
                        tasks: vec![task],
                        estimated_effort: None,
                        status: PlanStatus::Executing,
                        created_at: 1,
                        resolved_at: None,
                        host_message_id: None,
                        origin_restrictions: None,
                    };
                    let conversation = crate::domain::models::Conversation {
                        id: "conv-1".to_owned(),
                        plans: std::collections::HashMap::from([("p1".to_owned(), plan)]),
                        ..Default::default()
                    };
                    let task_ref = crate::adapters::tui::mission_control::tasks::TaskRef {
                        conversation_id: "conv-1".to_owned(),
                        plan_id: "p1".to_owned(),
                        number: 1,
                    };
                    assert!(crate::adapters::tui::task_window::open(
                        s,
                        &conversation,
                        &task_ref
                    ));
                }),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "Agent window",
                setup: Box::new(|s| {
                    crate::adapters::tui::agent_window::open(
                        s,
                        crate::domain::models::AgentId::new(),
                    );
                }),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "Adapters window",
                setup: Box::new(|s| {
                    crate::adapters::tui::adapters_window::open(s, Vec::new());
                }),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "Fork decision",
                setup: confirmation(crate::adapters::tui::decision_queue::DecisionKind::Fork),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "Rewind decision",
                setup: confirmation(crate::adapters::tui::decision_queue::DecisionKind::Rewind),
                expect: CtrlQRowExpectation::Quit,
            },
            Row {
                label: "Export overwrite decision",
                setup: confirmation(
                    crate::adapters::tui::decision_queue::DecisionKind::ExportOverwrite,
                ),
                expect: CtrlQRowExpectation::Quit,
            },
        ];

        for row in rows {
            let mut state = make_state();
            (row.setup)(&mut state);
            let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlQ));
            match row.expect {
                CtrlQRowExpectation::Quit => {
                    assert_eq!(action, InputAction::Quit, "row {}", row.label);
                }
                CtrlQRowExpectation::Skip(reason) => {
                    assert_ne!(
                        action,
                        InputAction::Quit,
                        "row {} documented Skip({reason:?}) but it quits — update the story docs",
                        row.label
                    );
                }
            }
        }
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
    fn ctrl_f_in_input_opens_search_frame_in_typing_substate() {
        let mut state = make_state();
        open_search(&mut state);
        assert_eq!(state.focus, FocusState::Input);
        assert!(presentation::is_open(&state, Surface::Search));
        assert_eq!(state.search_state.substate, SearchSubstate::Typing);
        assert_eq!(state.search_state.query.as_str(), "");
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
        assert_eq!(state.focus, FocusState::Input);
        assert!(presentation::is_open(&state, Surface::Search));
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
        assert_eq!(state.focus, FocusState::Input);
        assert!(!presentation::is_open(&state, Surface::Search));
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
        assert_eq!(state.search_state.query.as_str(), "nginx");
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
        state.search_state.query = "foo".into();
        state.search_state.substate = SearchSubstate::Navigating;
        // Typing 'x' while in Navigating should return to Typing AND append 'x'
        // so the user can refine without pressing Esc.
        let action = handle_input(&mut state, &DomainInputEvent::KeyPress('x'));
        assert_eq!(action, InputAction::SearchReturnToTyping);
        assert_eq!(state.search_state.query.as_str(), "foox");
        assert_eq!(state.search_state.substate, SearchSubstate::Typing);
    }

    #[test]
    fn ctrl_u_clears_query_and_stays_in_typing() {
        let mut state = make_state();
        open_search(&mut state);
        state.search_state.query = "something".into();
        state.search_state.substate = SearchSubstate::Navigating;
        let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlU));
        assert_eq!(action, InputAction::SearchReturnToTyping);
        assert_eq!(state.search_state.query.as_str(), "");
        assert_eq!(state.search_state.substate, SearchSubstate::Typing);
    }

    #[test]
    fn backspace_in_typing_pops_query_and_requests_rescan() {
        let mut state = make_state();
        open_search(&mut state);
        state.search_state.query = "hello".into();
        let action = handle_input(
            &mut state,
            &DomainInputEvent::SpecialKey(DomainKey::Backspace),
        );
        assert_eq!(action, InputAction::SearchQueryChanged);
        assert_eq!(state.search_state.query.as_str(), "hell");
    }

    #[test]
    fn backspace_in_navigating_returns_to_typing_and_pops() {
        let mut state = make_state();
        open_search(&mut state);
        state.search_state.query = "hello".into();
        state.search_state.substate = SearchSubstate::Navigating;
        let action = handle_input(
            &mut state,
            &DomainInputEvent::SpecialKey(DomainKey::Backspace),
        );
        assert_eq!(action, InputAction::SearchReturnToTyping);
        assert_eq!(state.search_state.query.as_str(), "hell");
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

    #[test]
    fn search_navigating_enter_requests_keep_position_without_popping_before_effect() {
        let mut state = make_state();
        open_search(&mut state);
        state.search_state.substate = SearchSubstate::Navigating;
        let frame = state.modal_stack.top().cloned();
        assert_eq!(focus_context(&state), Some(Context::SearchNavigating));
        assert_eq!(
            char_overlay_context(&state),
            Some(Context::SearchNavigating)
        );
        assert_eq!(
            handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter)),
            InputAction::SearchCommit
        );
        assert_eq!(state.modal_stack.top(), frame.as_ref());
        assert_eq!(state.focus, FocusState::Input);
    }

    #[test]
    fn search_back_is_an_effect_and_leader_captures_the_dynamic_frame_context() {
        let mut state = make_state();
        open_search(&mut state);
        let frame = state.modal_stack.top().cloned();
        assert_eq!(
            handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlC)),
            InputAction::CloseSearch
        );
        assert_eq!(state.modal_stack.top(), frame.as_ref());
        state.search_state.substate = SearchSubstate::Navigating;
        assert_eq!(
            handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX)),
            InputAction::Consumed
        );
        assert_eq!(state.pending_leader, Some(Context::SearchNavigating));
        assert_eq!(state.modal_stack.top(), frame.as_ref());
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
        state.focus = FocusState::Chat;
        presentation::reducer::open(state, Surface::BookmarkList);
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
                isolated: false,
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
                isolated: false,
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
    fn slash_clear_is_a_first_class_true_context_reset() {
        let mut state = make_state();
        state.input_buffer = "/clear".to_string();
        assert!(matches!(
            submit_message(&mut state),
            InputAction::ExecuteCommand { name, args: None } if name == "clear"
        ));
    }

    // Story 11.4 (Task 5.2) — `/context <sub>` routes to ExecuteCommand (not the
    // user-defined SubmitWithContext path) so the event-loop dispatch sees the arg.
    #[test]
    fn slash_context_routes_to_execute_command_with_subverb() {
        for (input, expected_arg) in [
            ("/context off", Some("off")),
            ("/context on", Some("on")),
            ("/context show", Some("show")),
            ("/context", None),
        ] {
            let mut state = make_state();
            state.input_buffer = input.to_string();
            match submit_message(&mut state) {
                InputAction::ExecuteCommand { name, args } => {
                    assert_eq!(name, "context", "input: {input}");
                    assert_eq!(args.as_deref(), expected_arg, "input: {input}");
                }
                other => panic!("expected ExecuteCommand for {input:?}, got {other:?}"),
            }
        }
    }

    // Story 11 (TUI E2E regression) — `/memory <sub>` must route to ExecuteCommand
    // like `/context`, so the event-loop arm sees consolidate/forget/<adapter>.
    // Before the fix it fell through to SubmitWithContext and dispatched an empty
    // turn (provider 400 "Input cannot be empty").
    #[test]
    fn slash_memory_routes_to_execute_command_with_subverb() {
        for (input, expected_arg) in [
            ("/memory consolidate", Some("consolidate")),
            ("/memory forget the secret", Some("forget the secret")),
            ("/memory daily-log", Some("daily-log")),
            ("/memory", None),
        ] {
            let mut state = make_state();
            state.input_buffer = input.to_string();
            match submit_message(&mut state) {
                InputAction::ExecuteCommand { name, args } => {
                    assert_eq!(name, "memory", "input: {input}");
                    assert_eq!(args.as_deref(), expected_arg, "input: {input}");
                }
                other => panic!("expected ExecuteCommand for {input:?}, got {other:?}"),
            }
        }
    }

    // 14.3c AI-12.3 human-smoke regression — `/fanout <N> <prompt>` (and
    // `/fanout cancel`) MUST route to ExecuteCommand so the event-loop `/fanout`
    // arm launches the wave. Before the fix it fell through to SubmitWithContext
    // with an empty `text`, dispatching an EMPTY main turn (provider 400 "Empty
    // input messages") and never launching the wave. Positive control: a genuine
    // user-defined command name still falls through to SubmitWithContext.
    #[test]
    fn slash_fanout_routes_to_execute_command_with_args() {
        for (input, expected_arg) in [
            ("/fanout 3 explore the module", Some("3 explore the module")),
            ("/fanout cancel", Some("cancel")),
            ("/fanout", None),
        ] {
            let mut state = make_state();
            state.input_buffer = input.to_string();
            match submit_message(&mut state) {
                InputAction::ExecuteCommand { name, args } => {
                    assert_eq!(name, "fanout", "input: {input}");
                    assert_eq!(args.as_deref(), expected_arg, "input: {input}");
                }
                other => panic!("expected ExecuteCommand for {input:?}, got {other:?}"),
            }
        }

        // Positive control: an unknown command still routes to SubmitWithContext
        // (proves the fanout arm is a specific match, not a catch-all).
        let mut state = make_state();
        state.input_buffer = "/totallyunknowncmd hi".to_string();
        assert!(
            matches!(
                submit_message(&mut state),
                InputAction::SubmitWithContext { .. }
            ),
            "unknown command must still fall through to SubmitWithContext"
        );
    }

    #[test]
    fn test_typing_at_agents_slash_switches_autocomplete_kind() {
        let mut state = make_state();
        // Type '@'
        let _ = handle_input(&mut state, &DomainInputEvent::KeyPress('@'));
        assert_eq!(state.autocomplete.kind, Some(AutocompleteKind::FileMention));
        // Type 'Agents/'
        for c in "Agents/".chars() {
            let _ = handle_input(&mut state, &DomainInputEvent::KeyPress(c));
        }
        assert_eq!(
            state.autocomplete.kind,
            Some(AutocompleteKind::AgentMention)
        );
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
        assert_eq!(
            state.autocomplete.kind,
            Some(AutocompleteKind::AgentMention)
        );
        // Backspace once to delete '/'
        let _ = handle_input(
            &mut state,
            &DomainInputEvent::SpecialKey(DomainKey::Backspace),
        );
        assert_eq!(state.autocomplete.kind, Some(AutocompleteKind::FileMention));
    }

    // Story 19.13: `@a2a/` namespace entry is case-insensitive — any casing of
    // the namespace token switches the popup to A2aMention and consumes the
    // namespace, while the suffix filter keeps the user's casing verbatim.
    #[test]
    fn test_typing_at_a2a_slash_switches_autocomplete_kind() {
        let mut state = make_state();
        for c in "@a2A/".chars() {
            let _ = handle_input(&mut state, &DomainInputEvent::KeyPress(c));
        }
        assert_eq!(state.autocomplete.kind, Some(AutocompleteKind::A2aMention));
        assert_eq!(state.autocomplete.filter_text, "");
        for c in "MyPeer".chars() {
            let _ = handle_input(&mut state, &DomainInputEvent::KeyPress(c));
        }
        assert_eq!(state.autocomplete.filter_text, "MyPeer");
        let _ = handle_input(
            &mut state,
            &DomainInputEvent::SpecialKey(DomainKey::Backspace),
        );
        assert_eq!(state.autocomplete.filter_text, "MyPee");
    }

    // Story 19.13: deleting the '/' of a mixed-case `@A2a/` namespace falls
    // back to FileMention (the A2A guard lowercases before matching) and keeps
    // the typed text in the buffer as a plain file filter.
    #[test]
    fn test_backspace_past_a2a_slash_returns_to_file_mention() {
        let mut state = make_state();
        let _ = handle_input(&mut state, &DomainInputEvent::KeyPress('@'));
        for c in "A2a/".chars() {
            let _ = handle_input(&mut state, &DomainInputEvent::KeyPress(c));
        }
        assert_eq!(state.autocomplete.kind, Some(AutocompleteKind::A2aMention));
        let _ = handle_input(
            &mut state,
            &DomainInputEvent::SpecialKey(DomainKey::Backspace),
        );
        assert_eq!(state.autocomplete.kind, Some(AutocompleteKind::FileMention));
        assert_eq!(state.input_buffer, "@A2a");
        assert_eq!(state.autocomplete.filter_text, "A2a");
    }

    // Story 19.13: selecting an A2A suggestion (Tab through handle_input)
    // inserts the canonical `a2a__<peer>__<skill>` wire name — never the raw
    // peer skill ID. A hostile peer exposing a skill named "Read" must not
    // produce a bare local-tool reference, the multi-byte Unicode text around
    // the trigger must survive, and the cursor must rest after the insertion.
    #[test]
    fn test_a2a_selection_inserts_wire_name_not_raw_hostile_read() {
        let mut state = make_state();
        for c in "héllo 🌍  終".chars() {
            let _ = handle_input(&mut state, &DomainInputEvent::KeyPress(c));
        }
        for _ in " 終".chars() {
            let _ = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Left));
        }
        let _ = handle_input(&mut state, &DomainInputEvent::KeyPress('@'));
        assert_eq!(state.autocomplete.kind, Some(AutocompleteKind::FileMention));
        for c in "a2a/".chars() {
            let _ = handle_input(&mut state, &DomainInputEvent::KeyPress(c));
        }
        assert_eq!(state.autocomplete.kind, Some(AutocompleteKind::A2aMention));
        state.autocomplete.suggestions.push(
            crate::domain::models::autocomplete::AutocompleteSuggestion::A2aAgent {
                peer: "evil-peer".to_string(),
                name: "Read".to_string(),
                description: "hostile skill".to_string(),
            },
        );
        let _ = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Tab));
        assert!(
            !state.autocomplete.is_open(),
            "popup dismissed after selection"
        );
        assert_eq!(
            state.input_buffer, "héllo 🌍 a2a__evil-peer__Read 終",
            "canonical wire name replaces '@' trigger; Unicode prefix and suffix preserved"
        );
        assert_eq!(
            state.cursor_position,
            "héllo 🌍 a2a__evil-peer__Read".chars().count(),
            "cursor directly after the inserted wire name"
        );
    }

    // ── Story 16.5.5: Feedback Action Dispatch Arbiter ────────────────────────

    #[test]
    fn bare_c_in_input_focus_with_feedback_inserts_text_not_compact() {
        let mut state = make_state();
        state.focus = FocusState::Input;
        let action = handle_char(&mut state, 'c');
        assert_eq!(action, InputAction::Consumed);
        assert!(state.input_buffer.contains('c'));
    }

    #[test]
    fn story21_1_ctrl_k_kills_the_line_tail_and_the_next_c_is_literal_text() {
        // Old: `Ctrl+K` then `c` dispatched FeedbackCompact and never typed.
        // New (AC12, migrated by 21-3 AC11): Ctrl+K kills the line tail and arms
        // nothing; `c` types into the composer.
        let mut state = make_state();
        state.focus = FocusState::Input;
        state.input_buffer = "one".to_owned();
        state.cursor_position = 1;
        let ctrl_k = DomainInputEvent::SpecialKey(DomainKey::CtrlK);
        handle_input(&mut state, &ctrl_k);
        assert_eq!(state.input_buffer, "o");
        let action = handle_char(&mut state, 'c');
        assert_eq!(action, InputAction::Consumed);
        assert_eq!(state.input_buffer, "oc", "c types as text");
    }

    #[test]
    fn story21_1_ctrl_k_chord_is_gone_in_every_focus() {
        // Old: `Ctrl+K c` dispatched FeedbackCompact in any focus. New (AC12):
        // the chord is deleted — the next `c` is whatever it is without Ctrl+K.
        for focus in [FocusState::Input, FocusState::Chat] {
            let mut state = make_state();
            state.focus = focus.clone();
            let ctrl_k = DomainInputEvent::SpecialKey(DomainKey::CtrlK);
            handle_input(&mut state, &ctrl_k);
            let action = handle_char(&mut state, 'c');
            assert!(
                !matches!(
                    action,
                    InputAction::Decision(_) | InputAction::FocusDecisions
                ),
                "Ctrl+K arms nothing in {focus:?}: {action:?}"
            );
        }
    }

    #[test]
    fn chat_focus_bare_c_still_copies() {
        let mut state = make_state();
        state.focus = FocusState::Chat;
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
    fn story21_1_ctrl_k_then_z_is_literal_text_no_leader_is_pending() {
        // Old (16.5.5): Ctrl+K armed a chord leader and `z` was swallowed as an
        // invalid feedback key. New (AC12): Ctrl+K is a kill-line key, never a chord.
        let mut state = make_state();
        state.focus = FocusState::Input;
        let ctrl_k = DomainInputEvent::SpecialKey(DomainKey::CtrlK);
        handle_input(&mut state, &ctrl_k);
        let action = handle_char(&mut state, 'z');
        assert_eq!(action, InputAction::Consumed);
        assert!(state.input_buffer.contains('z'), "z types as text");
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
    fn tab_in_chat_focus_emits_cycle_item() {
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
    fn wave_keys_are_conditional_on_retained_wave() {
        let mut without_wave = make_state();
        without_wave.focus = FocusState::Chat;
        assert_ne!(
            handle_input(&mut without_wave, &DomainInputEvent::KeyPress('d')),
            InputAction::OpenDivergeView
        );

        let mut with_wave = state_with_wave();
        assert_eq!(
            handle_input(&mut with_wave, &DomainInputEvent::KeyPress('d')),
            InputAction::OpenDivergeView
        );
        assert!(with_wave.needs_redraw);

        let mut with_wave = state_with_wave();
        assert_eq!(
            handle_input(&mut with_wave, &DomainInputEvent::KeyPress('s')),
            InputAction::CloseDivergeView
        );

        let mut with_wave = state_with_wave();
        // P3 (review): `r` from Chat focus opens the wave overlay so the user
        // can select a slot, rather than blindly re-running slot 0.
        assert_eq!(
            handle_input(&mut with_wave, &DomainInputEvent::KeyPress('r')),
            InputAction::OpenWaveOverlay
        );
        assert!(presentation::is_open(&with_wave, Surface::WaveOverlay));
        assert_eq!(with_wave.focus, FocusState::Chat);
    }

    #[test]
    fn enter_on_wave_strip_opens_overlay_before_tool_inspection() {
        let mut state = state_with_wave();
        assert_eq!(
            handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter)),
            InputAction::OpenWaveOverlay
        );
    }

    #[test]
    fn story21_6_spawn_gate_keys_belong_to_the_frame_not_the_chat_handlers() {
        use crate::adapters::tui::decision_queue::{
            DecisionKind, PendingSpawnGate, raise_spawn_gate,
        };
        let mut state = make_state();
        state.focus = FocusState::Chat;
        raise_spawn_gate(
            &mut state,
            "c1",
            PendingSpawnGate {
                spec: crate::adapters::tui::fanout_spec::parse_fanout(Some("3 test")).unwrap(),
                requested: 3,
                threshold: 2,
                adjusted: None,
            },
        );
        assert!(state.decisions.holds(DecisionKind::SpawnGate));
        assert_eq!(
            state.modal_stack.top().map(|frame| frame.surface),
            Some(Surface::Decision),
            "a user-started gate takes the keyboard"
        );
        // Nothing the old chat handlers answered (`c` cap, `n` close-wave) reaches them now,
        // and the frame never reads a letter into the composer.
        for c in ['c', 'n'] {
            let action = handle_input(&mut state, &DomainInputEvent::KeyPress(c));
            assert_ne!(action, InputAction::CloseWaveOverlay, "{c}");
            assert_ne!(action, InputAction::SpawnGateConfirm, "{c}");
        }
        assert!(state.input_buffer.is_empty());
        // `Esc` defers, never clears the gate.
        handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
        assert!(state.decisions.holds(DecisionKind::SpawnGate));
    }

    /// 6D-L1-03: background and stale queue members cannot own confirmation chrome.
    #[test]
    fn story21_6_only_live_here_confirmations_present() {
        use crate::adapters::tui::decision_queue::{self, Arrival, DecisionDraft, DecisionKind};
        let mut state = make_state();
        let facts = crate::adapters::tui::notification_log::LogFacts {
            active_conversation_id: Some("here".into()),
            ..Default::default()
        };
        decision_queue::prune_closed(&mut state, &facts);
        decision_queue::raise(
            &mut state,
            DecisionDraft::new(DecisionKind::Forget, "elsewhere").conversation("elsewhere"),
            Arrival::Requested,
        );
        assert!(state.decisions.holds(DecisionKind::Forget));
        assert!(!state.decisions.presenting(DecisionKind::Forget));
        assert!(!decision_queue::higher_confirmation_presenting(&state));
        decision_queue::raise(
            &mut state,
            DecisionDraft::new(DecisionKind::Forget, "here").conversation("here"),
            Arrival::Requested,
        );
        assert!(state.decisions.presenting(DecisionKind::Forget));
        assert!(decision_queue::higher_confirmation_presenting(&state));
        decision_queue::withdraw_for(&mut state, DecisionKind::Forget, "here");
        assert!(state.decisions.holds(DecisionKind::Forget));
        assert!(!state.decisions.presenting(DecisionKind::Forget));
        assert!(!decision_queue::higher_confirmation_presenting(&state));
    }

    /// A deferred Forget must not hide or disable a reopened proposal frame.
    #[cfg(feature = "a2a")]
    #[test]
    fn story21_6_deferred_forget_restores_local_proposal() {
        use crate::adapters::tui::decision_queue::{
            self, DecisionEffect, DecisionKind, DecisionSubject, Responder,
        };
        use crate::adapters::tui::notifications::NotificationStore;
        use crate::domain::clock::MockClock;
        use crate::domain::models::{SemanticMessageType, TeamProposal};
        use ratatui::{Terminal, backend::TestBackend};
        use std::sync::Arc;

        let mut state = make_state();
        let clock = Arc::new(MockClock::new(std::time::Instant::now()));
        state.notifications = NotificationStore::from_config(
            &crate::domain::models::NotificationsConfig::default(),
            clock.clone(),
        );
        let facts = crate::adapters::tui::notification_log::LogFacts {
            active_conversation_id: Some("c".into()),
            ..Default::default()
        };
        let proposal = Arc::new(TeamProposal {
            id: "tp-deferred".into(),
            conversation_id: "c".into(),
            recipients: vec!["peer".into()],
            message_type: SemanticMessageType::ArchitectureUpdate,
            body: "proposal body".into(),
        });
        decision_queue::raise_team_proposal(&mut state, proposal, Responder::Local);
        let proposal_key = state.decisions.items().next().unwrap().key();
        let paint = |state: &TuiState| {
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            let mut painted = false;
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    painted = crate::adapters::tui::widgets::decision_frame::render(
                        frame,
                        area,
                        state,
                        &facts,
                        &state.theme,
                        "Decision",
                        None,
                    );
                })
                .unwrap();
            assert!(painted);
            let buffer = terminal.backend().buffer();
            (0..24)
                .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>())
                .collect::<Vec<_>>()
        };
        let before = paint(&state);
        assert!(before.iter().any(|row| row.contains("proposal body")));
        decision_queue::raise_forget(
            &mut state,
            decision_queue::PendingForgetCard {
                conversation_id: "c".into(),
                candidates: Vec::new(),
                focused_index: 0,
            },
        );
        assert_eq!(
            decision_queue::current(&state, &facts).unwrap().kind,
            DecisionKind::Forget
        );
        route_conversation_decision(
            &mut state,
            "c",
            DomainInputEvent::SpecialKey(DomainKey::Esc),
        );
        assert!(state.decisions.holds(DecisionKind::Forget));
        // Deferral does not automatically reselect waiting items (A9).
        assert!(decision_queue::open(&mut state, &proposal_key));
        assert_eq!(
            decision_queue::current(&state, &facts).unwrap().kind,
            DecisionKind::TeamProposal
        );
        let after = paint(&state);
        assert!(after.iter().any(|row| row.contains("proposal body")));
        decision_queue::note_painted(&mut state);
        clock.advance(decision_queue::INERT_AFTER_PAINT);
        assert_eq!(
            route_conversation_decision(&mut state, "c", DomainInputEvent::KeyPress('y')),
            InputAction::Decision(DecisionEffect::TeamProposalSend)
        );
        assert!(
            matches!(state.decisions.take_resolved(), Some(DecisionSubject::TeamProposal(payload))
            if payload.proposal.id == "tp-deferred" && payload.proposal.conversation_id == "c")
        );
    }

    /// Proposal rows act on their frame; Back defers and Review is a sub-context.
    #[cfg(feature = "a2a")]
    #[test]
    fn story21_6_local_team_proposal_rows_decide_once_and_back_defers() {
        use crate::adapters::tui::decision_queue::{
            self, DecisionEffect, DecisionKind, DecisionSubject, Responder,
        };
        use crate::adapters::tui::notifications::NotificationStore;
        use crate::domain::clock::MockClock;
        use crate::domain::models::{SemanticMessageType, TeamProposal};
        use std::sync::Arc;

        let proposal = Arc::new(TeamProposal {
            id: "tp1".into(),
            conversation_id: "c".into(),
            recipients: vec!["lena-po".into()],
            message_type: SemanticMessageType::ArchitectureUpdate,
            body: "body".into(),
        });
        let build = || {
            let mut state = make_state();
            let clock = Arc::new(MockClock::new(std::time::Instant::now()));
            state.notifications = NotificationStore::from_config(
                &crate::domain::models::NotificationsConfig::default(),
                clock.clone(),
            );
            decision_queue::raise_team_proposal(&mut state, proposal.clone(), Responder::Local);
            (state, clock)
        };
        for (key, effect) in [
            ('y', DecisionEffect::TeamProposalSend),
            ('n', DecisionEffect::TeamProposalDismiss),
        ] {
            let (mut state, clock) = build();
            decision_queue::note_painted(&mut state);
            assert_eq!(
                route_conversation_decision(&mut state, "c", DomainInputEvent::KeyPress(key)),
                InputAction::Consumed
            );
            clock.advance(decision_queue::INERT_AFTER_PAINT);
            assert_eq!(
                route_conversation_decision(&mut state, "c", DomainInputEvent::KeyPress(key)),
                InputAction::Decision(effect)
            );
            assert!(
                matches!(state.decisions.take_resolved(), Some(DecisionSubject::TeamProposal(payload))
                if payload.proposal.id == "tp1" && payload.proposal.conversation_id == "c")
            );
            assert!(!state.decisions.holds(DecisionKind::TeamProposal));
            assert_ne!(
                handle_input(&mut state, &DomainInputEvent::KeyPress(key)),
                InputAction::Decision(effect)
            );
        }
        let (mut review, clock) = build();
        decision_queue::note_painted(&mut review);
        clock.advance(decision_queue::INERT_AFTER_PAINT);
        assert_eq!(
            route_conversation_decision(&mut review, "c", DomainInputEvent::KeyPress('r')),
            InputAction::Consumed
        );
        assert!(
            matches!(&review.decisions.items().next().unwrap().subject, DecisionSubject::TeamProposal(p) if p.reviewing)
        );
        assert_eq!(
            route_conversation_decision(
                &mut review,
                "c",
                DomainInputEvent::SpecialKey(DomainKey::Esc)
            ),
            InputAction::Consumed
        );
        assert!(
            matches!(&review.decisions.items().next().unwrap().subject, DecisionSubject::TeamProposal(p) if !p.reviewing)
        );
        let (mut typing, clock) = build();
        typing.input_buffer = "draft".into();
        decision_queue::note_painted(&mut typing);
        clock.advance(decision_queue::INERT_AFTER_PAINT);
        assert_eq!(
            route_conversation_decision(&mut typing, "c", DomainInputEvent::KeyPress('y')),
            InputAction::Consumed
        );
        assert_eq!(typing.input_buffer, "draft");
        assert!(typing.decisions.holds(DecisionKind::TeamProposal));
        for key in [DomainKey::Esc, DomainKey::CtrlC] {
            let (mut state, _) = build();
            assert_eq!(
                route_conversation_decision(&mut state, "c", DomainInputEvent::SpecialKey(key)),
                InputAction::Consumed
            );
            let item = state.decisions.items().next().unwrap();
            assert_eq!(item.kind, DecisionKind::TeamProposal);
            assert!(item.deferred, "{key:?}");
            assert!(state.modal_stack.top().is_none());
        }
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

    // ─── AC1 reach-firewall: remaining keys ─────────────────────────────────

    #[test]
    fn ac1_esc_in_wave_overlay_closes_overlay() {
        let mut state = state_with_wave();
        presentation::reducer::open(&mut state, Surface::WaveOverlay);
        let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
        assert_eq!(action, InputAction::CloseWaveOverlay);
    }

    #[test]
    fn ac1_enter_in_wave_overlay_drills_selected_slot() {
        let mut state = state_with_wave();
        presentation::reducer::open(&mut state, Surface::WaveOverlay);
        state.wave_overlay_selected = 1;
        let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
        assert_eq!(action, InputAction::WaveDrillSpoke(1));
    }

    #[test]
    fn ac1_j_in_wave_overlay_advances_selection() {
        let mut state = state_with_wave();
        presentation::reducer::open(&mut state, Surface::WaveOverlay);
        state.wave_overlay_selected = 0;
        let _ = handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
        // With EmptyWaveHandle (0 spokes), selection stays clamped at 0
        assert_eq!(state.wave_overlay_selected, 0);
        assert!(state.needs_redraw);
    }

    #[test]
    fn ac1_k_in_wave_overlay_reverses_selection() {
        let mut state = state_with_wave();
        presentation::reducer::open(&mut state, Surface::WaveOverlay);
        state.wave_overlay_selected = 2;
        let _ = handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
        assert_eq!(state.wave_overlay_selected, 1);
        assert!(state.needs_redraw);
    }

    #[test]
    fn ac1_r_in_wave_overlay_reruns_selected_slot() {
        let mut state = state_with_wave();
        presentation::reducer::open(&mut state, Surface::WaveOverlay);
        state.wave_overlay_selected = 2;
        let action = handle_input(&mut state, &DomainInputEvent::KeyPress('r'));
        assert_eq!(action, InputAction::WaveRerunSpoke(2));
    }

    #[test]
    fn ac1_d_in_wave_overlay_opens_diverge() {
        let mut state = state_with_wave();
        presentation::reducer::open(&mut state, Surface::WaveOverlay);
        let action = handle_input(&mut state, &DomainInputEvent::KeyPress('d'));
        assert_eq!(action, InputAction::OpenDivergeView);
    }

    #[test]
    fn ac1_s_in_wave_overlay_closes_diverge() {
        let mut state = state_with_wave();
        presentation::reducer::open(&mut state, Surface::WaveOverlay);
        let action = handle_input(&mut state, &DomainInputEvent::KeyPress('s'));
        assert_eq!(action, InputAction::CloseDivergeView);
    }

    // ─── AC1 reach-firewall: positive controls ──────────────────────────────

    #[test]
    fn ac1_d_without_wave_does_not_open_diverge() {
        let mut state = make_state();
        state.focus = FocusState::Chat;
        let action = handle_input(&mut state, &DomainInputEvent::KeyPress('d'));
        assert_ne!(action, InputAction::OpenDivergeView);
    }

    #[test]
    fn ac1_enter_without_wave_does_not_open_overlay() {
        let mut state = make_state();
        state.focus = FocusState::Chat;
        let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
        assert_ne!(action, InputAction::OpenWaveOverlay);
    }

    // ─── AC3 diverge involution ─────────────────────────────────────────────

    #[test]
    fn ac3_d_then_s_is_identity_involution() {
        let mut state = state_with_wave();
        // d opens diverge
        let action = handle_input(&mut state, &DomainInputEvent::KeyPress('d'));
        assert_eq!(action, InputAction::OpenDivergeView);
        assert!(state.needs_redraw);

        // s closes diverge — back to Chat focus
        state.needs_redraw = false;
        let action = handle_input(&mut state, &DomainInputEvent::KeyPress('s'));
        assert_eq!(action, InputAction::CloseDivergeView);
        assert!(state.needs_redraw);
    }

    // ─── 14.3c: Esc-unwind dismiss of the wave display ──────────────────────

    #[test]
    fn esc_dismisses_wave_in_chat_focus() {
        let mut state = state_with_wave();
        state.focus = FocusState::Chat;
        let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
        assert_eq!(action, InputAction::DismissWave);
    }

    #[test]
    fn esc_closes_diverge_before_dismissing_wave() {
        // With the DivergeView open, Esc unwinds it first (keeps the wave),
        // NOT a full dismiss.
        let mut state = state_with_wave();
        state.focus = FocusState::Chat;
        let open = handle_input(&mut state, &DomainInputEvent::KeyPress('d'));
        crate::adapters::tui::handlers::wave_overlay::apply(&mut state, "wave-stage", &open);
        assert!(presentation::is_open(&state, Surface::WaveOverlay));
        let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
        assert_eq!(action, InputAction::CloseDivergeView);
        crate::adapters::tui::handlers::wave_overlay::apply(&mut state, "wave-stage", &action);
        assert!(!presentation::is_open(&state, Surface::WaveOverlay));
        assert!(state.wave_run.is_some());
    }

    #[test]
    fn esc_without_wave_is_unchanged_no_dismiss() {
        // Regression guard (UX safety): with NO wave present, Esc must NOT
        // produce DismissWave/CloseDivergeView — the existing focus-toggle
        // behavior is preserved untouched.
        let mut state = make_state();
        state.focus = FocusState::Chat;
        let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
        assert_ne!(action, InputAction::DismissWave);
        assert_ne!(action, InputAction::CloseDivergeView);
        // Existing behavior: Esc in Chat toggles focus to Input.
        assert_eq!(state.focus, FocusState::Input);
    }

    #[test]
    fn dismiss_wave_clears_all_wave_state() {
        let mut state = state_with_wave();
        state.wave_state = Some(crate::adapters::tui::state::WaveViewState::started(
            crate::domain::models::AgentId::root(),
            3,
        ));
        state.rerunning_slot = Some(1);
        state.wave_diverge_open = true;
        state.active_wave_id = Some(5);

        state.dismiss_wave(); // PRODUCTION handler

        assert!(state.wave_run.is_none(), "handle dropped");
        assert!(state.wave_state.is_none(), "view state cleared");
        assert!(state.rerunning_slot.is_none(), "lamp cleared");
        assert!(!state.wave_diverge_open, "diverge closed");
        assert!(!state.is_wave_in_flight(), "in-flight marker cleared");
    }

    // ─── AC6 cancel state machine ───────────────────────────────────────────

    // 14.3c AI-12.3 remediation (DF-CR-14-3c-6): the prior LF3/LF4 keystones
    // were vacuous (inline handler re-implementation / tokio-util / setter-getter
    // / tautology mutants). These DRIVE the extracted `TuiState::*` handlers — the
    // SAME code the event loop runs — so a mutated handler turns its keystone RED.

    #[test]
    fn ac8_request_wave_cancel_fires_token_iff_in_flight() {
        use tokio_util::sync::CancellationToken;

        // In flight: the trigger fires the wave-cancel token (a real spoke child
        // observes it), clears the lamp, and self-heals the in-flight marker.
        let mut state = state_with_wave();
        let token = CancellationToken::new();
        let spoke_child = token.child_token(); // models a dispatched spoke's token
        state.wave_cancel = Some(token);
        state.active_wave_id = Some(7);
        state.rerunning_slot = Some(2);
        assert!(
            state.request_wave_cancel(),
            "in-flight Ctrl-C must be consumed"
        );
        assert!(
            spoke_child.is_cancelled(),
            "cancel-all must propagate to the spoke child token"
        );
        assert!(
            state.rerunning_slot.is_none(),
            "rerun lamp cleared on cancel"
        );
        assert!(!state.is_wave_in_flight(), "in-flight marker self-healed");

        // Positive control + mutant kill: NO wave in flight → the trigger must
        // NOT fire (returns false so Ctrl-C falls through to quit). An ungated
        // handler would fire `idle_child` here → RED.
        let mut idle = make_state();
        let token2 = CancellationToken::new();
        let idle_child = token2.child_token();
        idle.wave_cancel = Some(token2);
        idle.active_wave_id = None;
        assert!(
            !idle.request_wave_cancel(),
            "idle Ctrl-C must not be consumed"
        );
        assert!(
            !idle_child.is_cancelled(),
            "idle Ctrl-C must NOT fire a stale wave token"
        );
    }

    #[test]
    fn ac8_handle_wave_cancelled_marks_view_and_retires() {
        use crate::adapters::tui::state::WaveViewState;
        let mut state = state_with_wave();
        state.wave_state = Some(WaveViewState::started(
            crate::domain::models::AgentId::root(),
            3,
        ));
        state.active_wave_id = Some(9);

        state.handle_wave_cancelled(); // PRODUCTION handler

        assert!(
            state.wave_state.as_ref().unwrap().cancelled,
            "wave view marked cancelled"
        );
        assert!(!state.is_wave_in_flight(), "in-flight marker retired");
        // Positive control: a fresh view is NOT cancelled (the flag is real, not
        // always-true). A handler that skips record_wave_cancelled → first
        // assert RED.
        assert!(!WaveViewState::started(crate::domain::models::AgentId::root(), 3).cancelled);
    }

    // ─── AC7 rerun-UI state machine ─────────────────────────────────────────

    #[test]
    fn ac11_handle_wave_run_ready_swaps_handle_and_clears_lamp() {
        let mut state = state_with_wave();
        let old_ptr =
            std::sync::Arc::as_ptr(state.wave_run.as_ref().unwrap()) as *const () as usize;
        state.rerunning_slot = Some(1); // a rerun was in flight
        state.active_wave_id = Some(4);

        let new_handle: std::sync::Arc<dyn crate::domain::ports::wave_handle::WaveHandle> =
            std::sync::Arc::new(SpyWaveHandle::new(3));
        let new_ptr = std::sync::Arc::as_ptr(&new_handle) as *const () as usize;

        let returned = state.handle_wave_run_ready(new_handle); // PRODUCTION handler

        assert_eq!(
            returned,
            Some(1),
            "rerun completion returns the slot for the notice"
        );
        assert!(state.rerunning_slot.is_none(), "lamp cleared on swap");
        assert!(!state.is_wave_in_flight(), "in-flight marker retired");
        let current_ptr =
            std::sync::Arc::as_ptr(state.wave_run.as_ref().unwrap()) as *const () as usize;
        assert_eq!(current_ptr, new_ptr, "wave_run points at the NEW handle");
        assert_ne!(current_ptr, old_ptr, "old handle was swapped out");
    }

    #[test]
    fn ac11_rerun_swap_leaves_new_handle_live_not_cancelled() {
        // Shared-token-hazard guard: handle_wave_run_ready must NOT cancel the
        // swapped-out handle. In the production CoW rerun the prior + new handle
        // SHARE one wave_cancel token (orchestrator/mod.rs:754), so a naive
        // old.cancel() would also cancel the freshly-swapped-in handle. Model the
        // shared token by giving the OLD and NEW handle the SAME cancel flag — a
        // naive abort then flips it and BOTH asserts go RED.
        let shared = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut state = make_state();
        state.focus = FocusState::Chat;
        state.wave_run = Some(std::sync::Arc::new(SpyWaveHandle::with_flag(
            2,
            shared.clone(),
        ))); // prior
        state.rerunning_slot = Some(0);

        let new_handle: std::sync::Arc<dyn crate::domain::ports::wave_handle::WaveHandle> =
            std::sync::Arc::new(SpyWaveHandle::with_flag(2, shared.clone())); // CoW: shares the token

        state.handle_wave_run_ready(new_handle); // PRODUCTION handler

        assert!(
            !shared.load(std::sync::atomic::Ordering::SeqCst),
            "swap must not cancel the shared token (would kill the new handle)"
        );
        assert!(
            !state.wave_run.as_ref().unwrap().snapshot().cancelled,
            "new handle stays live after the rerun swap"
        );
    }

    #[test]
    fn ac11_handle_spoke_rerun_started_sets_correct_slot_counter_inert() {
        use crate::adapters::tui::state::WaveViewState;
        let mut state = state_with_wave();
        state.wave_state = Some(WaveViewState::started(
            crate::domain::models::AgentId::root(),
            3,
        ));
        state.needs_redraw = false;

        state.handle_spoke_rerun_started(2); // PRODUCTION handler, slot 2 (P3 guard)

        assert_eq!(
            state.rerunning_slot,
            Some(2),
            "lamp must be the pressed slot (P3: not blindly 0)"
        );
        assert!(state.needs_redraw, "redraw requested");
        // Counter-inert: rerun-started must NOT bump completed_count.
        assert_eq!(
            state.wave_state.as_ref().unwrap().completed_count,
            0,
            "SpokeRerunStarted must not bump completed_count"
        );
        // Positive control: the bump method DOES bump — proves the assert above
        // is non-vacuous (a handler that bumped would read 1 → RED).
        let mut wv = WaveViewState::started(crate::domain::models::AgentId::root(), 3);
        wv.record_spoke_completed();
        assert_eq!(wv.completed_count, 1);
    }

    #[test]
    fn ac11_handle_spoke_rerun_reverted_clears_matching_slot_only() {
        let mut state = state_with_wave();
        state.rerunning_slot = Some(2);

        state.handle_spoke_rerun_reverted(1); // wrong slot → must NOT clear
        assert_eq!(
            state.rerunning_slot,
            Some(2),
            "a stale revert for another slot must not clear the live lamp"
        );

        state.handle_spoke_rerun_reverted(2); // matching slot → clears
        assert!(
            state.rerunning_slot.is_none(),
            "matching revert clears the lamp"
        );
    }

    #[test]
    fn ac11_push_vs_pull_render_reads_swapped_handle_not_stale_push_counter() {
        // The gate's centre of gravity. The render PULLS the count from
        // state.wave_run.snapshot().completed (event_loop.rs:9408) — NEVER the
        // push counter wave_state.completed_count. Poison the push source to 99,
        // swap in a handle honestly reporting 3 via the PRODUCTION handler, then
        // assert the production render data-source reads 3, not 99.
        use crate::adapters::tui::state::WaveViewState;
        let mut state = state_with_wave();
        state.wave_state = Some(WaveViewState::started(
            crate::domain::models::AgentId::root(),
            3,
        ));
        if let Some(wv) = state.wave_state.as_mut() {
            wv.completed_count = 99; // POISON the stale push source
        }
        state.rerunning_slot = Some(1);

        state.handle_wave_run_ready(std::sync::Arc::new(SpyWaveHandle::new(3)));

        // Mirrors the production render data-source line (event_loop.rs:9408).
        let rendered_completed = state.wave_run.as_ref().unwrap().snapshot().completed;
        assert_eq!(
            rendered_completed, 3,
            "render must PULL the count from the swapped handle"
        );
        assert_ne!(
            rendered_completed, 99,
            "render must NOT read the poisoned push counter"
        );
    }
    #[test]
    fn story21_7_enter_is_inert_in_the_read_only_room_tab() {
        let mut state = TuiState::new(160, 40);
        presentation::reducer::open_mission_control(
            &mut state,
            crate::adapters::tui::mission_control::McTab::Room,
        );
        state.input_buffer = "draft".to_owned();
        assert_eq!(
            handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter)),
            InputAction::Consumed
        );
        assert_eq!(
            state.input_buffer, "draft",
            "Enter never reaches the composer"
        );
        assert!(crate::adapters::tui::mission_control::is_top(&state));
    }

    /// Both artifact modes share the queue's y/n dispatch.
    #[test]
    fn apply_card_keys_only_own_input_while_decision_frame_is_active() {
        use crate::adapters::tui::decision_queue::{self, ArtifactCardMode, PendingArtifactCard};
        let make_card = |mode: ArtifactCardMode| {
            let hash =
                crate::domain::models::ContentHash::parse_hex(&"e".repeat(64)).expect("hash");
            PendingArtifactCard {
                conversation_id: "conversation".to_owned(),
                artifact: crate::domain::models::EvidenceArtifact {
                    id: crate::domain::models::ArtifactId::from(hash),
                    kind: crate::domain::models::ArtifactKind::Patch,
                    producer: crate::domain::models::AgentId::parse("spoke-1").expect("agent"),
                    content_hash: hash,
                    authority: crate::domain::models::CapabilityTokenId::default(),
                    provenance: vec![crate::domain::models::ProvenanceTag::UserOriginated],
                    depends_on: Vec::new(),
                    review: Some(crate::domain::models::ReviewStatus::Pending),
                    host: crate::domain::models::HostBinding::new("host-A", "workspace"),
                },
                files: vec!["src/lib.rs".to_owned()],
                workspace: std::path::PathBuf::from("/workspace"),
                predates_apply_records: false,
                mode,
            }
        };
        for mode in [
            ArtifactCardMode::Apply,
            ArtifactCardMode::Resolve(crate::domain::models::OperatorApplyFinding::Present),
        ] {
            for (key, expected) in [
                ('y', InputAction::ApplyCardAccept),
                ('n', InputAction::ApplyCardDecline),
                ('x', InputAction::Consumed),
            ] {
                let clock = std::sync::Arc::new(crate::domain::clock::MockClock::new(
                    std::time::Instant::now(),
                ));
                let mut state = TuiState::new(80, 24);
                state.notifications =
                    crate::adapters::tui::notifications::NotificationStore::from_config(
                        &crate::domain::models::NotificationsConfig::default(),
                        clock.clone(),
                    );
                decision_queue::raise_artifact(&mut state, make_card(mode));
                decision_queue::note_painted(&mut state);
                clock.advance(decision_queue::INERT_AFTER_PAINT);
                assert_eq!(
                    route_conversation_decision(
                        &mut state,
                        "conversation",
                        DomainInputEvent::KeyPress(key)
                    ),
                    expected,
                    "{mode:?}"
                );
            }
            let mut state = TuiState::new(80, 24);
            decision_queue::raise_artifact(&mut state, make_card(mode));
            assert_eq!(
                route_conversation_decision(
                    &mut state,
                    "conversation",
                    DomainInputEvent::SpecialKey(DomainKey::Esc)
                ),
                InputAction::Consumed
            );
            assert!(decision_queue::artifact_pending(&state, "conversation"));
            assert_eq!(state.focus, FocusState::Input);
            handle_input(&mut state, &DomainInputEvent::KeyPress('n'));
            assert_eq!(state.input_buffer, "n", "{mode:?}");
            assert!(decision_queue::artifact_pending(&state, "conversation"));
        }
    }
}

#[cfg(test)]
mod story21_2_shortcut_tests {
    use super::*;
    use crate::adapters::tui::decision_queue::{DecisionKind, PlanCardPayload, raise_plan_card};
    use crate::adapters::tui::quit_confirm::{QuitFacts, QuitHost};
    use crate::domain::models::plan::{Plan, PlanStatus};

    fn key(c: char) -> DomainInputEvent {
        DomainInputEvent::KeyPress(c)
    }
    fn special(k: DomainKey) -> DomainInputEvent {
        DomainInputEvent::SpecialKey(k)
    }
    fn plan_card(state: &mut TuiState) {
        raise_plan_card(
            state,
            PlanCardPayload {
                conversation_id: "c1".into(),
                plan_id: "p1".into(),
                plan_snapshot: Plan {
                    id: "p1".into(),
                    title: "t".into(),
                    tasks: vec![],
                    estimated_effort: None,
                    status: PlanStatus::Pending,
                    created_at: 0,
                    resolved_at: None,
                    host_message_id: None,
                    origin_restrictions: None,
                },
            },
        );
    }

    #[test]
    fn story21_6_plan_letters_are_text_in_input_and_never_resolve_the_plan_in_chat() {
        // Typing protects the composer: the plan waits and the letters are text.
        let mut input = TuiState::new(80, 24);
        input.focus = FocusState::Input;
        input.input_buffer = "a".into();
        input.cursor_position = 1;
        plan_card(&mut input);
        for c in ['y', 'e', 'n'] {
            let action = handle_input(&mut input, &key(c));
            assert!(
                !matches!(
                    action,
                    InputAction::PlanCardApprove
                        | InputAction::PlanCardEdit
                        | InputAction::PlanCardReject
                ),
                "Input `{c}` must not resolve the plan, got {action:?}"
            );
        }
        assert_eq!(input.input_buffer, "ayen", "letters typed at the cursor");
        assert!(
            input.decisions.holds(DecisionKind::PlanCard),
            "plan retained"
        );
        // The plan is answerable only through its frame: a bare Chat letter is no answer.
        let mut chat = TuiState::new(80, 24);
        chat.focus = FocusState::Chat;
        plan_card(&mut chat);
        for c in ['y', 'e', 'n'] {
            let action = handle_input(&mut chat, &key(c));
            assert!(
                !matches!(
                    action,
                    InputAction::PlanCardApprove
                        | InputAction::PlanCardEdit
                        | InputAction::PlanCardReject
                ),
                "Chat `{c}` must not resolve the plan before the frame is ready, got {action:?}"
            );
        }
        assert!(chat.decisions.holds(DecisionKind::PlanCard));
    }

    #[test]
    fn story21_2_esc_never_submits_multiline_input_and_preserves_text() {
        let mut state = TuiState::new(80, 24);
        state.multiline_mode = true;
        state.input_buffer = "line one\nline two".into();
        state.cursor_position = state.input_buffer.chars().count();
        let action = handle_input(&mut state, &special(DomainKey::Esc));
        assert!(
            !matches!(action, InputAction::SubmitMessage(_)),
            "{action:?}"
        );
        assert_eq!(state.input_buffer, "line one\nline two");
        assert_eq!(state.focus, FocusState::Chat);
        // History recall is unsent text too: Esc does not send it.
        let mut recalled = TuiState::new(80, 24);
        recalled.multiline_mode = true;
        recalled.input_history.push("older message".into());
        handle_input(&mut recalled, &special(DomainKey::Up));
        let before = recalled.input_buffer.clone();
        let action = handle_input(&mut recalled, &special(DomainKey::Esc));
        assert!(
            !matches!(action, InputAction::SubmitMessage(_)),
            "{action:?}"
        );
        assert_eq!(recalled.input_buffer, before);
    }

    #[test]
    fn story21_2_ctrl_e_is_end_of_line_and_alt_m_toggles() {
        let mut state = TuiState::new(80, 24);
        state.input_buffer = "ab\ncd".into();
        state.cursor_position = 0;
        let before = state.multiline_mode;
        handle_input(&mut state, &special(DomainKey::CtrlE));
        assert_eq!(state.cursor_position, 2, "end of the first logical line");
        assert_eq!(state.multiline_mode, before, "Ctrl+E no longer toggles");
        handle_input(&mut state, &special(DomainKey::AltM));
        assert_ne!(state.multiline_mode, before, "Alt+M toggles");
        // Mid-line on a LATER logical line: moves to that line's end, not the buffer's.
        state.input_buffer = "ab\ncd\nef".into();
        state.cursor_position = 4; // between `c` and `d`
        handle_input(&mut state, &special(DomainKey::CtrlE));
        assert_eq!(state.cursor_position, 5, "end of the SECOND logical line");
        assert_eq!(state.input_buffer, "ab\ncd\nef", "text untouched");
    }

    #[test]
    fn story21_2_leader_enter_sends_once_while_plain_enter_inserts_newline() {
        let mut state = TuiState::new(80, 24);
        state.multiline_mode = true;
        state.input_buffer = "hello\nworld".into();
        state.cursor_position = state.input_buffer.chars().count();
        // Plain Enter in multiline mode is a newline, never a send.
        let plain = handle_input(&mut state, &special(DomainKey::Enter));
        assert_eq!(plain, InputAction::Consumed);
        assert_eq!(state.input_buffer, "hello\nworld\n");
        // Ctrl+X Enter sends exactly once and restores the Input origin.
        assert_eq!(
            handle_input(&mut state, &special(DomainKey::CtrlX)),
            InputAction::Consumed
        );
        assert!(state.pending_leader.is_some());
        let sent = handle_input(&mut state, &special(DomainKey::Enter));
        assert_eq!(sent, InputAction::SubmitMessage("hello\nworld\n".into()));
        assert!(state.pending_leader.is_none());
        assert_eq!(state.focus, FocusState::Input);
        assert!(state.input_buffer.is_empty());
        // The leader is consumed: a second Enter sends nothing.
        assert_eq!(
            handle_input(&mut state, &special(DomainKey::Enter)),
            InputAction::Consumed
        );
    }

    #[test]
    fn story21_2_leader_is_untimed_esc_restores_origin_and_unknown_is_consumed_once() {
        let mut state = TuiState::new(80, 24);
        state.focus = FocusState::Chat;
        handle_input(&mut state, &special(DomainKey::CtrlX));
        assert!(state.pending_leader.is_some());
        handle_input(&mut state, &special(DomainKey::Esc));
        assert!(state.pending_leader.is_none());
        assert_eq!(state.focus, FocusState::Chat);
        handle_input(&mut state, &special(DomainKey::CtrlX));
        assert_eq!(handle_input(&mut state, &key('z')), InputAction::Consumed);
        assert!(state.pending_leader.is_none());
        assert_eq!(state.focus, FocusState::Chat);
        // The follow-up was consumed: it did not become text or a command.
        assert!(state.input_buffer.is_empty());
        // Story 21.3 AC2: and it is announced exactly once, naming the context.
        let notices: Vec<String> = state
            .notifications
            .records()
            .map(|record| record.message.clone())
            .collect();
        assert_eq!(notices, ["No binding for Ctrl+X z in Chat"]);
    }

    #[test]
    fn story21_2_question_mark_is_literal_in_input_and_help_in_chat_or_leader() {
        let mut input = TuiState::new(80, 24);
        handle_input(&mut input, &key('?'));
        assert_eq!(input.input_buffer, "?");
        assert!(!presentation::is_open(&input, Surface::Help));
        handle_input(&mut input, &special(DomainKey::CtrlX));
        handle_input(&mut input, &key('?'));
        assert!(
            presentation::is_open(&input, Surface::Help),
            "Ctrl+X ? opens help"
        );
        handle_input(&mut input, &special(DomainKey::Esc));
        assert_eq!(input.focus, FocusState::Input, "Esc restores Input");
        assert_eq!(input.input_buffer, "?");
        let mut chat = TuiState::new(80, 24);
        chat.focus = FocusState::Chat;
        handle_input(&mut chat, &key('?'));
        assert!(presentation::is_open(&chat, Surface::Help));
    }

    #[test]
    fn story21_2_ctrl_f_cue_names_an_executable_route() {
        let mut state = TuiState::new(80, 24);
        state.input_buffer = "draft".into();
        handle_input(&mut state, &special(DomainKey::CtrlF));
        let message = state
            .notifications
            .visible()
            .last()
            .map(|record| record.message.clone())
            .expect("expected a notice cue");
        assert!(
            message.contains("Ctrl+F") && !message.contains("Esc"),
            "{message}"
        );
        // The named path really works: clear the input, then Ctrl+F opens search.
        state.input_buffer.clear();
        state.cursor_position = 0;
        assert_eq!(
            handle_input(&mut state, &special(DomainKey::CtrlF)),
            InputAction::OpenSearch
        );
    }

    #[test]
    fn story21_2_every_local_builtin_routes_to_execute_command() {
        for builtin in crate::adapters::builtin_commands::BUILTINS
            .iter()
            .filter(|b| b.local)
        {
            let mut state = TuiState::new(80, 24);
            state.input_buffer = format!("/{} ünï args", builtin.name);
            let action = submit_message_for_test(&mut state);
            let expected_args = builtin.takes_args.then(|| "ünï args".to_string());
            assert_eq!(
                action,
                InputAction::ExecuteCommand {
                    name: builtin.name.to_string(),
                    args: expected_args,
                },
                "/{} must reach its dispatcher",
                builtin.name
            );
        }
        // `/ cmd` stays literal text and an unregistered name stays a custom lookup.
        let mut literal = TuiState::new(80, 24);
        literal.input_buffer = "/ quit".into();
        assert_eq!(
            submit_message_for_test(&mut literal),
            InputAction::SubmitMessage("/ quit".into())
        );
        let mut custom = TuiState::new(80, 24);
        custom.input_buffer = "/not-a-builtin x".into();
        assert!(matches!(
            submit_message_for_test(&mut custom),
            InputAction::SubmitWithContext {
                command: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn story21_2_quit_frame_owns_y_before_a_pending_plan_card() {
        let mut state = TuiState::new(80, 24);
        state.focus = FocusState::Chat;
        plan_card(&mut state);
        let facts = QuitFacts {
            streams: 1,
            ..QuitFacts::idle(QuitHost::Local)
        };
        crate::adapters::tui::quit_confirm::request(&mut state, facts);
        // n cancels, restores the caller and leaves the plan untouched.
        assert_eq!(handle_input(&mut state, &key('n')), InputAction::Consumed);
        assert!(state.quit_confirm.is_none());
        assert!(state.decisions.holds(DecisionKind::PlanCard));
        // Re-raise: `y` is owned by S-QUIT only — never PlanCardApprove.
        crate::adapters::tui::quit_confirm::request(
            &mut state,
            QuitFacts {
                streams: 1,
                ..QuitFacts::idle(QuitHost::Local)
            },
        );
        assert_eq!(
            handle_input(&mut state, &key('y')),
            InputAction::QuitConfirmed
        );
        assert!(
            state.decisions.holds(DecisionKind::PlanCard),
            "no card was resolved"
        );
    }

    /// R9c (21.3): the open leader owns the follow-up before any card, so
    /// `Ctrl+X n` / `y` / `e` never answer a pending plan (HEAD let `n` reach it).
    #[test]
    fn story21_3_leader_follow_ups_never_answer_a_pending_plan_card() {
        for (follow_up, forbidden) in [
            ('y', InputAction::PlanCardApprove),
            ('n', InputAction::PlanCardReject),
            ('e', InputAction::PlanCardEdit),
        ] {
            let mut state = TuiState::new(80, 24);
            state.focus = FocusState::Chat;
            assert_eq!(
                handle_input(&mut state, &special(DomainKey::CtrlX)),
                InputAction::Consumed
            );
            assert!(state.pending_leader.is_some(), "the leader opens first");
            // The plan arrives under the open leader: it waits, the leader keeps the keys.
            plan_card(&mut state);
            assert!(state.pending_leader.is_some(), "the leader stays open");
            let action = handle_input(&mut state, &key(follow_up));
            assert_ne!(action, forbidden, "{follow_up}");
            assert!(
                state.pending_leader.is_none(),
                "{follow_up}: the leader closed"
            );
            assert!(
                state.decisions.holds(DecisionKind::PlanCard),
                "{follow_up}: plan kept"
            );
        }
    }
}
