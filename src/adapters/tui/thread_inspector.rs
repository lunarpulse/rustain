//! S-THREAD (Story 21.8): the host-neutral half of the thread window. One eligibility
//! rule (`actions`/`applies`) feeds both the generated rail and the key dispatch, one
//! raw-key letter table serves both hosts' pre-filters, and `on_key` is the single key
//! executor: the local host maps its outcome to an `InputAction`, the attached host to
//! its async effects. The frame itself (stack, slot, title) belongs to the presentation
//! reducer; painting is `widgets::thread_inspector`.

use crate::adapters::daemon::protocol::{ClientFrame, RecipientAction};
use crate::adapters::tui::{
    handlers::recipient_actions::action_is_pending,
    keymap::{
        self, CommandId, Context, Host, Key, Resolved, table_block::BodyCmd,
        table_overlay::OverlayCmd, table_thread::ThreadCmd,
    },
    presentation::{Surface, reducer, viewer},
    state::{ThreadAccess, ThreadInspectorState, ThreadReplyField, ThreadTarget, TuiState},
};
use crate::domain::models::{
    ChatMessage, Conversation, PeerRowControl, PeerRowStage, RetroField, RetroFieldSet,
    RetroPreview, SemanticMessageType, VerdictOffer,
};

const MAX_OUTCOME_TARGETS: usize = 32;
const MAX_OUTCOME_LINES: usize = 16;

struct OutcomeEntry {
    key: String,
    lines: std::collections::VecDeque<String>,
    last: u64,
    retained_reply: Option<String>,
}

/// The session-local outcome log (Story 21.8 A13): at most 32 targets with at most 16 lines
/// each; when full, the target with the oldest last outcome is evicted whole. A refused reply's
/// text rides in its target's entry (`retained_reply`) until the next reply field takes it.
#[derive(Default)]
pub struct ThreadOutcomes {
    entries: Vec<OutcomeEntry>,
    clock: u64,
}

impl ThreadOutcomes {
    fn entry(&mut self, key: &str) -> &mut OutcomeEntry {
        self.clock += 1;
        let at = match self.entries.iter().position(|entry| entry.key == key) {
            Some(at) => at,
            None => {
                if self.entries.len() >= MAX_OUTCOME_TARGETS
                    && let Some(oldest) = self
                        .entries
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, entry)| entry.last)
                        .map(|(at, _)| at)
                {
                    self.entries.remove(oldest);
                }
                self.entries.push(OutcomeEntry {
                    key: key.to_owned(),
                    lines: Default::default(),
                    last: 0,
                    retained_reply: None,
                });
                self.entries.len() - 1
            }
        };
        let entry = &mut self.entries[at];
        entry.last = self.clock;
        entry
    }

    /// Record one outcome line of `key`.
    pub fn push(&mut self, key: &str, line: String) {
        let entry = self.entry(key);
        if entry.lines.len() >= MAX_OUTCOME_LINES {
            entry.lines.pop_front();
        }
        entry.lines.push_back(line);
    }

    /// The outcome lines of `key`, oldest first.
    pub fn lines<'a>(&'a self, key: &str) -> impl Iterator<Item = &'a str> + 'a {
        self.entries
            .iter()
            .find(|entry| entry.key == key)
            .into_iter()
            .flat_map(|entry| entry.lines.iter().map(String::as_str))
    }

    /// Keep the text of a reply the daemon did not take, on its target.
    pub fn retain_reply(&mut self, key: &str, text: String) {
        self.entry(key).retained_reply = Some(text);
    }

    /// The retained reply text of `key`, once.
    pub fn take_retained(&mut self, key: &str) -> Option<String> {
        self.entries
            .iter_mut()
            .find(|entry| entry.key == key)
            .and_then(|entry| entry.retained_reply.take())
    }

    pub fn retained(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|entry| entry.key == key)
            .and_then(|entry| entry.retained_reply.as_deref())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The pinned response (the peer-row control) of `node`, if the conversation still has one.
pub(crate) fn pinned_response<'a>(
    conversation: &'a Conversation,
    node: &str,
) -> Option<(&'a ChatMessage, &'a PeerRowControl)> {
    conversation.messages.iter().find_map(|message| {
        let control = conversation.peer_row_controls.get(&message.id)?;
        (control.node_id == node).then_some((message, control))
    })
}

/// What a writer's key does to the opened item; which of them the item offers now is
/// `actions`. The first five legacy slots (`[Option<_>; 6]`) fix the rail order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ThreadAction {
    Respond,
    Discuss,
    Busy,
    Acknowledge,
    ShareStatus,
    DeclineStatus,
    Context,
    /// Story 19.22 `AC4(c)` — `[d] Discuss concerns` on an item that carries a story list;
    /// prefills a typed `--discuss` verdict reply.
    DiscussConcerns,
    /// Story 19.22 `AC4(c)` — `[y] Accept` when the list assigns a story to you; queues the
    /// Accept verdict frame directly.
    Accept,
    /// Story 19.25 `AC3` — share the stored retrospective preview, minus the fields left out.
    RetroShare,
    /// Story 19.25 `AC3` — toggle one preview field.
    RetroLeaveOut,
    /// Story 19.25 `AC3` — decline, indistinguishable from nothing.
    RetroDecline,
}

impl ThreadAction {
    /// The registry row this action runs under.
    pub(crate) const fn command(self) -> ThreadCmd {
        match self {
            Self::Respond => ThreadCmd::Respond,
            Self::Discuss | Self::DiscussConcerns => ThreadCmd::Discuss,
            Self::Busy => ThreadCmd::Busy,
            Self::Acknowledge => ThreadCmd::Acknowledge,
            Self::Accept => ThreadCmd::Accept,
            Self::ShareStatus | Self::RetroShare => ThreadCmd::Share,
            Self::DeclineStatus | Self::RetroDecline => ThreadCmd::Decline,
            Self::RetroLeaveOut => ThreadCmd::LeaveOut(0),
            Self::Context => ThreadCmd::Context,
        }
    }

    /// Whether `cmd` triggers this action: `[1-3] Leave out` owns three rows, one per
    /// `RetroField::ALL` entry; every other action has one.
    pub(crate) fn handles(self, cmd: ThreadCmd) -> bool {
        match (self, cmd) {
            (Self::RetroLeaveOut, ThreadCmd::LeaveOut(_)) => true,
            _ => self.command() == cmd,
        }
    }

    /// The item-derived words of the rail entry (the key comes from the registry).
    pub(crate) const fn label(self, selected: bool, redacted: bool) -> &'static str {
        match self {
            Self::Respond => "Respond",
            Self::Discuss => "Discuss",
            Self::DiscussConcerns => "Discuss concerns",
            Self::Busy => "Busy — defer 30m",
            Self::Acknowledge => "Acknowledge",
            Self::Accept => "Accept",
            Self::ShareStatus => "Share status",
            Self::RetroShare if redacted => "Share with redactions",
            Self::RetroShare => "Share",
            Self::DeclineStatus | Self::RetroDecline => "Decline",
            Self::RetroLeaveOut => "Leave out",
            Self::Context if selected => "Stop using as context",
            Self::Context => "Use as context",
        }
    }
}

/// Story 19.25 `AC3` — the manifest field a `[1-3] Leave out` digit names.
pub(crate) fn retro_field_for_key(key: char) -> Option<RetroField> {
    let index = key.to_digit(10)?.checked_sub(1)?;
    RetroField::ALL.get(usize::try_from(index).ok()?).copied()
}

/// Story 19.25 `AC3` — the persisted preview a retro item acts on: a non-removed
/// `retro_request` item whose pinned control awaits the operator with a stored preview
/// (restored cards included, the 19-24 BH-1 rule).
pub(crate) fn retro_preview<'a>(
    conversation: &'a Conversation,
    inspector: &ThreadInspectorState,
) -> Option<&'a RetroPreview> {
    if inspector.item_state == Some("removed")
        || inspector.message_type != SemanticMessageType::RetroRequest
    {
        return None;
    }
    let (_, control) = pinned_response(conversation, inspector.node_id.as_deref()?)?;
    if control.stage != PeerRowStage::AwaitingOperator {
        return None;
    }
    control.retro.as_ref()
}

/// Story 19.25 `AC3` — the preview plus this client's leave-out overlay.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RetroOverlay<'a> {
    pub(crate) preview: &'a RetroPreview,
    pub(crate) left_out: Option<&'a RetroFieldSet>,
}

/// Story 19.25 `AC3` — borrows only the conversation and the overlay map.
pub(crate) fn retro_overlay<'a>(
    conversation: &'a Conversation,
    inspector: &ThreadInspectorState,
    left_out: &'a std::collections::HashMap<String, RetroFieldSet>,
) -> Option<RetroOverlay<'a>> {
    let preview = retro_preview(conversation, inspector)?;
    let address = inspector.item_address()?;
    Some(RetroOverlay {
        preview,
        left_out: left_out.get(address.item().as_str()),
    })
}

/// Story 19.25 `AC2`/`AC3` — the preview's unwrapped text lines, shared by the pending card
/// and the thread window: header (or the nothing line), one line per field (`left out` when
/// deselected), then the `Not shared:` line with the left-out names prepended.
pub(crate) fn retro_preview_lines(
    preview: &RetroPreview,
    left_out: Option<&RetroFieldSet>,
) -> Vec<String> {
    use crate::domain::models::NOT_SHARED_RETRO;
    let mut lines = Vec::with_capacity(RetroField::ALL.len() + 2);
    let mut not_shared = String::from("Not shared: ");
    if preview.nothing_in_window {
        lines.push(
            "Shares nothing: no team messages reached this host in the last 14 days".to_owned(),
        );
    } else {
        lines.push("Shares (last 14 days):".to_owned());
        for ((index, field), text) in RetroField::ALL
            .into_iter()
            .enumerate()
            .zip(preview.contribution.text_lines())
        {
            if left_out.is_some_and(|set| set.contains(&field)) {
                lines.push(format!("[{}] {}: left out", index + 1, field.label()));
                not_shared.push_str(field.label());
                not_shared.push_str(" · ");
            } else {
                lines.push(text);
            }
        }
    }
    not_shared.push_str(
        NOT_SHARED_RETRO
            .strip_prefix("Not shared: ")
            .unwrap_or(NOT_SHARED_RETRO),
    );
    lines.push(not_shared);
    lines
}

/// Story 19.25 — rail-fact bits of the retrospective overlay: bit 23 when the preview is
/// shown, bits 24–26 per left-out `RetroField::ALL` entry.
pub(crate) const RETRO_OVERLAY_BITS: u32 = 0b1111 << 23;

pub(crate) fn retro_overlay_bits(overlay: Option<&RetroOverlay<'_>>) -> u32 {
    let Some(overlay) = overlay else {
        return 0;
    };
    RetroField::ALL
        .into_iter()
        .enumerate()
        .filter(|(_, field)| overlay.left_out.is_some_and(|set| set.contains(field)))
        .fold(1 << 23, |bits, (index, _)| bits | (1 << (24 + index)))
}

/// The pinned identity rows (never scrolled): the item id (or the response node), its type
/// token (and the peer-tainted mark once it is selected as context) and the thread root.
pub(crate) fn identity_text(inspector: &ThreadInspectorState) -> String {
    let mut heading = match &inspector.target {
        ThreadTarget::Item(address) => format!(
            "item actions · {}\n{}",
            address.item().as_str(),
            type_token(inspector.message_type),
        ),
        ThreadTarget::Node(node) => format!(
            "response actions · {node}\n{}",
            type_token(inspector.message_type),
        ),
        // The sent thread carries its identity in the first body line and the title; it
        // pins no rows of its own.
        ThreadTarget::Sent { .. } => return String::new(),
    };
    if inspector.context_selected {
        heading.push_str(" · peer-tainted");
    }
    if let Some(root) = &inspector.root_id
        && inspector
            .item_address()
            .is_some_and(|a| root != a.item().as_str())
    {
        heading.push_str("\nthread root · ");
        heading.push_str(root);
    }
    heading
}

fn type_token(message_type: SemanticMessageType) -> &'static str {
    if message_type == SemanticMessageType::Unknown {
        "unknown type"
    } else {
        crate::domain::models::semantic_message_type_metadata(message_type).token
    }
}

/// The frame's painted title: `Thread · <type token>`.
pub(crate) fn title(message_type: SemanticMessageType) -> String {
    format!("Thread · {}", type_token(message_type))
}

/// The frame's painted title on the response view of a peer node.
pub(crate) fn node_title() -> String {
    "Thread · response".to_owned()
}

/// A peer alias as painted in a title: sanitized, bounded and with bidi characters shown as
/// visible escapes (the operator typed it, but an attached request can carry anything).
fn painted_peer(peer: &str) -> String {
    use crate::domain::services::transparency::{MAX_PEER_ID_BYTES, sanitize_disclosable};
    crate::infrastructure::runtime::recipient_view::escape_disclosable(&sanitize_disclosable(
        peer,
        MAX_PEER_ID_BYTES,
    ))
}

/// The frame's painted title on the thread of an item this host sent.
pub(crate) fn sent_title(peer: &str) -> String {
    format!("Thread · sent · {}", painted_peer(peer))
}

/// Whether the thread's reply field holds text that quitting would lose.
pub(crate) fn reply_field_has_text(state: &TuiState) -> bool {
    state
        .thread_inspector
        .as_ref()
        .and_then(|inspector| inspector.reply.as_ref())
        .is_some_and(|reply| !reply.field.is_empty())
}

/// Which writer actions the opened item offers now, in rail order (slot order): none unless
/// the frame is an attached writer's and the conversation is the writer's own. This is the
/// one eligibility rule: the rail paints exactly these and the dispatch runs exactly these.
pub(crate) fn actions(
    state: &TuiState,
    conversation: Option<&Conversation>,
) -> [Option<ThreadAction>; 6] {
    let mut actions = [None; 6];
    let (Some(inspector), Some(conversation)) = (state.thread_inspector.as_ref(), conversation)
    else {
        return actions;
    };
    // A node view offers no draft decision of its own (21-6's decision frame is the one route).
    if inspector.access != ThreadAccess::Writer || inspector.item_address().is_none() {
        return actions;
    }
    let control = inspector
        .node_id
        .as_deref()
        .and_then(|node| pinned_response(conversation, node))
        .map(|(_, control)| control);
    let live_drafting =
        control.is_some_and(|c| c.node_resolvable && c.stage == PeerRowStage::Drafting);
    let present = inspector.item_state != Some("removed");
    if present {
        if !live_drafting {
            // Story 19.22 `AC4(c)` — on a list item a verdict is offered in place of
            // Respond; every other type and stage is unchanged.
            actions[0] = if inspector.verdict_offer != VerdictOffer::None {
                Some(ThreadAction::DiscussConcerns)
            } else {
                match inspector.message_type {
                    SemanticMessageType::ArchitectureUpdate | SemanticMessageType::BugReport => {
                        Some(ThreadAction::Discuss)
                    }
                    SemanticMessageType::Consultation => Some(ThreadAction::Respond),
                    _ if control.is_some_and(|c| c.stage != PeerRowStage::Settled) => {
                        Some(ThreadAction::Respond)
                    }
                    _ => None,
                }
            };
            // Ready keeps its draft-review decision in 21-6's frame, including on an
            // orphaned control that is no longer resolvable.
            if inspector.verdict_offer == VerdictOffer::AcceptOrDiscuss
                && !control.is_some_and(|c| c.stage == PeerRowStage::Ready)
            {
                actions[2] = Some(ThreadAction::Accept);
            }
        }
        actions[1] = match inspector.message_type {
            SemanticMessageType::Consultation => Some(ThreadAction::Busy),
            // Story 19.21 `C5` — `UX-DR-TM-01:111` ratified `[✓] Confirm → [Acknowledge]`
            // for a story assignment; the same `/team ack` frame, the same correlated
            // Applied/Idempotent result, and the daemon's transition gate rechecks it.
            // A receipt, ⛔ not an acceptance and not a person acting.
            SemanticMessageType::ArchitectureUpdate
            // Story 19.22 `AC4(c)` — an Act 6 scope change is acknowledged like any
            // received notice; a receipt, never an acceptance.
            | SemanticMessageType::ScopeChange
            | SemanticMessageType::DesignUpdate
            | SemanticMessageType::StoryAssignment
            | SemanticMessageType::BugReport
                if inspector.item_state == Some("received") =>
            {
                Some(ThreadAction::Acknowledge)
            }
            _ => None,
        };
    }
    // Like `[r] Respond`, the status acts survive a restart: without a live card the
    // daemon appends the stored preview directly (orphan arm).
    if present
        && inspector.message_type == SemanticMessageType::StatusRequest
        && control.is_some_and(|c| c.stage == PeerRowStage::AwaitingOperator && c.status.is_some())
    {
        actions[1] = Some(ThreadAction::Busy);
        actions[2] = Some(ThreadAction::ShareStatus);
        actions[4] = Some(ThreadAction::DeclineStatus);
    }
    // Story 19.25 `AC3` — the same rule for the retrospective preview.
    if retro_preview(conversation, inspector).is_some() {
        actions[1] = Some(ThreadAction::Busy);
        actions[2] = Some(ThreadAction::RetroShare);
        actions[3] = Some(ThreadAction::RetroLeaveOut);
        actions[4] = Some(ThreadAction::RetroDecline);
    }
    if inspector.context_available || inspector.context_selected {
        actions[5] = Some(ThreadAction::Context);
    }
    for slot in &mut actions {
        let Some(action) = *slot else {
            continue;
        };
        let wire_action = match action {
            ThreadAction::Busy => RecipientAction::Defer,
            ThreadAction::Acknowledge => RecipientAction::Acknowledge,
            ThreadAction::ShareStatus | ThreadAction::DeclineStatus => RecipientAction::ShareStatus,
            // Toggles are suppressed too: the overlay never changes while the act built
            // from it is in flight.
            ThreadAction::RetroShare | ThreadAction::RetroLeaveOut | ThreadAction::RetroDecline => {
                RecipientAction::ShareRetro
            }
            ThreadAction::Context if inspector.context_selected => RecipientAction::ContextRemove,
            ThreadAction::Context => RecipientAction::ContextSelect,
            // Story 19.22 — verdict acts are replies too: same suppression key.
            ThreadAction::Respond
            | ThreadAction::Discuss
            | ThreadAction::DiscussConcerns
            | ThreadAction::Accept => RecipientAction::Reply,
        };
        if action_is_pending(
            state,
            wire_action,
            &inspector.target,
            inspector.node_id.as_deref(),
        ) || (action == ThreadAction::Context
            && action_is_pending(
                state,
                if inspector.context_selected {
                    RecipientAction::ContextSelect
                } else {
                    RecipientAction::ContextRemove
                },
                &inspector.target,
                inspector.node_id.as_deref(),
            ))
        {
            *slot = None;
        }
    }
    actions
}

/// The action `cmd` runs on the opened item now, if any.
pub(crate) fn action_for(
    state: &TuiState,
    conversation: Option<&Conversation>,
    cmd: ThreadCmd,
) -> Option<ThreadAction> {
    // A writer key whose hint the last paint did not show whole is inert (rail truth).
    if state
        .thread_inspector
        .as_ref()
        .is_none_or(|inspector| !inspector.rail_live.get().0)
    {
        return None;
    }
    actions(state, conversation)
        .into_iter()
        .flatten()
        .find(|action| action.handles(cmd))
}

fn page_rail_live(state: &TuiState) -> bool {
    state
        .thread_inspector
        .as_ref()
        .is_some_and(|inspector| inspector.rail_live.get().1)
}

/// What `x` retracts, decided by facts: the opened sent item (through the confirm-time read
/// of `/team retract <peer> <item>`) or the newest auto-sent reply of the opened received item
/// (through the retract confirmation of that one message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RetractPlan {
    Item { peer: String, item: String },
    Reply { message_id: String, at: String },
}

impl RetractPlan {
    /// The item-derived words of the rail entry (the key comes from the registry).
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Item { .. } => "Retract".to_owned(),
            Self::Reply { at, .. } => format!("Retract auto-sent reply {at}"),
        }
    }
}

/// `HH:MM` (UTC) of a message time in seconds, from the time of day.
fn clock_label(created_at: i64) -> String {
    let seconds = created_at.rem_euclid(86_400);
    format!("{:02}:{:02}", seconds / 3_600, (seconds % 3_600) / 60)
}

/// The auto-sent reply `conversation` can still retract for the received item `address`
/// (the newest one): a row `retractable_rows` lists (automatic, unretracted, settled) that
/// answers this item, by its peer-row control or by its node.
fn newest_retractable_reply(
    conversation: &Conversation,
    address: &crate::domain::models::ItemAddress,
    node: Option<&str>,
) -> Option<RetractPlan> {
    crate::infrastructure::runtime::attach_loop::retractable_rows(conversation)
        .find(|row| {
            conversation
                .peer_row_controls
                .get(&row.id)
                .is_some_and(|control| control.item_address.as_ref() == Some(address))
                || node.is_some_and(|node| row.id.strip_prefix("peer-response-") == Some(node))
        })
        .map(|row| RetractPlan::Reply {
            message_id: row.id.clone(),
            at: clock_label(row.created_at),
        })
}

/// What `x` would retract now (Story 21.8 A14), `None` when nothing is eligible. Writers only
/// (an observer or a local reader never paints it); a badge alone is never a fact: the sent
/// item must be this host's own per the completed read and not yet retracted, the reply must
/// be a row `retractable_rows` lists for the attached conversation.
pub(crate) fn retract_plan(
    state: &TuiState,
    conversation: Option<&Conversation>,
) -> Option<RetractPlan> {
    let inspector = state.thread_inspector.as_ref()?;
    if inspector.access != ThreadAccess::Writer || state.local_read_only {
        return None;
    }
    match &inspector.target {
        ThreadTarget::Sent { peer, item } => {
            let sent = inspector.sent.as_ref()?;
            (!sent.loading && sent.mine && !sent.retracted).then(|| RetractPlan::Item {
                peer: peer.clone(),
                item: item.clone(),
            })
        }
        ThreadTarget::Item(address) => {
            newest_retractable_reply(conversation?, address, inspector.node_id.as_deref())
        }
        ThreadTarget::Node(_) => None,
    }
}

/// The queued 21-6 draft decision of the node the response view shows, if the decision queue
/// would open it now (`decision_queue::can_open`: queued, decidable on this tab, no text field
/// or quit guard on top). Never `holds`: a decision of another tab or a stale one is not offered.
pub(crate) fn open_decision_key(
    state: &TuiState,
    conversation: Option<&Conversation>,
) -> Option<crate::adapters::tui::decision_queue::DecisionKey> {
    use crate::adapters::tui::decision_queue::{self, DecisionKey, DecisionKind};
    let inspector = state.thread_inspector.as_ref()?;
    let ThreadTarget::Node(node) = &inspector.target else {
        return None;
    };
    if inspector.access != ThreadAccess::Writer {
        return None;
    }
    let key = DecisionKey {
        kind: DecisionKind::RecipientDraftApproval,
        source: node.clone(),
        conversation_id: Some(conversation?.id.clone()),
    };
    // A stale draft decision (its response left) is no decision to open.
    (state
        .decisions
        .item(&key)
        .is_some_and(|item| !item.is_stale())
        && decision_queue::can_open(state, &key))
    .then_some(key)
}

/// Whether the painted rail offers `cmd` by facts alone (the last paint's fit decides whether
/// the key is also live: `applies`).
pub(crate) fn offered(
    state: &TuiState,
    conversation: Option<&Conversation>,
    cmd: ThreadCmd,
) -> bool {
    let Some(inspector) = state.thread_inspector.as_ref() else {
        return false;
    };
    match cmd {
        ThreadCmd::Back
        | ThreadCmd::SendReply
        | ThreadCmd::ReplyBack
        | ThreadCmd::LeaveViewport => true,
        ThreadCmd::Earlier => inspector.omitted_before > 0,
        ThreadCmd::Newer => inspector.omitted_after > 0,
        ThreadCmd::Copy => true,
        ThreadCmd::Toggle => inspector.can_toggle && inspector.access != ThreadAccess::LocalOnly,
        ThreadCmd::OpenDecision => open_decision_key(state, conversation).is_some(),
        ThreadCmd::Viewport => inspector.wide_fit.get().0 > 0,
        ThreadCmd::Retract => retract_plan(state, conversation).is_some(),
        _ => actions(state, conversation)
            .into_iter()
            .flatten()
            .any(|action| action.handles(cmd)),
    }
}

/// Whether `cmd` can act on the thread now (S-THREAD's `applies`): every key of the page rail
/// needs its fact and the rail painted whole (rail truth == dispatch), writer rows need an
/// offered action. An inapplicable row is never painted and its key is a consumed no-op.
pub(crate) fn applies(
    state: &TuiState,
    conversation: Option<&Conversation>,
    cmd: ThreadCmd,
) -> bool {
    let Some(inspector) = state.thread_inspector.as_ref() else {
        return false;
    };
    match cmd {
        ThreadCmd::Back
        | ThreadCmd::SendReply
        | ThreadCmd::ReplyBack
        | ThreadCmd::LeaveViewport => true,
        ThreadCmd::Earlier
        | ThreadCmd::Newer
        | ThreadCmd::Copy
        | ThreadCmd::Toggle
        | ThreadCmd::OpenDecision
        | ThreadCmd::Viewport => inspector.rail_live.get().1 && offered(state, conversation, cmd),
        // `x` is painted in the writer rail section: live when that section is.
        ThreadCmd::Retract => {
            inspector.rail_live.get().0 && offered(state, conversation, ThreadCmd::Retract)
        }
        _ => action_for(state, conversation, cmd).is_some(),
    }
}

/// What a key asks of the host once the shared executor has applied its state-only part.
#[derive(Debug)]
pub(crate) enum ThreadOutcome {
    Consumed,
    /// Load the earlier (or later) bounded page.
    Page {
        earlier: bool,
    },
    /// `Tab`: show the other view (item ⇄ response) of the same exchange.
    Toggle,
    /// A correlated writer frame to queue. `joined` carries the node and item whose journal
    /// join the host checks first (the Busy fast path reuses the durable principal/task join).
    Send {
        frame: ClientFrame,
        joined: Option<(String, crate::domain::models::ItemAddress)>,
    },
    /// `x`: raise the retract confirmation of this plan (21-6's decision on L3).
    Retract(RetractPlan),
}

/// The key context of the thread frame now: the reply field and the filter field are text
/// contexts, the block viewport swaps the thread rows for the horizontal ones.
pub(crate) fn frame_context(state: &TuiState) -> Context {
    match state.thread_inspector.as_ref() {
        Some(inspector) if inspector.reply.is_some() => Context::ThreadReply,
        Some(inspector) if inspector.search.editing => Context::InspectorSearch,
        Some(inspector) if inspector.viewport => Context::BodyViewport,
        _ => Context::ThreadInspector,
    }
}

/// The one key executor of the top thread frame: resolves `key` in the frame's own context
/// (`ThreadInspector` at the root, `BodyViewport` inside the block viewport) for `host`, then
/// applies it. Unbound and inapplicable keys are consumed (never the composer, the Stage or a
/// card).
pub(crate) fn on_key(
    state: &mut TuiState,
    key: Key,
    host: Host,
    conversation: Option<&Conversation>,
) -> ThreadOutcome {
    if state
        .modal_stack
        .top()
        .is_none_or(|frame| frame.surface != Surface::ThreadInspector)
    {
        return ThreadOutcome::Consumed;
    }
    let context = frame_context(state);
    let Resolved::Hit(row) = keymap::resolve(context, host, &[key]) else {
        return ThreadOutcome::Consumed;
    };
    state.needs_redraw = true;
    match row.command {
        CommandId::Thread(ThreadCmd::Back) => {
            // The first `Esc` clears a kept filter, the next backs the frame.
            if state
                .thread_inspector
                .as_ref()
                .is_some_and(|inspector| !inspector.search.query.is_empty())
            {
                if let Some(inspector) = state.thread_inspector.as_mut() {
                    inspector.search.back();
                }
                search_changed(state);
            } else {
                reducer::back(state);
                crate::adapters::tui::handlers::recipient_actions::refresh_action_notices(
                    state, false,
                );
            }
            ThreadOutcome::Consumed
        }
        CommandId::Thread(ThreadCmd::LeaveViewport) => {
            if let Some(inspector) = state.thread_inspector.as_mut() {
                inspector.viewport = false;
                inspector.viewer.h_off = 0;
            }
            ThreadOutcome::Consumed
        }
        CommandId::Viewer(command) if !command.horizontal() => {
            if let Some(inspector) = state.thread_inspector.as_mut() {
                let (rows, viewport) = inspector.fit.get();
                // A pending jump-to-end (`follow`, set by `G`) is resolved against the live
                // fit, so relative moves start from the real bottom (8B-02).
                if inspector.viewer.follow {
                    inspector.viewer.top = usize::MAX;
                }
                viewer::apply(&mut inspector.viewer, command, rows, viewport);
            }
            ThreadOutcome::Consumed
        }
        // Sideways movement exists only inside the block viewport: `h/l/0/$` move the wide
        // blocks' shared offset there and nothing at the root.
        CommandId::Viewer(command) => {
            if context == Context::BodyViewport
                && let Some(inspector) = state.thread_inspector.as_mut()
            {
                let (widest, cols) = inspector.wide_fit.get();
                viewer::apply_horizontal(&mut inspector.viewer, command, widest, cols);
            }
            ThreadOutcome::Consumed
        }
        CommandId::Body(BodyCmd::ToggleMode) => {
            if context == Context::ThreadInspector
                && page_rail_live(state)
                && let Some(inspector) = state.thread_inspector.as_mut()
            {
                inspector.mode = inspector.mode.toggled();
                inspector.invalidate();
            }
            ThreadOutcome::Consumed
        }
        CommandId::Overlay(OverlayCmd::InspectorSearch) => {
            if page_rail_live(state)
                && let Some(inspector) = state.thread_inspector.as_mut()
            {
                inspector.search.open();
            }
            ThreadOutcome::Consumed
        }
        CommandId::Thread(ThreadCmd::Viewport) => {
            if applies(state, conversation, ThreadCmd::Viewport)
                && let Some(inspector) = state.thread_inspector.as_mut()
            {
                inspector.viewport = true;
            }
            ThreadOutcome::Consumed
        }
        CommandId::Thread(ThreadCmd::Copy) => {
            if applies(state, conversation, ThreadCmd::Copy) {
                copy_original(state);
            }
            ThreadOutcome::Consumed
        }
        CommandId::Thread(ThreadCmd::OpenDecision) => {
            if applies(state, conversation, ThreadCmd::OpenDecision)
                && let Some(key) = open_decision_key(state, conversation)
            {
                crate::adapters::tui::decision_queue::open(state, &key);
            }
            ThreadOutcome::Consumed
        }
        CommandId::Thread(ThreadCmd::Retract) => {
            match applies(state, conversation, ThreadCmd::Retract)
                .then(|| retract_plan(state, conversation))
                .flatten()
            {
                Some(plan) => ThreadOutcome::Retract(plan),
                None => ThreadOutcome::Consumed,
            }
        }
        CommandId::Thread(ThreadCmd::Toggle) => {
            if applies(state, conversation, ThreadCmd::Toggle) {
                ThreadOutcome::Toggle
            } else {
                ThreadOutcome::Consumed
            }
        }
        CommandId::Thread(cmd @ (ThreadCmd::Earlier | ThreadCmd::Newer)) => {
            if applies(state, conversation, cmd) {
                ThreadOutcome::Page {
                    earlier: cmd == ThreadCmd::Earlier,
                }
            } else {
                ThreadOutcome::Consumed
            }
        }
        CommandId::Thread(cmd) => match action_for(state, conversation, cmd) {
            Some(action) => run_action(state, action, key),
            None => ThreadOutcome::Consumed,
        },
        _ => ThreadOutcome::Consumed,
    }
}

/// A wheel step scrolls the body, clamped against the last painted rows.
pub(crate) fn on_wheel(
    state: &mut TuiState,
    delta: crate::domain::models::view_state::ScrollDelta,
) {
    if let Some(inspector) = state.thread_inspector.as_mut() {
        let (rows, viewport) = inspector.fit.get();
        // A pending jump-to-end (`follow`, set by Bottom) is resolved against the live fit.
        if inspector.viewer.follow {
            inspector.viewer.top = usize::MAX;
        }
        viewer::apply_scroll(&mut inspector.viewer, delta, rows, viewport);
        state.needs_redraw = true;
    }
}

fn run_action(state: &mut TuiState, action: ThreadAction, key: Key) -> ThreadOutcome {
    let Some(inspector) = state.thread_inspector.as_ref() else {
        return ThreadOutcome::Consumed;
    };
    let Some(address) = inspector.item_address() else {
        return ThreadOutcome::Consumed;
    };
    let item_id = address.item().as_str().to_owned();
    let send = |frame| ThreadOutcome::Send {
        frame,
        joined: None,
    };
    match action {
        ThreadAction::Respond | ThreadAction::Discuss | ThreadAction::DiscussConcerns => {
            open_reply(state, action == ThreadAction::DiscussConcerns);
            ThreadOutcome::Consumed
        }
        // Story 19.22 `AC4(c)` — `[y] Accept` queues the verdict frame directly: empty text
        // (the daemon normalizes `Accepted.`) and an Accept verdict, through the same
        // correlated Reply slot as a typed reply.
        ThreadAction::Accept => send(ClientFrame::ReplyToRecipientItem {
            item_id,
            text: String::new(),
            verdict: Some(crate::adapters::daemon::protocol::ReplyVerdict::Accept),
        }),
        ThreadAction::Busy => ThreadOutcome::Send {
            frame: ClientFrame::DeferRecipientItem { item_id },
            joined: inspector
                .node_id
                .clone()
                .map(|node| (node, address.clone())),
        },
        ThreadAction::Acknowledge => send(ClientFrame::AcknowledgeRecipientItem { item_id }),
        ThreadAction::ShareStatus | ThreadAction::DeclineStatus => {
            send(ClientFrame::ShareRecipientStatus {
                item_id,
                decline: action == ThreadAction::DeclineStatus,
            })
        }
        // Story 19.25 `AC3` — Share sends the stored overlay as its leave-out list; Decline
        // sends no fields (the daemon refuses decline + fields).
        ThreadAction::RetroShare | ThreadAction::RetroDecline => {
            let decline = action == ThreadAction::RetroDecline;
            let leave_out = if decline {
                Vec::new()
            } else {
                state
                    .retro_left_out
                    .get(&item_id)
                    .map(|set| set.iter().copied().collect())
                    .unwrap_or_default()
            };
            send(ClientFrame::ShareRecipientRetro {
                item_id,
                leave_out,
                decline,
            })
        }
        // Story 19.25 `AC3` — a digit toggles one field of this client's overlay; the
        // window re-renders from it.
        ThreadAction::RetroLeaveOut => {
            let Key::Char(digit) = key else {
                return ThreadOutcome::Consumed;
            };
            let Some(field) = retro_field_for_key(digit) else {
                return ThreadOutcome::Consumed;
            };
            let set = state.retro_left_out.entry(item_id.clone()).or_default();
            if !set.insert(field) {
                set.remove(&field);
            }
            if set.is_empty() {
                state.retro_left_out.remove(&item_id);
            }
            ThreadOutcome::Consumed
        }
        ThreadAction::Context => send(ClientFrame::SetRecipientContext {
            item_id,
            selected: !inspector.context_selected,
        }),
    }
}

/// `r`/`d`: open the frame-owned reply field on the opened item (the composer is never read
/// or written). A reply the daemon refused earlier pre-fills it.
fn open_reply(state: &mut TuiState, discuss: bool) {
    let Some(inspector) = state.thread_inspector.as_ref() else {
        return;
    };
    let key = inspector.outcome_key();
    let retained = state.thread_outcomes.take_retained(&key);
    let Some(inspector) = state.thread_inspector.as_mut() else {
        return;
    };
    let mut reply = ThreadReplyField {
        discuss,
        ..ThreadReplyField::default()
    };
    if let Some(text) = retained {
        reply.field.set(&text);
    }
    inspector.reply = Some(reply);
    inspector.invalidate();
    state.needs_redraw = true;
}

/// What the reply field's `Enter` asks of the attached host.
#[derive(Debug)]
pub(crate) enum ReplyOutcome {
    Consumed,
    /// Send this reply: the host runs it through the `/team reply` parse and dispatch.
    Submit {
        item_id: String,
        text: String,
        discuss: bool,
    },
}

/// A key of the reply field that is not text editing: `Enter` submits, `Esc`/`Ctrl+C` clear
/// the field and leave it (the next `Esc` backs the frame).
pub(crate) fn on_reply_key(state: &mut TuiState, key: Key) -> ReplyOutcome {
    let Resolved::Hit(row) = keymap::resolve(Context::ThreadReply, Host::Attached, &[key]) else {
        return ReplyOutcome::Consumed;
    };
    state.needs_redraw = true;
    match row.command {
        CommandId::Thread(ThreadCmd::ReplyBack) => {
            if let Some(inspector) = state.thread_inspector.as_mut() {
                inspector.reply = None;
                inspector.invalidate();
            }
            ReplyOutcome::Consumed
        }
        CommandId::Thread(ThreadCmd::SendReply) => {
            let Some(inspector) = state.thread_inspector.as_ref() else {
                return ReplyOutcome::Consumed;
            };
            let (Some(reply), Some(address)) = (inspector.reply.as_ref(), inspector.item_address())
            else {
                return ReplyOutcome::Consumed;
            };
            let text = reply.field.as_str();
            if text.trim().is_empty() {
                return ReplyOutcome::Consumed;
            }
            // The bound is checked on the field before any command line is built.
            if text.len() > crate::domain::models::MAX_REPLY_BYTES {
                crate::adapters::tui::handlers::recipient_actions::show_notice(
                    state,
                    None,
                    format!(
                        "reply is longer than {} KiB — shorten it before sending",
                        crate::domain::models::MAX_REPLY_BYTES / 1024
                    ),
                );
                return ReplyOutcome::Consumed;
            }
            ReplyOutcome::Submit {
                item_id: address.item().as_str().to_owned(),
                text: text.to_owned(),
                discuss: reply.discuss,
            }
        }
        _ => ReplyOutcome::Consumed,
    }
}

/// The reply was dispatched: the field and the frame leave, back to the base exactly as it was.
pub(crate) fn reply_sent(state: &mut TuiState) {
    reducer::close(state, Surface::ThreadInspector);
    state.needs_redraw = true;
}

/// `c`: copy the opened item's original body (never the escaped display, never turn rows)
/// through the injectable copier. The bound is checked on the source length before anything
/// is copied; nothing available is a notice, never an empty copy.
fn copy_original(state: &mut TuiState) {
    use crate::adapters::tui::handlers::clipboard::{notify_copy, notify_failure};
    use crate::adapters::tui::notifications::{self, NoticeSource};
    let copier = state.clipboard_copier.clone();
    let source = state
        .thread_inspector
        .as_mut()
        .and_then(|inspector| inspector.original.take());
    match &source {
        None => notifications::info(state, NoticeSource::Clipboard, "Nothing to copy"),
        Some(text) if text.len() > crate::adapters::tui::tool_inspector::source::COPY_MAX_BYTES => {
            notify_failure(state, "item body exceeds the 8 MiB copy limit");
        }
        Some(text) => match copier {
            Some(copier) => notify_copy(state, text, &*copier),
            None => notify_copy(
                state,
                text,
                &crate::adapters::tui::clipboard::copy_to_clipboard,
            ),
        },
    }
    if let Some(inspector) = state.thread_inspector.as_mut() {
        inspector.original = source;
    }
}

/// The filter was edited or cleared: the body rows depend on it.
pub(crate) fn search_changed(state: &mut TuiState) {
    if let Some(inspector) = state.thread_inspector.as_mut() {
        inspector.invalidate();
        inspector.viewer.top = 0;
    }
    state.needs_redraw = true;
}

/// `Enter` in the filter field keeps the filter; a query that matches nothing says so and
/// leaves the view unchanged.
pub(crate) fn accept_search(state: &mut TuiState) {
    let Some(inspector) = state.thread_inspector.as_mut() else {
        return;
    };
    inspector.search.accept();
    if !crate::adapters::tui::thread_body::has_match(&inspector.segments, &inspector.search) {
        let query = inspector.search.query.as_str().to_owned();
        inspector.search.back();
        crate::adapters::tui::notifications::info(
            state,
            crate::adapters::tui::notifications::NoticeSource::Search,
            format!("Search: no match for \"{query}\""),
        );
    }
    search_changed(state);
}

/// The writer action letters (and `o`): a Repeat, a Release or a modified variant of one
/// never acts.
const WRITER_LETTERS: &[char] = &[
    'r', 'd', 'b', 'a', 'y', 's', 'n', '1', '2', '3', 'u', 'o', 'x',
];

/// The raw press-only rule of the thread frame (PC-14): conversion discards kind and
/// modifiers, so each host's raw pre-filter asks this before converting. Only the writer
/// action letters are guarded and only at the root of the frame (its text fields and the block
/// viewport take Shift text and readline chords); registered chords (`Ctrl+X`, `Ctrl+P`,
/// `Ctrl+Q`, `Ctrl+C`, `Ctrl+D`, `Ctrl+U`) and shifted printables (`G`, `$`) resolve normally.
pub(crate) fn ignores_raw_key(state: &TuiState, key: &crossterm::event::KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
    if state
        .modal_stack
        .top()
        .is_none_or(|frame| frame.surface != Surface::ThreadInspector)
        || frame_context(state) != Context::ThreadInspector
    {
        return false;
    }
    let KeyCode::Char(c) = key.code else {
        return false;
    };
    if !WRITER_LETTERS.contains(&c.to_ascii_lowercase()) {
        return false;
    }
    if key.kind != KeyEventKind::Press {
        return true;
    }
    let registered_chord = key.modifiers == KeyModifiers::CONTROL && matches!(c, 'd' | 'u' | 'x');
    !key.modifiers.is_empty() && !registered_chord
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::daemon::protocol::RecipientActionTarget;
    use crate::adapters::tui::state::RecipientActionState;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    const NODE: &str = "node-1";
    const ALL: [ThreadAction; 12] = [
        ThreadAction::Respond,
        ThreadAction::Discuss,
        ThreadAction::Busy,
        ThreadAction::Acknowledge,
        ThreadAction::ShareStatus,
        ThreadAction::DeclineStatus,
        ThreadAction::Context,
        ThreadAction::DiscussConcerns,
        ThreadAction::Accept,
        ThreadAction::RetroShare,
        ThreadAction::RetroLeaveOut,
        ThreadAction::RetroDecline,
    ];

    /// An exhaustive match: adding or removing a variant fails to compile here, so `ALL`
    /// (and the node-only `Approve`/`Edit`/`Reject`/`Decline`-on-node absence) stays honest.
    fn variant_name(action: ThreadAction) -> &'static str {
        match action {
            ThreadAction::Respond => "Respond",
            ThreadAction::Discuss => "Discuss",
            ThreadAction::Busy => "Busy",
            ThreadAction::Acknowledge => "Acknowledge",
            ThreadAction::ShareStatus => "ShareStatus",
            ThreadAction::DeclineStatus => "DeclineStatus",
            ThreadAction::Context => "Context",
            ThreadAction::DiscussConcerns => "DiscussConcerns",
            ThreadAction::Accept => "Accept",
            ThreadAction::RetroShare => "RetroShare",
            ThreadAction::RetroLeaveOut => "RetroLeaveOut",
            ThreadAction::RetroDecline => "RetroDecline",
        }
    }

    fn item_address() -> crate::domain::models::ItemAddress {
        crate::domain::models::ItemAddress::from_a2a_ingress(
            crate::domain::models::PeerId::from_public_key(&[5; 32]).unwrap(),
            crate::domain::models::ItemId::from_replay("item-1"),
        )
    }

    fn item_id() -> String {
        item_address().item().as_str().to_owned()
    }

    /// A state whose only frame is a thread window opened through the reducer.
    fn thread_state(
        access: ThreadAccess,
        message_type: SemanticMessageType,
        item_state: Option<&'static str>,
    ) -> TuiState {
        let mut state = TuiState::new(80, 24);
        let mut inspector =
            ThreadInspectorState::new(ThreadTarget::Item(item_address()), access, message_type);
        inspector.node_id = Some(NODE.to_owned());
        inspector.item_state = item_state;
        reducer::open_thread(&mut state, inspector, title(message_type));
        state
    }

    fn inspector(state: &mut TuiState) -> &mut ThreadInspectorState {
        state.thread_inspector.as_mut().unwrap()
    }

    /// A conversation holding the pinned response row of `NODE`, if `control` is given.
    fn conversation(
        control: Option<(PeerRowStage, bool)>,
        message_type: SemanticMessageType,
        status: bool,
        retro: bool,
    ) -> Conversation {
        let mut conversation = Conversation::default();
        let Some((stage, node_resolvable)) = control else {
            return conversation;
        };
        let message = ChatMessage {
            id: "row-1".into(),
            content: "peer text".into(),
            ..Default::default()
        };
        conversation.peer_row_controls.insert(
            message.id.clone(),
            PeerRowControl {
                node_id: NODE.into(),
                item_address: Some(item_address()),
                stories: None,
                status: status.then(|| {
                    crate::domain::models::StatusPreview::new(
                        crate::domain::models::StatusDisclosure::Minimal,
                        &[],
                    )
                }),
                retro: retro.then(|| RetroPreview {
                    contribution: crate::domain::models::RetroContribution::default(),
                    nothing_in_window: false,
                }),
                selected_for: None,
                message_type,
                sender_label: "peer".into(),
                stage,
                node_resolvable,
            },
        );
        conversation.messages.push(message);
        conversation
    }

    fn offered(state: &TuiState, conversation: &Conversation) -> [Option<ThreadAction>; 6] {
        actions(state, Some(conversation))
    }

    fn pend(state: &mut TuiState, action: RecipientAction, item: &str, composed: Option<&str>) {
        state.recipient_actions.insert(
            (
                action,
                RecipientActionTarget::Item {
                    item_id: item.to_owned(),
                },
            ),
            RecipientActionState::Pending {
                composed_input: composed.map(str::to_owned),
                reply: None,
            },
        );
    }

    #[test]
    fn story21_8_thread_action_command_and_handles_follow_the_registry_rows() {
        for (action, command) in [
            (ThreadAction::Respond, ThreadCmd::Respond),
            (ThreadAction::Discuss, ThreadCmd::Discuss),
            (ThreadAction::DiscussConcerns, ThreadCmd::Discuss),
            (ThreadAction::Busy, ThreadCmd::Busy),
            (ThreadAction::Acknowledge, ThreadCmd::Acknowledge),
            (ThreadAction::Accept, ThreadCmd::Accept),
            (ThreadAction::ShareStatus, ThreadCmd::Share),
            (ThreadAction::RetroShare, ThreadCmd::Share),
            (ThreadAction::DeclineStatus, ThreadCmd::Decline),
            (ThreadAction::RetroDecline, ThreadCmd::Decline),
            (ThreadAction::RetroLeaveOut, ThreadCmd::LeaveOut(0)),
            (ThreadAction::Context, ThreadCmd::Context),
        ] {
            assert_eq!(action.command(), command, "{}", variant_name(action));
            assert!(action.handles(command), "{}", variant_name(action));
        }
        // `[1-3] Leave out` owns three rows; nothing else owns any of them.
        for index in 0..3 {
            for action in ALL {
                assert_eq!(
                    action.handles(ThreadCmd::LeaveOut(index)),
                    action == ThreadAction::RetroLeaveOut,
                    "{} LeaveOut({index})",
                    variant_name(action)
                );
            }
        }
        // Distinct rows never cross-trigger (positive controls above).
        assert!(!ThreadAction::Respond.handles(ThreadCmd::Discuss));
        assert!(!ThreadAction::Busy.handles(ThreadCmd::Acknowledge));
        assert!(!ThreadAction::ShareStatus.handles(ThreadCmd::Decline));
        assert!(!ThreadAction::RetroDecline.handles(ThreadCmd::Share));
        assert!(!ThreadAction::Context.handles(ThreadCmd::Back));
    }

    #[test]
    fn story21_8_thread_action_labels_are_exact() {
        for (action, label) in [
            (ThreadAction::Respond, "Respond"),
            (ThreadAction::Discuss, "Discuss"),
            (ThreadAction::DiscussConcerns, "Discuss concerns"),
            (ThreadAction::Busy, "Busy — defer 30m"),
            (ThreadAction::Acknowledge, "Acknowledge"),
            (ThreadAction::Accept, "Accept"),
            (ThreadAction::ShareStatus, "Share status"),
            (ThreadAction::DeclineStatus, "Decline"),
            (ThreadAction::RetroDecline, "Decline"),
            (ThreadAction::RetroLeaveOut, "Leave out"),
        ] {
            for (selected, redacted) in [(false, false), (true, false), (false, true), (true, true)]
            {
                assert_eq!(
                    action.label(selected, redacted),
                    label,
                    "{} ({selected}, {redacted})",
                    variant_name(action)
                );
            }
        }
        assert_eq!(ThreadAction::RetroShare.label(false, false), "Share");
        assert_eq!(ThreadAction::RetroShare.label(true, false), "Share");
        assert_eq!(
            ThreadAction::RetroShare.label(false, true),
            "Share with redactions"
        );
        assert_eq!(ThreadAction::Context.label(false, false), "Use as context");
        assert_eq!(ThreadAction::Context.label(false, true), "Use as context");
        assert_eq!(
            ThreadAction::Context.label(true, false),
            "Stop using as context"
        );
    }

    #[test]
    fn story21_8_title_names_the_message_type() {
        assert_eq!(title(SemanticMessageType::Unknown), "Thread · unknown type");
        assert_eq!(
            title(SemanticMessageType::Consultation),
            format!(
                "Thread · {}",
                crate::domain::models::semantic_message_type_metadata(
                    SemanticMessageType::Consultation
                )
                .token
            )
        );
    }

    #[test]
    fn story21_8_retro_field_for_key_maps_the_three_digits() {
        assert_eq!(retro_field_for_key('1'), Some(RetroField::Received));
        assert_eq!(retro_field_for_key('2'), Some(RetroField::ReplyTimes));
        assert_eq!(retro_field_for_key('3'), Some(RetroField::AckTimes));
        for key in ['0', '4', '9', 'a', 'u', ' ', '$'] {
            assert_eq!(retro_field_for_key(key), None, "{key:?}");
        }
    }

    #[test]
    fn story21_8_retro_overlay_bits_encode_the_preview_and_left_out_fields() {
        let preview = RetroPreview {
            contribution: crate::domain::models::RetroContribution::default(),
            nothing_in_window: false,
        };
        assert_eq!(RETRO_OVERLAY_BITS, 0b1111 << 23);
        assert_eq!(retro_overlay_bits(None), 0);
        let shown = RetroOverlay {
            preview: &preview,
            left_out: None,
        };
        assert_eq!(retro_overlay_bits(Some(&shown)), 1 << 23);
        let mut left_out = RetroFieldSet::new();
        left_out.insert(RetroField::ReplyTimes);
        let one = RetroOverlay {
            preview: &preview,
            left_out: Some(&left_out),
        };
        assert_eq!(retro_overlay_bits(Some(&one)), (1 << 23) | (1 << 25));
        let mut seen = std::collections::BTreeSet::new();
        for subset in 0..8u8 {
            let set: RetroFieldSet = RetroField::ALL
                .into_iter()
                .enumerate()
                .filter(|(index, _)| subset & (1 << index) != 0)
                .map(|(_, field)| field)
                .collect();
            let overlay = RetroOverlay {
                preview: &preview,
                left_out: Some(&set),
            };
            let bits = retro_overlay_bits(Some(&overlay));
            assert_eq!(bits & !RETRO_OVERLAY_BITS, 0, "subset {subset}");
            assert_eq!(bits & (1 << 23), 1 << 23, "the preview bit is always set");
            assert!(seen.insert(bits), "every left-out set has its own key");
        }
        assert_eq!(seen.len(), 8);
        assert!(seen.contains(&RETRO_OVERLAY_BITS), "all three left out");
    }

    /// Ported from the deleted widget tests: the action bits the rail key ORs in are disjoint
    /// from the overlay bits and from the access/page/context/redaction flag bits.
    #[test]
    fn story21_8_action_bits_stay_disjoint_from_overlay_and_flag_bits() {
        // The widget's rail fact word: 31 read-only, 30 local-only, 29 earlier, 28 newer,
        // 27 context selected, 22 redacted.
        let flag_bits: u32 = (1 << 31) | (1 << 30) | (1 << 29) | (1 << 28) | (1 << 27) | (1 << 22);
        let mut action_bits = 0u32;
        for action in ALL {
            let index = action as u8;
            assert!(index < 22, "{} bit {index}", variant_name(action));
            let bit = 1u32 << index;
            assert_eq!(
                action_bits & bit,
                0,
                "{} repeats a bit",
                variant_name(action)
            );
            action_bits |= bit;
        }
        assert_eq!(action_bits.count_ones(), 12);
        assert_eq!(action_bits & RETRO_OVERLAY_BITS, 0);
        assert_eq!(action_bits & flag_bits, 0);
        assert_eq!(RETRO_OVERLAY_BITS & flag_bits, 0);
    }

    #[test]
    fn story21_8_actions_by_type_stage_and_item_state() {
        use PeerRowStage::{AwaitingOperator, Drafting, Ready, Settled};
        use SemanticMessageType as T;
        use ThreadAction as A;
        let writer = ThreadAccess::Writer;
        let received = Some("received");
        let n = None;
        #[rustfmt::skip]
        let table: Vec<(&str, SemanticMessageType, Option<&'static str>, Option<(PeerRowStage, bool)>, [Option<ThreadAction>; 6])> = vec![
            ("consultation awaiting", T::Consultation, received, Some((AwaitingOperator, true)), [Some(A::Respond), Some(A::Busy), n, n, n, n]),
            ("consultation live drafting", T::Consultation, received, Some((Drafting, true)), [n, Some(A::Busy), n, n, n, n]),
            ("consultation orphaned drafting", T::Consultation, received, Some((Drafting, false)), [Some(A::Respond), Some(A::Busy), n, n, n, n]),
            ("consultation ready", T::Consultation, received, Some((Ready, true)), [Some(A::Respond), Some(A::Busy), n, n, n, n]),
            ("consultation without a row", T::Consultation, received, n_control(), [Some(A::Respond), Some(A::Busy), n, n, n, n]),
            ("unknown awaiting", T::Unknown, received, Some((AwaitingOperator, true)), [Some(A::Respond), n, n, n, n, n]),
            ("unknown settled", T::Unknown, received, Some((Settled, true)), [n; 6]),
            ("unknown without a row", T::Unknown, received, n_control(), [n; 6]),
            ("architecture received", T::ArchitectureUpdate, received, n_control(), [Some(A::Discuss), Some(A::Acknowledge), n, n, n, n]),
            ("architecture acknowledged", T::ArchitectureUpdate, Some("acknowledged"), n_control(), [Some(A::Discuss), n, n, n, n, n]),
            ("architecture state unset", T::ArchitectureUpdate, None, n_control(), [Some(A::Discuss), n, n, n, n, n]),
            ("bug report received", T::BugReport, received, n_control(), [Some(A::Discuss), Some(A::Acknowledge), n, n, n, n]),
            ("scope change received", T::ScopeChange, received, n_control(), [n, Some(A::Acknowledge), n, n, n, n]),
            ("design update received", T::DesignUpdate, received, n_control(), [n, Some(A::Acknowledge), n, n, n, n]),
            ("story assignment received", T::StoryAssignment, received, n_control(), [n, Some(A::Acknowledge), n, n, n, n]),
            ("story assignment acknowledged", T::StoryAssignment, Some("acknowledged"), n_control(), [n; 6]),
            ("design update awaiting", T::DesignUpdate, received, Some((AwaitingOperator, true)), [Some(A::Respond), Some(A::Acknowledge), n, n, n, n]),
            ("removed consultation", T::Consultation, Some("removed"), Some((AwaitingOperator, true)), [n; 6]),
            ("removed architecture", T::ArchitectureUpdate, Some("removed"), n_control(), [n; 6]),
        ];
        for (name, message_type, item_state, control, expected) in table {
            let state = thread_state(writer, message_type, item_state);
            let conv = conversation(control, message_type, false, false);
            assert_eq!(offered(&state, &conv), expected, "{name}");
            // Positive control: the same facts as a reader offer nothing.
            for access in [ThreadAccess::ReadOnly, ThreadAccess::LocalOnly] {
                let state = thread_state(access, message_type, item_state);
                assert_eq!(offered(&state, &conv), [None; 6], "{name} as {access:?}");
            }
        }
    }

    fn n_control() -> Option<(PeerRowStage, bool)> {
        None
    }

    #[test]
    fn story21_8_verdict_offers_replace_respond_and_accept_waits_for_review() {
        use crate::domain::models::VerdictOffer;
        use PeerRowStage::{Drafting, Ready};
        let t = SemanticMessageType::ArchitectureUpdate;
        let mut state = thread_state(ThreadAccess::Writer, t, Some("received"));
        let none_conv = conversation(None, t, false, false);
        inspector(&mut state).verdict_offer = VerdictOffer::DiscussOnly;
        assert_eq!(
            offered(&state, &none_conv),
            [
                Some(ThreadAction::DiscussConcerns),
                Some(ThreadAction::Acknowledge),
                None,
                None,
                None,
                None
            ]
        );
        inspector(&mut state).verdict_offer = VerdictOffer::AcceptOrDiscuss;
        assert_eq!(
            offered(&state, &none_conv),
            [
                Some(ThreadAction::DiscussConcerns),
                Some(ThreadAction::Acknowledge),
                Some(ThreadAction::Accept),
                None,
                None,
                None
            ]
        );
        // Ready keeps its draft-review decision in 21-6's frame: no Accept.
        let ready = conversation(Some((Ready, true)), t, false, false);
        assert_eq!(
            offered(&state, &ready),
            [
                Some(ThreadAction::DiscussConcerns),
                Some(ThreadAction::Acknowledge),
                None,
                None,
                None,
                None
            ]
        );
        // A live draft owns the item: neither the verdict nor Accept.
        let drafting = conversation(Some((Drafting, true)), t, false, false);
        assert_eq!(
            offered(&state, &drafting),
            [
                None,
                Some(ThreadAction::Acknowledge),
                None,
                None,
                None,
                None
            ]
        );
    }

    #[test]
    fn story21_8_status_and_retro_previews_offer_their_acts() {
        use PeerRowStage::{AwaitingOperator, Ready};
        use ThreadAction as A;
        let status_type = SemanticMessageType::StatusRequest;
        let state = thread_state(ThreadAccess::Writer, status_type, Some("received"));
        let with = conversation(Some((AwaitingOperator, true)), status_type, true, false);
        let without = conversation(Some((AwaitingOperator, true)), status_type, false, false);
        assert_eq!(
            offered(&state, &with),
            [
                Some(A::Respond),
                Some(A::Busy),
                Some(A::ShareStatus),
                None,
                Some(A::DeclineStatus),
                None
            ]
        );
        assert_eq!(
            offered(&state, &without),
            [Some(A::Respond), None, None, None, None, None]
        );

        let retro_type = SemanticMessageType::RetroRequest;
        let state = thread_state(ThreadAccess::Writer, retro_type, Some("received"));
        let with = conversation(Some((AwaitingOperator, true)), retro_type, false, true);
        assert_eq!(
            offered(&state, &with),
            [
                Some(A::Respond),
                Some(A::Busy),
                Some(A::RetroShare),
                Some(A::RetroLeaveOut),
                Some(A::RetroDecline),
                None
            ]
        );
        let settled_elsewhere = conversation(Some((Ready, true)), retro_type, false, true);
        assert_eq!(
            offered(&state, &settled_elsewhere),
            [Some(A::Respond), None, None, None, None, None],
            "only an awaiting preview offers the retro acts"
        );
        let removed = thread_state(ThreadAccess::Writer, retro_type, Some("removed"));
        assert_eq!(offered(&removed, &with), [None; 6]);
        // The same facts as a reader: nothing (positive control above).
        let read_only = thread_state(ThreadAccess::ReadOnly, retro_type, Some("received"));
        assert_eq!(offered(&read_only, &with), [None; 6]);
        // A thread without its conversation offers nothing either.
        assert_eq!(actions(&state, None), [None; 6]);
    }

    #[test]
    fn story21_8_context_offer_follows_availability_or_selection() {
        let t = SemanticMessageType::Unknown;
        let conv = conversation(None, t, false, false);
        let mut state = thread_state(ThreadAccess::Writer, t, Some("received"));
        assert_eq!(offered(&state, &conv), [None; 6]);
        inspector(&mut state).context_available = true;
        assert_eq!(
            offered(&state, &conv),
            [None, None, None, None, None, Some(ThreadAction::Context)]
        );
        inspector(&mut state).context_available = false;
        inspector(&mut state).context_selected = true;
        assert_eq!(
            offered(&state, &conv)[5],
            Some(ThreadAction::Context),
            "a selected item can always be removed"
        );
        // Not for a reader even when available.
        let mut read_only = thread_state(ThreadAccess::ReadOnly, t, Some("received"));
        inspector(&mut read_only).context_available = true;
        assert_eq!(offered(&read_only, &conv), [None; 6]);
    }

    #[test]
    fn story21_8_in_flight_acts_suppress_only_their_own_slots() {
        use PeerRowStage::AwaitingOperator;
        use ThreadAction as A;
        let t = SemanticMessageType::Consultation;
        let conv = conversation(Some((AwaitingOperator, true)), t, false, false);
        let fresh = || {
            let mut state = thread_state(ThreadAccess::Writer, t, Some("received"));
            inspector(&mut state).context_available = true;
            state
        };
        let baseline = [
            Some(A::Respond),
            Some(A::Busy),
            None,
            None,
            None,
            Some(A::Context),
        ];
        assert_eq!(offered(&fresh(), &conv), baseline);

        let mut state = fresh();
        pend(&mut state, RecipientAction::Defer, &item_id(), None);
        assert_eq!(
            offered(&state, &conv),
            [Some(A::Respond), None, None, None, None, Some(A::Context)]
        );
        let mut state = fresh();
        pend(&mut state, RecipientAction::Reply, &item_id(), None);
        assert_eq!(
            offered(&state, &conv),
            [None, Some(A::Busy), None, None, None, Some(A::Context)]
        );
        let mut state = fresh();
        pend(&mut state, RecipientAction::ContextSelect, &item_id(), None);
        assert_eq!(
            offered(&state, &conv),
            [Some(A::Respond), Some(A::Busy), None, None, None, None]
        );
        // Another item's act does not suppress this item's actions…
        let mut state = fresh();
        pend(&mut state, RecipientAction::Defer, "another-item", None);
        assert_eq!(offered(&state, &conv), baseline);
        // A typed composition for another item does not own this frame's reply field.
        pend(
            &mut state,
            RecipientAction::Reply,
            "another-item",
            Some("typed"),
        );
        assert_eq!(offered(&state, &conv), baseline);
        // A settled act is no longer in flight.
        let mut state = fresh();
        pend(&mut state, RecipientAction::Defer, &item_id(), None);
        crate::adapters::tui::handlers::recipient_actions::apply_action_result(
            &mut state,
            RecipientAction::Defer,
            RecipientActionTarget::Item { item_id: item_id() },
            crate::adapters::daemon::protocol::RecipientActionOutcome::Applied { reply: None },
        );
        assert!(state.thread_outcomes.lines(&item_id()).next().is_some());
        assert_eq!(offered(&state, &conv), baseline);
        // A selected item's removal is suppressed by its own wire act.
        let mut state = fresh();
        inspector(&mut state).context_selected = true;
        assert_eq!(offered(&state, &conv)[5], Some(A::Context));
        pend(&mut state, RecipientAction::ContextRemove, &item_id(), None);
        assert_eq!(offered(&state, &conv)[5], None);

        let ack_type = SemanticMessageType::ArchitectureUpdate;
        let ack_conv = conversation(None, ack_type, false, false);
        let mut state = thread_state(ThreadAccess::Writer, ack_type, Some("received"));
        pend(&mut state, RecipientAction::Acknowledge, &item_id(), None);
        assert_eq!(
            offered(&state, &ack_conv),
            [Some(A::Discuss), None, None, None, None, None]
        );

        let status_type = SemanticMessageType::StatusRequest;
        let status_conv = conversation(Some((AwaitingOperator, true)), status_type, true, false);
        let mut state = thread_state(ThreadAccess::Writer, status_type, Some("received"));
        pend(&mut state, RecipientAction::ShareStatus, &item_id(), None);
        assert_eq!(
            offered(&state, &status_conv),
            [Some(A::Respond), Some(A::Busy), None, None, None, None]
        );

        let retro_type = SemanticMessageType::RetroRequest;
        let retro_conv = conversation(Some((AwaitingOperator, true)), retro_type, false, true);
        let mut state = thread_state(ThreadAccess::Writer, retro_type, Some("received"));
        pend(&mut state, RecipientAction::ShareRetro, &item_id(), None);
        assert_eq!(
            offered(&state, &retro_conv),
            [Some(A::Respond), Some(A::Busy), None, None, None, None],
            "Share, Leave out and Decline share one in-flight key"
        );
    }

    /// The node view (and its Approve/Edit/Reject) is slice 8-B: the thread target is an
    /// item, and no offered action is a draft decision.
    #[test]
    fn story21_8_thread_offers_no_draft_decisions() {
        use PeerRowStage::{AwaitingOperator, Drafting, Ready, Settled};
        for variant in ALL {
            assert!(
                !matches!(variant_name(variant), "Approve" | "Edit" | "Reject"),
                "no draft-decision action exists"
            );
        }
        let mut labels = std::collections::BTreeSet::new();
        for message_type in [
            SemanticMessageType::Consultation,
            SemanticMessageType::StoryAssignment,
            SemanticMessageType::DesignUpdate,
            SemanticMessageType::StatusRequest,
            SemanticMessageType::BugReport,
            SemanticMessageType::ScopeChange,
            SemanticMessageType::ArchitectureUpdate,
            SemanticMessageType::RetroRequest,
            SemanticMessageType::Unknown,
        ] {
            for stage in [AwaitingOperator, Drafting, Ready, Settled] {
                for item_state in [
                    None,
                    Some("received"),
                    Some("acknowledged"),
                    Some("removed"),
                ] {
                    let mut state = thread_state(ThreadAccess::Writer, message_type, item_state);
                    inspector(&mut state).context_available = true;
                    let conv = conversation(Some((stage, true)), message_type, true, true);
                    for action in offered(&state, &conv).into_iter().flatten() {
                        labels.insert(action.label(false, false));
                    }
                }
            }
        }
        assert!(labels.contains("Respond"), "the sweep does offer actions");
        for forbidden in ["Approve", "Edit", "Reject"] {
            assert!(!labels.contains(forbidden), "{forbidden}");
        }
        // This fixture is an item even though node targets also exist.
        let state = thread_state(ThreadAccess::Writer, SemanticMessageType::Unknown, None);
        let address = state
            .thread_inspector
            .as_ref()
            .unwrap()
            .item_address()
            .unwrap();
        assert_eq!(address.item().as_str(), item_id());
    }

    #[test]
    fn story21_8_applies_gates_paging_and_writer_rows() {
        use PeerRowStage::AwaitingOperator;
        let t = SemanticMessageType::Consultation;
        let conv = conversation(Some((AwaitingOperator, true)), t, false, false);
        let mut state = thread_state(ThreadAccess::Writer, t, Some("received"));
        let applies = |state: &TuiState, cmd| applies(state, Some(&conv), cmd);
        assert!(applies(&state, ThreadCmd::Back));
        assert!(applies(&state, ThreadCmd::Respond));
        assert!(applies(&state, ThreadCmd::Busy));
        for cmd in [
            ThreadCmd::Acknowledge,
            ThreadCmd::Discuss,
            ThreadCmd::Accept,
            ThreadCmd::Share,
            ThreadCmd::Decline,
            ThreadCmd::LeaveOut(1),
            ThreadCmd::Context,
        ] {
            assert!(!applies(&state, cmd), "{cmd:?} is not offered");
        }
        assert!(!applies(&state, ThreadCmd::Earlier));
        assert!(!applies(&state, ThreadCmd::Newer));
        inspector(&mut state).omitted_before = 2;
        assert!(applies(&state, ThreadCmd::Earlier));
        assert!(!applies(&state, ThreadCmd::Newer));
        inspector(&mut state).omitted_before = 0;
        inspector(&mut state).omitted_after = 1;
        assert!(applies(&state, ThreadCmd::Newer));
        assert!(!applies(&state, ThreadCmd::Earlier));
        inspector(&mut state).context_available = true;
        assert!(applies(&state, ThreadCmd::Context));
        // A reader pages but cannot act.
        let mut reader = thread_state(ThreadAccess::ReadOnly, t, Some("received"));
        inspector(&mut reader).omitted_after = 1;
        assert!(applies(&reader, ThreadCmd::Newer));
        assert!(!applies(&reader, ThreadCmd::Respond));
        // No thread open: nothing applies, not even Back.
        let closed = TuiState::new(80, 24);
        assert!(!applies(&closed, ThreadCmd::Back));
        // A retro item's digits all apply while it offers Leave out.
        let retro_type = SemanticMessageType::RetroRequest;
        let retro_conv = conversation(Some((AwaitingOperator, true)), retro_type, false, true);
        let retro_state = thread_state(ThreadAccess::Writer, retro_type, Some("received"));
        for index in 0..3 {
            assert!(super::applies(
                &retro_state,
                Some(&retro_conv),
                ThreadCmd::LeaveOut(index)
            ));
        }
        assert_eq!(
            action_for(&retro_state, Some(&retro_conv), ThreadCmd::LeaveOut(2)),
            Some(ThreadAction::RetroLeaveOut)
        );
    }

    #[test]
    fn story21_8_ignores_raw_key_is_a_press_only_rule_for_writer_letters() {
        let key = |code, modifiers, kind| KeyEvent::new_with_kind(code, modifiers, kind);
        let state = thread_state(
            ThreadAccess::Writer,
            SemanticMessageType::Consultation,
            Some("received"),
        );
        let none = KeyModifiers::NONE;
        for c in "rdbaysn123uox".chars() {
            let code = KeyCode::Char(c);
            assert!(
                !ignores_raw_key(&state, &key(code, none, KeyEventKind::Press)),
                "{c}"
            );
            assert!(
                ignores_raw_key(&state, &key(code, none, KeyEventKind::Repeat)),
                "{c}"
            );
            assert!(
                ignores_raw_key(&state, &key(code, none, KeyEventKind::Release)),
                "{c}"
            );
            for modifiers in [
                KeyModifiers::ALT,
                KeyModifiers::SHIFT,
                KeyModifiers::CONTROL,
            ] {
                // Ctrl+D / Ctrl+U and the Ctrl+X leader are registered chords.
                let chord = modifiers == KeyModifiers::CONTROL && matches!(c, 'd' | 'u' | 'x');
                assert_eq!(
                    ignores_raw_key(&state, &key(code, modifiers, KeyEventKind::Press)),
                    !chord,
                    "{c} {modifiers:?}"
                );
            }
        }
        // The uppercase spelling of a writer letter is the same letter.
        assert!(ignores_raw_key(
            &state,
            &key(KeyCode::Char('R'), KeyModifiers::SHIFT, KeyEventKind::Press)
        ));
        assert!(ignores_raw_key(
            &state,
            &key(KeyCode::Char('X'), KeyModifiers::SHIFT, KeyEventKind::Press)
        ));
        // A Release of a registered chord is still a Release.
        assert!(ignores_raw_key(
            &state,
            &key(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
                KeyEventKind::Release
            )
        ));
        // `x` is a writer letter (retract): a Press of the registered leader chord Ctrl+X
        // resolves, its Release is ignored like any Release of a letter.
        assert!(!ignores_raw_key(
            &state,
            &key(
                KeyCode::Char('x'),
                KeyModifiers::CONTROL,
                KeyEventKind::Press
            )
        ));
        assert!(ignores_raw_key(
            &state,
            &key(
                KeyCode::Char('x'),
                KeyModifiers::CONTROL,
                KeyEventKind::Release
            )
        ));
        // Everything else resolves normally, whatever the kind.
        for (code, modifiers) in [
            (KeyCode::Char('j'), none),
            (KeyCode::Char('G'), KeyModifiers::SHIFT),
            (KeyCode::Char('$'), KeyModifiers::SHIFT),
            (KeyCode::Char('p'), KeyModifiers::CONTROL),
            (KeyCode::Char('q'), KeyModifiers::CONTROL),
            (KeyCode::Char('c'), KeyModifiers::CONTROL),
            (KeyCode::Esc, none),
            (KeyCode::Enter, none),
            (KeyCode::PageDown, none),
        ] {
            for kind in [
                KeyEventKind::Press,
                KeyEventKind::Repeat,
                KeyEventKind::Release,
            ] {
                assert!(
                    !ignores_raw_key(&state, &key(code, modifiers, kind)),
                    "{code:?} {modifiers:?} {kind:?}"
                );
            }
        }
        // Only the top frame counts: with no frame, or another frame above, nothing is ignored.
        let repeat_r = key(KeyCode::Char('r'), none, KeyEventKind::Repeat);
        assert!(ignores_raw_key(&state, &repeat_r));
        assert!(!ignores_raw_key(&TuiState::new(80, 24), &repeat_r));
        let mut covered = thread_state(
            ThreadAccess::Writer,
            SemanticMessageType::Consultation,
            Some("received"),
        );
        reducer::open(&mut covered, Surface::Help);
        assert!(!ignores_raw_key(&covered, &repeat_r));
    }
}
