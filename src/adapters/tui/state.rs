use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use crate::adapters::agent_registry::AgentRegistry;
use crate::adapters::skill_registry::SkillRegistry;
use crate::adapters::tui::presentation::inspector_search::InspectorSearch;
use crate::adapters::tui::presentation::text_field::TextField;
use crate::adapters::tui::refresh_tracker::RefreshTracker;
use crate::domain::models::ImageAttachment;
use crate::domain::models::autocomplete::{AutocompleteKind, AutocompleteSuggestion};
use crate::domain::models::palette::{PaletteAction, PaletteEntry, PaletteScope};
use crate::domain::models::tab::TabId;
use crate::domain::models::turn::TurnId;
use crate::domain::models::view_state::SummaryTier;
use crate::domain::models::{FeedbackBlock, FocusState, RetryState, StatusState, UsageInfo};
use crate::domain::services::cross_search::CrossSearchResult;
use crate::domain::services::search::SearchMatch;

use super::color_detect::ColorCapability;
use super::theme::Theme;

mod login_overlay;
pub use login_overlay::*;

/// A five-second bookmark undo belongs to the live Stage and tab, even after close.
#[derive(Debug)]
pub struct BookmarkUndo {
    pub index: usize,
    pub when: Instant,
    pub owning_tab: TabId,
    pub conversation_id: String,
}

/// Attach-mode view descriptor for the status bar (Story 12.2c AC1/AC6).
///
/// Present only when the TUI is an attached client (`run_attached`); the local
/// TUI passes `None`. Carries the bits the status bar needs to render the
/// `attached` indicator and — when a second client holds the writer slot — the
/// dimmed `read-only — another client holds write` segment. Uses only
/// domain/tui-visible primitives (no `adapters::daemon::AttachMode`) so
/// `status_bar` stays free of an adapter→adapter dependency.
#[derive(Debug, Clone, Default)]
pub struct AttachInfo {
    /// This attachment is read-only because another client holds the writer slot
    /// (the daemon granted `ReadOnly`). Drives the never-silent status segment.
    pub read_only: bool,
    /// Channels the daemon reports connected (for the `attached` summary chip).
    pub channel_count: usize,
}

/// Story 19.16g — the passive transparency-log reminder rendered at the right
/// edge of the status bar. Identity-free by construction: a bounded count or
/// an unavailable mark, never a peer, item, or event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LogAwareness {
    /// Nothing unseen, or nothing observed yet: no segment and no separator.
    #[default]
    Hidden,
    /// Rows above the durable seen-through boundary (always > 0).
    Unseen(usize),
    /// Observation or persistence is unavailable or stale: `log: ?`, never an
    /// apparently current zero.
    Unavailable,
}

/// Story 19.16g — the TUI-side value state of the reminder. The
/// infrastructure observer writes `display` and `reset_revision`; a
/// successful draw pushes presented visits into `presented`. No I/O here.
#[derive(Debug, Default)]
pub struct LogAwarenessView {
    pub display: LogAwareness,
    /// The local reset revision a newly read log view is bound to.
    pub reset_revision: u64,
    /// Visits actually presented by a completed draw, awaiting persistence.
    pub presented: Vec<crate::domain::models::LogVisitCandidate>,
}

/// What the thread window shows: a received item (the journal thread), the response row of a
/// peer node (the legacy "response" view) or the thread of an item this host sent (the sent
/// thread, read from the peer).
#[derive(Debug, Clone)]
pub enum ThreadTarget {
    Item(crate::domain::models::ItemAddress),
    Node(String),
    /// `/team thread <peer> <item>`: the operator's own typing, kept raw for the commands the
    /// frame runs; every painted use sanitizes it.
    Sent {
        peer: String,
        item: String,
    },
    // The A2A log lives in Mission Control's Messages tab (Story 21.7 A12), not here.
}

/// The facts of a sent-thread frame: which read it waits for (target + generation + issuing
/// conversation) and what the completed read says about the opened item.
#[derive(Debug, Clone)]
pub struct SentThread {
    /// Incremented per `/team thread` and per reopen: only the newest read applies.
    pub generation: u64,
    /// The conversation that issued the read (origin keying).
    pub origin: String,
    /// The read has not completed yet (the body is `reading…`).
    pub loading: bool,
    /// The opened item is this host's own item per the read.
    pub mine: bool,
    /// The read marks the opened item retracted.
    pub retracted: bool,
}

/// What the opening host may do with the thread: act on it (attached writer), only read it
/// (attached observer), or read it without any daemon (the local host).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadAccess {
    Writer,
    ReadOnly,
    LocalOnly,
}

/// An in-flight item act, keyed by (action, target) in `TuiState.recipient_actions`. A
/// settled act leaves the map: its outcome line goes to the thread outcome log.
pub enum RecipientActionState {
    Pending {
        /// The text of a typed `/team reply …` submitted from the composer (restored there
        /// when the daemon refuses it).
        composed_input: Option<String>,
        /// The text submitted from the thread's reply field (kept in the thread when the
        /// daemon refuses it).
        reply: Option<String>,
    },
}

/// A frame-owned reply field (Story 21.8 A8): the text of a reply to the opened item, never
/// the composer.
#[derive(Debug, Clone, Default)]
pub struct ThreadReplyField {
    pub field: crate::adapters::tui::presentation::text_field::TextField,
    /// `[d] Discuss` of a story list: the typed verdict is `--discuss`.
    pub discuss: bool,
}

impl ThreadReplyField {
    /// The command this field will run, shown as its prompt.
    pub fn prompt(&self, item_id: &str) -> String {
        if self.discuss {
            format!("/team reply --discuss {item_id} ")
        } else {
            format!("/team reply {item_id} ")
        }
    }
}

/// Rows laid out for one (width, content generation, retrospective overlay) and the footer
/// rail for one (width, rail facts): bounded work per repaint, recomputed only when the key
/// changes.
#[derive(Default)]
pub struct ThreadLayoutCache {
    pub body_key: Option<(u16, u64, u32)>,
    pub body: Vec<ratatui::text::Line<'static>>,
    /// Rows of the body that scroll sideways (wide blocks of a rendered body).
    pub wide: Vec<std::ops::Range<usize>>,
    pub widest: usize,
    pub header: Vec<ratatui::text::Line<'static>>,
    pub footer_key: Option<(u16, u64)>,
    pub footer_info: Vec<ratatui::text::Line<'static>>,
    pub footer_reader: Vec<ratatui::text::Line<'static>>,
}

/// Journal/card data owned by the thread frame (the reducer sets and clears the slot with
/// the frame). Painting reads it through `&TuiState`, so what a paint learns (the fitted
/// row and viewport counts, the layout cache) lives in cells.
pub struct ThreadInspectorState {
    pub target: ThreadTarget,
    pub access: ThreadAccess,
    pub root_id: Option<String>,
    pub first_non_pinned_ordinal: Option<usize>,
    pub last_non_pinned_ordinal: Option<usize>,
    pub omitted_before: usize,
    pub omitted_after: usize,
    pub node_id: Option<String>,
    /// The item view of a node that has an item address, or the node view of an item that
    /// has a pinned response: attached `Tab` toggles between them.
    pub can_toggle: bool,
    pub item_state: Option<&'static str>,
    pub context_available: bool,
    pub context_selected: bool,
    pub viewer: crate::adapters::tui::presentation::viewer::Viewer,
    /// The page as the legacy literal text (tests read it; Source mode paints its segments).
    pub content: String,
    /// The page as local text and peer bodies (the Rendered mode and the filter paint these).
    pub segments: Vec<crate::adapters::tui::thread_body::ThreadSegment>,
    pub turns: Vec<crate::adapters::tui::thread_body::TurnMark>,
    /// Rendered markdown (default) or the literal source (`v`).
    pub mode: crate::adapters::tui::presentation::markdown_body::BodyMode,
    /// The opened item's original body bytes: what `c` copies (never the escaped display).
    pub original: Option<String>,
    pub message_type: crate::domain::models::SemanticMessageType,
    /// Story 19.22 `AC4(c)` — whether the opened item carries a story list and
    /// which verdicts it offers; `VerdictOffer::None` elsewhere.
    pub verdict_offer: crate::domain::models::VerdictOffer,
    /// The open reply field (attached writers), a child text context of this frame.
    pub reply: Option<ThreadReplyField>,
    /// The turn filter (`/`): the pinned identity rows and the opened item always stay.
    pub search: crate::adapters::tui::presentation::inspector_search::InspectorSearch,
    /// The block viewport (`Enter` on a wide block) is entered.
    pub viewport: bool,
    /// Bumped by every change the painted body depends on besides the width.
    pub generation: u64,
    /// `(rows, viewport)` of the last paint: the row list the viewer clamps against is the
    /// one that was painted.
    pub fit: std::cell::Cell<(usize, usize)>,
    /// `(widest wide block in cells, columns it scrolls in)` of the last paint; the widest
    /// is 0 when no block is wider than the body.
    pub wide_fit: std::cell::Cell<(usize, usize)>,
    /// `(writer rail painted whole, page rail painted whole)` at the last paint: a key whose
    /// hint was not painted is inert (`true, true` before the first paint).
    pub rail_live: std::cell::Cell<(bool, bool)>,
    pub layout: std::cell::RefCell<ThreadLayoutCache>,
    /// How many body layouts the cache has computed (a repaint at the same key adds none).
    pub layouts_built: std::cell::Cell<usize>,
    /// The read this sent-thread frame waits for or shows (`None` on every other target).
    pub sent: Option<SentThread>,
}

impl ThreadInspectorState {
    pub fn new(
        target: ThreadTarget,
        access: ThreadAccess,
        message_type: crate::domain::models::SemanticMessageType,
    ) -> Self {
        Self {
            target,
            access,
            root_id: None,
            first_non_pinned_ordinal: None,
            last_non_pinned_ordinal: None,
            omitted_before: 0,
            omitted_after: 0,
            node_id: None,
            can_toggle: false,
            item_state: None,
            context_available: false,
            context_selected: false,
            viewer: Default::default(),
            content: String::new(),
            segments: Vec::new(),
            turns: Vec::new(),
            mode: crate::adapters::tui::presentation::markdown_body::BodyMode::Rendered,
            original: None,
            message_type,
            verdict_offer: crate::domain::models::VerdictOffer::None,
            reply: None,
            search: Default::default(),
            viewport: false,
            generation: 0,
            fit: std::cell::Cell::new((0, 0)),
            wide_fit: std::cell::Cell::new((0, 0)),
            rail_live: std::cell::Cell::new((true, true)),
            layout: std::cell::RefCell::new(ThreadLayoutCache::default()),
            layouts_built: std::cell::Cell::new(0),
            sent: None,
        }
    }

    /// The opened item's address (`None` on the node view).
    pub fn item_address(&self) -> Option<&crate::domain::models::ItemAddress> {
        match &self.target {
            ThreadTarget::Item(address) => Some(address),
            ThreadTarget::Node(_) | ThreadTarget::Sent { .. } => None,
        }
    }

    /// The key of this target in the outcome log: the item id, or `node:<id>`.
    pub fn outcome_key(&self) -> String {
        match &self.target {
            ThreadTarget::Item(address) => address.item().as_str().to_owned(),
            ThreadTarget::Node(node) => format!("node:{node}"),
            ThreadTarget::Sent { peer, item } => format!("sent:{peer}:{item}"),
        }
    }

    /// Something the painted body depends on changed.
    pub fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    /// Show the newest rows (a fresh outcome line): painting clamps the offset.
    pub fn scroll_to_end(&mut self) {
        self.viewer.top = usize::MAX;
        self.viewer.follow = false;
    }
}

/// Direction for boundary navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

/// Four-axis key for turn height cache entries.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct HeightKey {
    pub turn_id: TurnId,
    pub expansion: bool,
    pub summary_tier: SummaryTier,
    pub terminal_width: u16,
}

/// Key for user/system message height cache entries.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct MessageHeightKey {
    pub msg_id: String,
    pub terminal_width: u16,
    pub content_hash: u64,
}

/// Cached layout for a turn, including height and per-part block offsets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CachedTurnLayout {
    pub height: usize,
    pub block_offsets: Vec<usize>,
    /// Story 19.9 A3: `(start_offset, tool_call_id)` for every
    /// `TurnPart::ToolInvocation` in this turn, in part order. `block_offsets`
    /// cannot serve this: it interleaves prose/reasoning starts with tool-block
    /// starts and carries no id, so a consumer cannot tell WHICH tool block a
    /// boundary belongs to — which is exactly why keyboard focus could only
    /// ever resolve to the conversation's first tool call.
    pub tool_block_offsets: Vec<(usize, String)>,
    /// Story 21.13: `(start_offset, block_item_id)` of every code block and
    /// table of every `Prose` part, in part order (D7). Computed in the same
    /// height pass; the host decides whether they take focus (local Stage only).
    pub md_block_offsets: Vec<(usize, String)>,
    /// Story 21.9 D7: `(start_offset, reasoning_item_id)` of every committed
    /// reasoning header, in part order. Computed in the same height pass; the
    /// host decides whether they take focus (local Stage only).
    pub reasoning_offsets: Vec<(usize, String)>,
}

/// Cache of rendered line heights, keyed by turn or message.
///
/// # Invalidation triggers
/// - `invalidate_all()`: resize, tier toggle
/// - `invalidate_turn(turn_id)`: fold toggle on a specific turn
/// - `evict_turns_not_in(live)`: rewind / fork / compact (turn list shrinks)
///
/// # Key axes
/// Turn entries use `HeightKey` (4-axis: turn_id, expansion, summary_tier,
/// terminal_width). Message entries use
/// `MessageHeightKey` (3-axis: msg_id, terminal_width, content_hash).
#[derive(Debug, Default)]
pub struct HeightCache {
    pub entries: HashMap<HeightKey, CachedTurnLayout>,
    pub message_entries: HashMap<MessageHeightKey, usize>,
    pub last_seen_turn_count: usize,
}

impl HeightCache {
    /// Get cached layout for a turn key.
    pub fn get(&self, key: &HeightKey) -> Option<&CachedTurnLayout> {
        self.entries.get(key)
    }

    /// Get cached height for a message key.
    pub fn get_message(&self, key: &MessageHeightKey) -> Option<usize> {
        self.message_entries.get(key).copied()
    }

    /// Set cached layout for a turn key.
    pub fn set(&mut self, key: HeightKey, layout: CachedTurnLayout) {
        self.entries.insert(key, layout);
    }

    /// Set cached height for a message key.
    pub fn set_message(&mut self, key: MessageHeightKey, height: usize) {
        self.message_entries.insert(key, height);
    }

    /// Full invalidation (resize, tier toggle, etc.).
    pub fn invalidate_all(&mut self) {
        self.entries.clear();
        self.message_entries.clear();
        self.last_seen_turn_count = 0;
    }

    /// Surgical invalidation: remove all entries for a specific turn.
    pub fn invalidate_turn(&mut self, turn_id: &TurnId) {
        self.entries.retain(|k, _| &k.turn_id != turn_id);
    }

    /// Evict entries for turns not in the live set.
    pub fn evict_turns_not_in<'a>(&mut self, live: impl Iterator<Item = &'a TurnId>) {
        let live_set: std::collections::HashSet<&TurnId> = live.collect();
        self.entries.retain(|k, _| live_set.contains(&k.turn_id));
    }
}

/// Per-tab render state holding the height cache and related render metadata.
/// Lives in the adapter layer (TuiState side-table) to respect hexagonal
/// architecture — TabState in domain/ cannot hold adapter types.
#[derive(Debug, Default)]
pub struct TabRenderState {
    pub height_cache: HeightCache,
    /// Last-painted absolute tool-header starts; moved from the Stage renderer.
    /// Adapter-side and tab-owned so selection never borrows another tab's layout.
    pub item_boundaries: Vec<(usize, String)>,
    pub cached_width: Option<u16>,
    /// Story 21.12 (D15): the output supports OSC 8, so Stage markdown links
    /// are encoded after the paint. Changes no geometry (the host suffix is
    /// painted either way), so no height-cache key depends on it. Production
    /// callers set it from `hyperlink::terminal_supports_osc8()` before the
    /// chat-pane draw; it defaults to `false`.
    pub osc8_links: bool,
    /// Story 21.13 (D5): the code cap the Stage heights and paint were built
    /// with. `None` (the default and the attached host) leaves code uncapped.
    /// Production writes it only through `set_code_cap`, which drops every
    /// cached height on a change (the cap shapes the geometry the cache stores);
    /// the field is `pub` solely because existing tests build this struct with
    /// `..Default::default()` literals.
    pub code_cap: Option<u16>,
    /// Story 21.9 (D6): how committed reasoning paints and is measured.
    /// `Expanded` by default so a caller that never chose (the attached host,
    /// tests) hides nothing; the local host writes it only through
    /// `set_reasoning`, which drops every cached height on a change.
    pub reasoning: crate::domain::models::ReasoningDisplay,
    /// Story 21.9 (D14): the workspace root tool lines display paths relative
    /// to. Set once per tab by each host (a refresh, never per draw); `None`
    /// shows paths as the model gave them.
    pub workspace_root: Option<Arc<std::path::Path>>,
    /// Story 21.9 (D17): what the empty conversation's launchpad shows, built
    /// by `launchpad_facts::refresh` and replaced only when its key changes.
    /// `None` paints the receipt-bound rows only.
    pub launchpad: Option<crate::adapters::tui::launchpad::LaunchpadFacts>,
}

impl TabRenderState {
    /// Adopt a new cap; the height cache is invalidated only on a change.
    pub fn set_code_cap(&mut self, cap: Option<u16>) {
        if self.code_cap != cap {
            self.code_cap = cap;
            self.height_cache.invalidate_all();
        }
    }

    /// Adopt a reasoning display; the height cache is invalidated only on a change.
    pub fn set_reasoning(&mut self, display: crate::domain::models::ReasoningDisplay) {
        if self.reasoning != display {
            self.reasoning = display;
            self.height_cache.invalidate_all();
        }
    }
}

/// Session-scoped input history buffer for Up/Down navigation and Ctrl+R search.
/// Bounded to MAX_HISTORY entries. Not persisted across restarts.
// Covers: UX-DR74 (input history)
pub struct InputHistory {
    entries: VecDeque<String>,
    /// Current position when navigating (None = not navigating).
    cursor: Option<usize>,
    /// Saves current input when user starts navigating history.
    draft: String,
}

const MAX_HISTORY: usize = 100;

impl InputHistory {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            cursor: None,
            draft: String::new(),
        }
    }

    /// Add entry, evict oldest if at capacity.
    pub fn push(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        // Suppress consecutive identical entries (shell-history convention, UX-DR74)
        if self.entries.back() == Some(&text) {
            return;
        }
        self.entries.push_back(text);
        if self.entries.len() > MAX_HISTORY {
            self.entries.pop_front();
        }
        self.reset_navigation();
    }

    /// Move cursor up (toward older entries), return entry.
    /// On first call, saves current_input as draft.
    pub fn navigate_up(&mut self, current_input: &str) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        match self.cursor {
            None => {
                // Start navigating from newest entry
                self.draft = current_input.to_string();
                let idx = self.entries.len() - 1;
                self.cursor = Some(idx);
                Some(&self.entries[idx])
            }
            Some(idx) => {
                if idx > 0 {
                    let new_idx = idx - 1;
                    self.cursor = Some(new_idx);
                    Some(&self.entries[new_idx])
                } else {
                    // Already at oldest entry
                    Some(&self.entries[0])
                }
            }
        }
    }

    /// Move cursor down (toward newer entries), return entry or None (back to draft).
    pub fn navigate_down(&mut self) -> Option<&str> {
        match self.cursor {
            None => None,
            Some(idx) => {
                if idx + 1 < self.entries.len() {
                    let new_idx = idx + 1;
                    self.cursor = Some(new_idx);
                    Some(&self.entries[new_idx])
                } else {
                    // Past the newest entry → back to draft
                    self.cursor = None;
                    Some(&self.draft)
                }
            }
        }
    }

    /// Clear cursor, discard draft.
    pub fn reset_navigation(&mut self) {
        self.cursor = None;
        self.draft.clear();
    }

    /// Case-insensitive substring search, returns (index, entry) pairs.
    pub fn search(&self, query: &str) -> Vec<(usize, &str)> {
        if query.is_empty() {
            return Vec::new();
        }
        let lower_query = query.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, entry)| entry.to_lowercase().contains(&lower_query))
            .map(|(i, entry)| (i, entry.as_str()))
            .collect()
    }

    /// Whether currently navigating history.
    pub fn is_navigating(&self) -> bool {
        self.cursor.is_some()
    }

    /// Get the saved draft.
    #[allow(dead_code)]
    pub fn draft(&self) -> &str {
        &self.draft
    }

    /// Number of entries.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether history is empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for InputHistory {
    fn default() -> Self {
        Self::new()
    }
}

/// State for reverse search overlay (Ctrl+R).
// Covers: UX-DR74 (reverse search)
pub struct ReverseSearchState {
    pub query: TextField,
    pub matches: Vec<(usize, String)>,
    /// None closes the routed Input popup; Some owns its selected row.
    pub selected_match: Option<usize>,
}

impl ReverseSearchState {
    pub fn new() -> Self {
        Self {
            query: TextField::default(),
            matches: Vec::new(),
            selected_match: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.selected_match.is_some()
    }

    pub fn reset(&mut self) {
        self.query.clear();
        self.matches.clear();
        self.selected_match = Some(0);
    }

    pub fn dismiss(&mut self) {
        self.selected_match = None;
    }
}

impl Default for ReverseSearchState {
    fn default() -> Self {
        Self::new()
    }
}

/// Sub-state of the within-conversation search overlay.
///
/// Resolves the `n` / `N` input-vs-navigation collision (Story 4-4 AC3): in
/// `Typing` every printable key builds the query; in `Navigating` the query is
/// committed and `n` / `N` cycle matches. Any printable key in `Navigating`
/// returns to `Typing` and applies the keystroke, letting the user refine the
/// query without pressing Esc.
// Covers: UX-DR86, Story 4-4 AC3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchSubstate {
    Typing,
    Navigating,
}

/// Transient peek-highlight applied to a single match for 1500 ms when a
/// cross-conversation search result is opened in a new tab (Story 4-4 AC6).
/// Fields are populated by the cross-search-result handler in Task 8.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PeekHighlight {
    pub m: SearchMatch,
    pub expires_at: Instant,
}

/// State for the within-conversation search overlay (Ctrl+F).
///
/// Distinct from `ReverseSearchState` (Ctrl+R input history search) — that one
/// searches the user's past-input buffer; this one searches the rendered
/// conversation. Byte offsets in `matches` are into the **original** message
/// content string, not a lowercased copy — see `domain::services::search`.
// Covers: UX-DR86, Story 4-4 AC1-AC7
pub struct SearchState {
    pub query: TextField,
    pub matches: Vec<SearchMatch>,
    /// Index into `matches` of the currently focused match.
    pub focused_match_index: usize,
    /// Typing (cursor in input, n/N build query) vs Navigating (query committed,
    /// n/N cycle matches). See AC3 amendment.
    pub substate: SearchSubstate,
    /// Last time `find_matches` ran — drives the 30 ms recomputation debounce
    /// in Task 3.3, separate from the calm-jump rule.
    pub last_search_instant: Option<Instant>,
    /// Query length at the time of the last `find_matches` call — when the
    /// length changes, bypass the 30 ms debounce; when it doesn't (held-key
    /// repeat), honor the debounce and skip the scan.
    pub last_query_len: usize,
    /// Exact authoritative view before this opening, owned by one tab.
    pub pre_search_view: Option<crate::domain::models::ViewState>,
    pub owning_tab: Option<crate::domain::models::tab::TabId>,
    /// Transient peek highlight for cross-search result jumps (AC6).
    /// Populated by Task 8 (open cross-search result in new tab).
    #[allow(dead_code)]
    pub peek_highlight: Option<PeekHighlight>,
}

impl SearchState {
    pub fn new() -> Self {
        Self {
            query: TextField::default(),
            matches: Vec::new(),
            focused_match_index: 0,
            substate: SearchSubstate::Typing,
            last_search_instant: None,
            last_query_len: 0,
            pre_search_view: None,
            owning_tab: None,
            peek_highlight: None,
        }
    }
}

impl Default for SearchState {
    fn default() -> Self {
        Self::new()
    }
}

/// State for the cross-conversation search overlay (`/` in the History window).
///
/// Lives in the adapter layer because it's TUI view state. The actual scan
/// runs in the event loop via `domain::services::cross_search::run_cross_search`
/// and writes its results back into this struct via a stale-guarded update.
// Covers: Story 4-4 AC5, AC6, AC7 (UX-DR87)
pub struct CrossSearchState {
    pub query: TextField,
    /// Every content edit invalidates earlier requests, even a retype to the same text.
    pub request_revision: u64,
    pub results: Vec<CrossSearchResult>,
    pub selected: usize,
    /// Set when the last scan hit the count cap (20 results).
    pub truncated_by_count: bool,
    /// Set when the last scan hit the wall-clock budget (200 ms).
    pub truncated_by_time: bool,
    /// Total conversations in the index at the time of the last scan.
    pub total: usize,
    /// Actual conversations scanned before truncation.
    pub scanned: usize,
    /// Currently running — shown as a "Searching…" hint while the scan task
    /// is in flight. Mirrors reviewer Fix 5's loading indicator requirement.
    pub running: bool,
    /// Static in-frame failure feedback; notification toasts paint below sheets.
    pub load_failed: bool,
}

impl CrossSearchState {
    pub fn new() -> Self {
        Self {
            query: TextField::default(),
            request_revision: 0,
            results: Vec::new(),
            selected: 0,
            truncated_by_count: false,
            truncated_by_time: false,
            total: 0,
            scanned: 0,
            running: false,
            load_failed: false,
        }
    }
}

impl Default for CrossSearchState {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolved file mention tracked from autocomplete selection.
/// Stored so we don't re-parse with regex at send time.
#[derive(Debug, Clone)]
pub struct ResolvedMention {
    /// File path relative to workspace.
    pub path: String,
    /// Who named the path: the user via the TUI, or a model tool call.
    pub source: crate::domain::models::FileContextProvenance,
}

/// State for inline autocomplete popup (/ commands and @ file mentions).
// Covers: UX-DR75
pub struct AutocompleteState {
    /// Routed popup identity; `None` means closed. Autocomplete is not a frame.
    pub kind: Option<AutocompleteKind>,
    /// Cursor position where the trigger character (/ or @) was typed.
    pub trigger_position: usize,
    /// Characters typed after the trigger, used for filtering.
    pub filter_text: String,
    /// Current filtered suggestions.
    pub suggestions: Vec<AutocompleteSuggestion>,
    /// Currently highlighted suggestion (0-based, wraps around).
    pub selected_index: usize,
    /// Scroll offset for long suggestion lists.
    pub scroll_offset: usize,
    /// Pre-trigger composer saved when a slash popup opened from typed input.
    /// `None` for mention popups, pasted/literal slash text and after any
    /// accept/dismiss (Story 21-2 A2).
    pub slash_rollback: Option<SlashRollback>,
}

/// Composer snapshot taken immediately before a slash trigger is typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashRollback {
    pub buffer: String,
    /// Cursor (char index) before the trigger; equals the trigger position.
    pub cursor: usize,
    pub scroll: usize,
}

impl AutocompleteState {
    pub fn new() -> Self {
        Self {
            kind: None,
            trigger_position: 0,
            filter_text: String::new(),
            suggestions: Vec::new(),
            selected_index: 0,
            scroll_offset: 0,
            slash_rollback: None,
        }
    }

    /// Open autocomplete with the given kind and trigger position.
    pub fn open(
        &mut self,
        kind: AutocompleteKind,
        trigger_position: usize,
        suggestions: Vec<AutocompleteSuggestion>,
    ) {
        self.kind = Some(kind);
        self.trigger_position = trigger_position;
        self.filter_text.clear();
        self.suggestions = suggestions;
        self.selected_index = 0;
        self.scroll_offset = 0;
        self.slash_rollback = None;
    }

    /// Dismiss the autocomplete popup.
    pub fn dismiss(&mut self) {
        self.kind = None;
        self.filter_text.clear();
        self.suggestions.clear();
        self.selected_index = 0;
        self.scroll_offset = 0;
        self.slash_rollback = None;
    }

    pub fn is_open(&self) -> bool {
        self.kind.is_some()
    }

    /// Record the composer as it was before a slash trigger was typed, so the
    /// provisional `/` + filter can be rolled back by Esc (Story 21-2 A2).
    /// Call only when a *new* slash popup opens from typed input.
    pub fn own_slash_edit(&mut self, buffer: &str, cursor: usize, scroll: usize) {
        self.slash_rollback = Some(SlashRollback {
            buffer: buffer.to_owned(),
            cursor,
            scroll,
        });
    }

    /// Consume slash ownership for an Esc cancel. Returns the saved composer
    /// only while the live composer still equals the provisional edit this
    /// popup made (`before + "/" + filter + after`, cursor at filter end);
    /// any unrelated edit or cursor relocation yields `None` so a stale
    /// snapshot is never restored. Ownership is always cleared.
    pub fn take_slash_rollback(&mut self, buffer: &str, cursor: usize) -> Option<SlashRollback> {
        let rollback = self.slash_rollback.take()?;
        if self.kind != Some(AutocompleteKind::SlashCommand)
            || self.trigger_position != rollback.cursor
        {
            return None;
        }
        let split = rollback
            .buffer
            .char_indices()
            .nth(rollback.cursor)
            .map_or(rollback.buffer.len(), |(byte, _)| byte);
        let (before, after) = rollback.buffer.split_at(split);
        let expected = format!("{before}/{}{after}", self.filter_text);
        let expected_cursor = rollback.cursor + 1 + self.filter_text.chars().count();
        (buffer == expected && cursor == expected_cursor).then_some(rollback)
    }

    /// True while a slash popup owns a provisional edit it may roll back.
    pub fn owns_slash_edit(&self) -> bool {
        self.slash_rollback.is_some()
    }

    /// Navigate up or down in the suggestion list (wraps around).
    pub fn navigate(&mut self, direction: Direction) {
        if self.suggestions.is_empty() {
            return;
        }
        match direction {
            Direction::Up => {
                if self.selected_index == 0 {
                    self.selected_index = self.suggestions.len() - 1;
                } else {
                    self.selected_index -= 1;
                }
            }
            Direction::Down => {
                self.selected_index = (self.selected_index + 1) % self.suggestions.len();
            }
            Direction::Left | Direction::Right => {}
        }
        // Adjust scroll offset to keep selected item visible
        let max_visible = 8;
        if self.selected_index < self.scroll_offset {
            self.scroll_offset = self.selected_index;
        } else if self.selected_index >= self.scroll_offset + max_visible {
            self.scroll_offset = self.selected_index + 1 - max_visible;
        }
    }

    /// Update the filter text and reset selection.
    #[allow(dead_code)]
    pub fn update_filter(&mut self, filter: String, suggestions: Vec<AutocompleteSuggestion>) {
        self.filter_text = filter;
        self.suggestions = suggestions;
        self.selected_index = 0;
        self.scroll_offset = 0;
    }

    /// Get the currently selected suggestion, if any.
    pub fn selected(&self) -> Option<&AutocompleteSuggestion> {
        self.suggestions.get(self.selected_index)
    }
}

impl Default for AutocompleteState {
    fn default() -> Self {
        Self::new()
    }
}

/// The task actions' shared state (Story 6.3/6.4, trimmed by Story 21.7): the plan the Tasks
/// tab resolves first, and the reorder in progress. The panel selection, the drill-down and
/// the auto-open bookkeeping left with the sidebar panel.
#[derive(Debug, Clone, Default)]
pub struct TaskPanelState {
    pub last_executed_plan_id: Option<String>,
    /// Story 6.4: the task being reordered, with the plan and conversation it belongs to and
    /// the order to restore on Esc (A17: reorder lives in the Tasks tab).
    pub reorder: Option<Reorder>,
}

/// A reorder in progress: which task of which plan, and the order Esc restores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reorder {
    pub conversation_id: String,
    pub plan_id: String,
    pub number: u32,
    pub original_order: Vec<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct AgentPanelState {
    pub cached_entries: Vec<crate::domain::models::subagent_view::AgentRowView>,
    pub spool_tail_cache: std::collections::HashMap<crate::domain::models::AgentId, String>,
    /// Agents whose cached tail is a lossy rendering of bytes that were not valid text (their
    /// window shows it but refuses to copy it as "the original").
    pub lossy_tails: std::collections::HashSet<crate::domain::models::AgentId>,
}

/// Story 6.4: user's choice on the skip cascade card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipCascadeChoice {
    CascadeSkip,
    ContinueAnyway,
    CancelSkip,
}

/// Story 7.4: context warning escalation level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextWarnLevel {
    None,
    Warn,
    Crit,
}

/// A provider column in the model selector dialog (Story 7.2 AC2).
/// TUI render state — lives in adapters/tui/, NOT domain/.
#[derive(Debug, Clone)]
pub struct ProviderColumn {
    pub provider_id: String,
    pub display_name: String,
    pub healthy: bool,
    pub models: Vec<crate::domain::models::provider::ModelDescriptor>,
    pub locked_hint: Option<String>,
    pub discovery: Option<crate::domain::models::provider::DiscoveryStatus>,
    /// Sign-in hint for an available provider whose credential is rejected.
    pub sign_in_hint: Option<String>,
}

/// Available providers retain registry order, including empty catalogues; public
/// catalogues without credentials follow in display-name order.
pub fn model_selector_columns(
    info: &dyn crate::domain::ports::ProviderInfoPort,
) -> Vec<ProviderColumn> {
    let mut columns: Vec<_> = info
        .list_providers()
        .into_iter()
        .map(|provider| ProviderColumn {
            models: info.list_models_by_provider(&provider.provider_id),
            discovery: info.discovery_status(&provider.provider_id),
            sign_in_hint: info.sign_in_hint(&provider.provider_id),
            provider_id: provider.provider_id,
            display_name: provider.display_name,
            healthy: provider.healthy,
            locked_hint: None,
        })
        .collect();
    let mut locked = info.list_locked_providers();
    locked.sort_by(|a, b| {
        a.display_name
            .cmp(&b.display_name)
            .then(a.provider_id.cmp(&b.provider_id))
    });
    columns.extend(locked.into_iter().map(|provider| ProviderColumn {
        discovery: info.discovery_status(&provider.provider_id),
        provider_id: provider.provider_id,
        display_name: provider.display_name,
        healthy: false,
        models: provider.models,
        locked_hint: Some(provider.sign_in_hint),
        sign_in_hint: None,
    }));
    columns
}

/// Pending context-window warning state (Story 7.2 AC5).
#[derive(Debug, Clone)]
pub struct ContextWarning {
    pub provider_id: String,
    pub model_id: String,
    pub model_display_name: String,
    pub context_window: u32,
    pub current_tokens: u32,
}

/// State for the model/provider selector overlay (Ctrl+X, M) (Story 7.2 AC1).
pub struct ModelSelectorState {
    pub columns: Vec<ProviderColumn>,
    pub selected_provider: usize,
    pub selected_model: usize,
    pub connecting: Option<String>,
    pub pending_context_warning: Option<ContextWarning>,
    pub refreshing: Arc<RefreshTracker>,
    pub search_active: bool,
    pub search_query: String,
    pub filtered_indices: Vec<usize>,
    /// Locked columns a sign-in can unlock: column id → store key to log in.
    pub sign_in: HashMap<String, String>,
    /// The discovery line under a column after its sign-in: (column id, line).
    pub bloom: Option<(String, String)>,
}

impl ModelSelectorState {
    pub fn new() -> Self {
        Self {
            columns: Vec::new(),
            selected_provider: 0,
            selected_model: 0,
            connecting: None,
            pending_context_warning: None,
            refreshing: RefreshTracker::new(),
            search_active: false,
            search_query: String::new(),
            filtered_indices: Vec::new(),
            sign_in: HashMap::new(),
            bloom: None,
        }
    }

    /// Prepare a fresh frame, seeding the active provider and model.
    pub fn initialize(
        &mut self,
        columns: Vec<ProviderColumn>,
        active_provider_id: &str,
        active_model_id: &str,
    ) {
        self.connecting = None;
        self.pending_context_warning = None;
        self.search_active = false;
        self.search_query.clear();
        self.filtered_indices.clear();
        self.bloom = None;
        // Only `handlers::model_switch::open_selector` knows which locked columns
        // a sign-in can unlock; any other opener offers none (never a stale map).
        self.sign_in.clear();
        self.columns = columns;

        self.selected_provider = self
            .columns
            .iter()
            .position(|c| c.provider_id == active_provider_id)
            .unwrap_or(0);

        self.selected_model = self
            .columns
            .get(self.selected_provider)
            .and_then(|col| {
                col.models
                    .iter()
                    .position(|m| m.model_id == active_model_id)
            })
            .unwrap_or(0);
    }

    /// Refresh an open snapshot without changing focus, search, or selection IDs.
    pub fn refresh_columns(&mut self, columns: Vec<ProviderColumn>) {
        let provider_id = self
            .columns
            .get(self.selected_provider)
            .map(|c| c.provider_id.clone());
        let model_id = self
            .columns
            .get(self.selected_provider)
            .and_then(|c| self.selected_model_index().and_then(|i| c.models.get(i)))
            .map(|m| m.model_id.clone());
        self.columns = columns;
        self.selected_provider = provider_id
            .as_ref()
            .and_then(|id| self.columns.iter().position(|c| &c.provider_id == id))
            .unwrap_or(0);
        self.recompute_filter();
        let col = self.columns.get(self.selected_provider);
        let model_index = col.and_then(|c| {
            model_id
                .as_ref()
                .and_then(|id| c.models.iter().position(|m| &m.model_id == id))
        });
        self.selected_model = if self.search_active && !self.search_query.is_empty() {
            model_index.and_then(|i| self.filtered_indices.iter().position(|&index| index == i))
        } else {
            model_index
        }
        .unwrap_or(0);
    }

    /// Release the closed frame's editable payload.
    pub fn clear(&mut self) {
        self.columns.clear();
        self.connecting = None;
        self.pending_context_warning = None;
        self.search_active = false;
        self.search_query.clear();
        self.filtered_indices.clear();
    }

    /// Navigate between providers (Left/Right). Wraps around; no-op when ≤1 column (AC7).
    pub fn navigate_provider(&mut self, direction: Direction) {
        if self.columns.len() <= 1 {
            return;
        }
        // The sign-in discovery line belongs to the column it was shown under.
        self.bloom = None;
        match direction {
            Direction::Left => {
                if self.selected_provider == 0 {
                    self.selected_provider = self.columns.len() - 1;
                } else {
                    self.selected_provider -= 1;
                }
            }
            Direction::Right => {
                self.selected_provider = (self.selected_provider + 1) % self.columns.len();
            }
            _ => {}
        }
        self.selected_model = 0;
        self.search_active = false;
        self.search_query.clear();
        self.filtered_indices.clear();
    }

    /// Navigate between models (Up/Down). Wraps within current column.
    pub fn navigate_model(&mut self, direction: Direction) {
        let count = self
            .columns
            .get(self.selected_provider)
            .map(|c| c.models.len())
            .unwrap_or(0);
        if count == 0 {
            return;
        }
        match direction {
            Direction::Up => {
                if self.selected_model == 0 {
                    self.selected_model = count - 1;
                } else {
                    self.selected_model -= 1;
                }
            }
            Direction::Down => {
                self.selected_model = (self.selected_model + 1) % count;
            }
            _ => {}
        }
    }

    /// Recompute filtered_indices based on search_query.
    pub fn recompute_filter(&mut self) {
        self.filtered_indices.clear();
        if !self.search_active || self.search_query.is_empty() {
            return;
        }
        let needle = self.search_query.to_lowercase();
        if let Some(col) = self.columns.get(self.selected_provider) {
            for (i, m) in col.models.iter().enumerate() {
                let id_lc = m.model_id.to_lowercase();
                let dn_lc = m.display_name.to_lowercase();
                if id_lc.contains(&needle) || dn_lc.contains(&needle) {
                    self.filtered_indices.push(i);
                }
            }
        }
        self.selected_model = 0;
        // Defensive clamp invariant (Preflight Consensus #6)
        if !self.filtered_indices.is_empty() && self.selected_model >= self.filtered_indices.len() {
            self.selected_model = self.filtered_indices.len() - 1;
        }
    }

    /// Return the effective index into columns[selected_provider].models.
    pub fn selected_model_index(&self) -> Option<usize> {
        if self.search_active && !self.search_query.is_empty() {
            self.filtered_indices.get(self.selected_model).copied()
        } else {
            let col = self.columns.get(self.selected_provider)?;
            if self.selected_model < col.models.len() {
                Some(self.selected_model)
            } else {
                None
            }
        }
    }

    /// Get the currently selected (provider_id, model).
    pub fn selected(&self) -> Option<(&str, &crate::domain::models::provider::ModelDescriptor)> {
        let col = self.columns.get(self.selected_provider)?;
        if col.locked_hint.is_some() {
            return None;
        }
        let idx = self.selected_model_index()?;
        let model = col.models.get(idx)?;
        Some((&col.provider_id, model))
    }

    /// Clone the RefreshTracker handle for spawning background tasks.
    pub fn refreshing_handle(&self) -> Arc<RefreshTracker> {
        Arc::clone(&self.refreshing)
    }
}

impl Default for ModelSelectorState {
    fn default() -> Self {
        Self::new()
    }
}

// ── ProfileSwitcher (Story 8.4 AC-1) ─────────────────────────────────────

/// State for the profile switcher overlay (Ctrl+X, P).
#[derive(Debug, Clone)]
pub struct ProfileSwitcherState {
    pub profiles: Vec<crate::domain::models::ProfileDescriptor>,
    pub selected: usize,
    pub active_index: Option<usize>,
    pub preview: Option<crate::domain::services::swap_tier::TransitionPlan>,
    pub current_dimensions: std::collections::BTreeMap<
        crate::domain::models::PortDimension,
        crate::domain::models::AdapterRef,
    >,
}

impl ProfileSwitcherState {
    pub fn new() -> Self {
        Self {
            profiles: Vec::new(),
            selected: 0,
            active_index: None,
            preview: None,
            current_dimensions: std::collections::BTreeMap::new(),
        }
    }

    pub fn initialize(
        &mut self,
        profiles: Vec<crate::domain::models::ProfileDescriptor>,
        active_name: &str,
        current_dims: std::collections::BTreeMap<
            crate::domain::models::PortDimension,
            crate::domain::models::AdapterRef,
        >,
    ) {
        self.profiles = profiles;
        self.active_index = self.profiles.iter().position(|p| p.name == active_name);
        self.selected = self.active_index.unwrap_or(0);
        self.current_dimensions = current_dims;
        self.preview = None;
    }

    pub fn clear(&mut self) {
        self.preview = None;
    }

    pub fn navigate(&mut self, direction: i32) {
        if self.profiles.is_empty() {
            return;
        }
        let len = self.profiles.len();
        self.selected = ((self.selected as i32 + direction).rem_euclid(len as i32)) as usize;
    }

    pub fn selected_profile(&self) -> Option<&crate::domain::models::ProfileDescriptor> {
        self.profiles.get(self.selected)
    }

    pub fn enter_preview(&mut self, plan: crate::domain::services::swap_tier::TransitionPlan) {
        self.preview = Some(plan);
    }

    pub fn exit_preview(&mut self) {
        self.preview = None;
    }

    pub fn compute_diff_for_selected(
        &self,
        profile: &crate::domain::models::ProfileDescriptor,
    ) -> Option<crate::domain::services::swap_tier::TransitionPlan> {
        if self.current_dimensions.is_empty() {
            return None;
        }
        let plan = crate::domain::services::swap_tier::TransitionPlan::from_selections(
            &self.current_dimensions,
            &profile.selection.dimensions,
            &profile.name,
            profile.identity_color.0,
            None,
        );
        Some(plan)
    }
}

impl Default for ProfileSwitcherState {
    fn default() -> Self {
        Self::new()
    }
}

// ── UsagePanel (Story 7.5 AC3) ────────────────────────────────────────────

/// One row in the panel's "Turn breakdown" section.
#[derive(Debug, Clone)]
pub struct TurnUsageRow {
    pub turn_index: u32,
    pub model: String,
    pub tokens_in: u32,
    pub tokens_out: u32,
    /// `None` when no pricing entry exists for the model (AC6 → "n/a").
    pub cost_usd: Option<f64>,
}

/// Today's session aggregate (Story 7.5 AC3).
#[derive(Debug, Clone, Default)]
pub struct SessionUsageSummary {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: Option<f64>,
    pub task_count: u32,
    pub elapsed_secs: i64,
    pub cache_read_tokens: u64,
    pub cache_total_tokens: u64,
    pub cache_savings_usd: f64,
}

/// Daily budget warning state (Story 7.5 AC5).
#[derive(Debug, Clone, Default)]
pub struct DailyBudgetState {
    pub spent_today_usd: f64,
    pub limit_usd: f64,
    pub computed_at_ms: i64,
    pub dismissed_until_unix: i64,
}

impl DailyBudgetState {
    pub fn percent(&self) -> u32 {
        if self.limit_usd <= 0.0 {
            return 0;
        }
        ((self.spent_today_usd / self.limit_usd) * 100.0)
            .round()
            .min(u32::MAX as f64) as u32
    }
}

/// State for the usage/cost panel overlay (Ctrl+X, U). Story 7.5 AC3 / UX-DR111.
pub struct UsagePanelState {
    /// Logical section cursor (0..4) — visual highlight only.
    pub selected_section: usize,
    pub turn_rows: Vec<TurnUsageRow>,
    pub session_today: SessionUsageSummary,
    pub per_model:
        std::collections::BTreeMap<String, crate::domain::services::cost_calculator::ModelCost>,
    pub missing_pricing_models: Vec<String>,
    /// Context window snapshot at panel-open (in/out + window). Read from
    /// `state.token_usage` / active model at open-time so the panel doesn't
    /// poll providers.
    pub context_used_tokens: u32,
    pub context_window_tokens: u32,
}

impl UsagePanelState {
    pub fn new() -> Self {
        Self {
            selected_section: 0,
            turn_rows: Vec::new(),
            session_today: SessionUsageSummary::default(),
            per_model: std::collections::BTreeMap::new(),
            missing_pricing_models: Vec::new(),
            context_used_tokens: 0,
            context_window_tokens: 0,
        }
    }

    /// Prepare the selection for a fresh Usage frame.
    pub fn initialize(&mut self) {
        self.selected_section = 0;
    }

    pub fn clear(&mut self) {
        self.selected_section = 0;
    }
}

impl Default for UsagePanelState {
    fn default() -> Self {
        Self::new()
    }
}

/// State for the durable-room viewer panel (`Ctrl+X, R` / `/room`).
/// Story 18.3a, AC1.
///
/// **Never presents itself as live.** `room` is one point-in-time fold of the
/// durable room journal under a shared `flock`, produced by
/// [`crate::domain::models::OrchestrationRoom::project_for_host`]. The header
/// says "as of <time>" and the boundary counts entries it has not shown.
#[derive(Default)]
pub struct RoomPanelState {
    /// The exact host-honest fold displayed by this panel.
    pub room: Option<crate::domain::models::OrchestrationRoom>,
    /// Host id the fold was derived against — the "here" that makes a foreign
    /// binding host-bound (AC2). Rendered so the operator can see which host
    /// the honesty claim is relative to.
    pub host_id: String,
    /// Highest journal `seq` the operator has actually seen **rendered**.
    /// Only the renderer advances it: a row is acknowledged when it was drawn,
    /// never when it was merely read.
    pub read_seq: u64,
    /// Highest journal `seq` represented by `room`.
    pub folded_seq: u64,
    /// Highest journal `seq` observed at the durable head. It may be newer than
    /// `folded_seq` while the viewport remains anchored to its current replay.
    pub latest_seq: u64,
    /// Observed head entries that are newer than `read_seq`.
    pub newer_entries: usize,
    /// Unrecognized `RoomEvent` tags in the journal. Rendered as an explicit
    /// unknown row, never silently dropped (UX-DR-ROOM-01).
    pub unknown_records: usize,
    /// First visible index in the current view.
    pub scroll_offset: usize,
    /// Rows the renderer can display; refreshed on every draw.
    pub viewport_rows: usize,
    /// Wall-clock millis of the last read, rendered as "as of …".
    pub read_at_ms: Option<i64>,
    /// A journal read that failed. Shown instead of an empty list, which would
    /// read as "nothing happened".
    pub error: Option<String>,
    /// `true` when this session composed no orchestration journal at all.
    /// Distinct from "the journal is empty" — the first zero-state of three.
    pub not_attached: bool,
    /// Top row and selected row from the last painted Room viewport. These are
    /// per-panel identities, unlike the shared numeric sidebar selection.
    anchor_node: Option<crate::domain::models::AgentId>,
    selected_node: Option<crate::domain::models::AgentId>,
    /// Accumulator for the one-second durable-head check while Room is open.
    head_poll_elapsed_ms: u64,
}

impl RoomPanelState {
    /// Node rows in stable id order. Empty before the first read.
    #[must_use]
    pub fn nodes(&self) -> Vec<&crate::domain::models::NodeView> {
        self.room
            .as_ref()
            .map(|room| room.nodes().values().collect())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn visible_len(&self) -> usize {
        self.room.as_ref().map_or(0, |room| room.nodes().len())
    }

    /// Which of the three distinguishable zero-states applies, if any.
    #[must_use]
    pub fn zero_state(&self) -> Option<RoomZeroState> {
        if self.not_attached {
            return Some(RoomZeroState::NotAttached);
        }
        let room = self.room.as_ref()?;
        if room.nodes().is_empty() {
            return Some(RoomZeroState::NoNodes);
        }
        room.nodes()
            .values()
            .all(|view| view.state.is_terminal())
            .then_some(RoomZeroState::AllTerminal)
    }

    /// The clamped cursor and scroll offset of a list of `len` rows shown through a window of
    /// `viewport_rows`: the cursor stays inside the list and the window follows it. One rule
    /// for the shared state (`synchronize_selection`) and the read-only painter.
    #[must_use]
    pub fn synced(
        len: usize,
        selected: usize,
        scroll_offset: usize,
        viewport_rows: usize,
    ) -> (usize, usize) {
        if len == 0 {
            return (0, 0);
        }
        let viewport_rows = viewport_rows.max(1);
        let selected = selected.min(len - 1);
        let mut scroll = scroll_offset.min(len.saturating_sub(viewport_rows));
        if selected < scroll {
            scroll = selected;
        } else if selected >= scroll.saturating_add(viewport_rows) {
            scroll = selected + 1 - viewport_rows;
        }
        (selected, scroll)
    }

    /// Clamp the cursor into the rendered viewport.
    pub fn synchronize_selection(&mut self, selected: &mut usize) {
        let (cursor, scroll) = Self::synced(
            self.visible_len(),
            *selected,
            self.scroll_offset,
            self.viewport_rows,
        );
        *selected = cursor;
        self.scroll_offset = scroll;
    }

    /// Record the renderer's actual row capacity and reconcile the viewport.
    pub fn set_viewport_rows(&mut self, viewport_rows: usize, selected: &mut usize) {
        self.viewport_rows = viewport_rows.max(1);
        self.synchronize_selection(selected);
    }

    /// Apply what a completed draw of the Room body painted: the real row capacity, the
    /// identities of the first and the cursor row, and, only for a body that was on top and
    /// reached the end of the fold, the acknowledgement of the folded rows.
    pub fn apply_paint(
        &mut self,
        paint: &crate::adapters::tui::widgets::room_panel::RoomPaint,
        selected: &mut usize,
        acknowledge: bool,
    ) {
        self.set_viewport_rows(paint.viewport_rows, selected);
        self.record_rendered_viewport(paint.anchor.clone(), paint.selected.clone());
        if acknowledge && paint.end_reached {
            self.acknowledge_rendered_boundary();
        }
    }

    /// Store one freshly folded durable read without moving the reader's
    /// identity anchors.
    pub fn apply_read(
        &mut self,
        room: crate::domain::models::OrchestrationRoom,
        host_id: String,
        max_seq: u64,
        unknown_records: usize,
        read_at_ms: i64,
        selected: &mut usize,
    ) {
        let anchor_index = self
            .anchor_node
            .as_ref()
            .and_then(|id| room.nodes().keys().position(|candidate| candidate == id));
        let selected_index = self
            .selected_node
            .as_ref()
            .and_then(|id| room.nodes().keys().position(|candidate| candidate == id));

        self.folded_seq = max_seq;
        self.latest_seq = max_seq;
        self.newer_entries = usize::try_from(max_seq.saturating_sub(self.read_seq)).unwrap_or(0);
        self.room = Some(room);
        self.host_id = host_id;
        self.unknown_records = unknown_records;
        self.read_at_ms = Some(read_at_ms);
        self.error = None;
        self.not_attached = false;
        if let Some(index) = anchor_index {
            self.scroll_offset = index;
        }
        if let Some(index) = selected_index {
            *selected = index;
        }
        self.synchronize_selection(selected);
    }

    /// Remember exactly which rows the last render anchored and selected.
    pub fn record_rendered_viewport(
        &mut self,
        anchor: Option<crate::domain::models::AgentId>,
        selected: Option<crate::domain::models::AgentId>,
    ) {
        self.anchor_node = anchor;
        self.selected_node = selected;
    }

    /// Observe the journal head without replacing the anchored replay.
    pub fn observe_head(&mut self, max_seq: u64) -> bool {
        let prior = self.newer_entries;
        self.latest_seq = max_seq;
        self.newer_entries = usize::try_from(max_seq.saturating_sub(self.read_seq)).unwrap_or(0);
        self.error = None;
        self.newer_entries != prior
    }

    /// Advance the low-frequency head-poll clock.
    pub fn head_poll_due(&mut self, tick_ms: u64, interval_ms: u64) -> bool {
        self.head_poll_elapsed_ms = self.head_poll_elapsed_ms.saturating_add(tick_ms);
        if self.head_poll_elapsed_ms < interval_ms {
            return false;
        }
        self.head_poll_elapsed_ms %= interval_ms;
        true
    }

    pub fn reset_head_poll(&mut self) {
        self.head_poll_elapsed_ms = 0;
    }

    /// Mark only the currently folded replay seen. A newer observed head stays
    /// pending until a later reopen folds and renders it.
    pub fn acknowledge_rendered_boundary(&mut self) {
        self.read_seq = self.folded_seq;
        self.newer_entries =
            usize::try_from(self.latest_seq.saturating_sub(self.read_seq)).unwrap_or(0);
    }
}

/// The three zero-states a durable room can be in. A blank pane cannot tell
/// them apart, and that indistinguishability is the failure this exists to
/// prevent (`RV/review-completeness.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomZeroState {
    /// No orchestration journal is composed in this session.
    NotAttached,
    /// The journal is readable and holds no node registrations.
    NoNodes,
    /// Every node in the room has reached a terminal state.
    AllTerminal,
}

/// The four distinguishable zero-states of the `/artifacts` surface.
///
/// One more than the Room panel's three, and the extra one is the point: a
/// room can hold artifacts and still have nothing left to review, which reads
/// identically to "no artifacts" on a blank pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactsZeroState {
    /// No orchestration journal is composed in this session.
    NotAttached,
    /// The journal is readable and holds no artifacts yet.
    NoArtifacts,
    /// Artifacts exist, but none of them is a patch.
    NoPatches,
    /// Every patch in the room has a recorded verdict.
    AllReviewed,
}

/// The durable artifact fold behind Mission Control's Artifacts tab (`Ctrl+X, E` /
/// `/artifacts`) and the artifact window.
/// Story 18.3a-c (AC3). Deliberately shaped like [`RoomPanelState`]: one
/// host-honest fold, an "as of" stamp, per-panel identity anchors, and no head
/// poll. The `/room` head-poll chrome is **not** copied — it is fed by a 1 Hz
/// tick this surface does not have, and copying the string without the wiring
/// ships an unreachable line (`DF-18-3a-c-ARTIFACT-HEAD-POLL`).
///
/// ⚠ [`Default`] is hand-written, not derived: `PermissionMode` has no
/// `Default` and must not acquire one here — a security mode's default is a
/// decision, not a convenience. The unread panel fails closed to
/// `PermissionMode::Plan`, whose disposition is `RefusedPlanMode` for every
/// patch, so a panel that has never been folded can never claim a patch
/// applies.
pub struct ArtifactsPanelState {
    /// The exact host-honest fold displayed by this panel.
    pub room: Option<crate::domain::models::OrchestrationRoom>,
    /// Host id the fold was derived against, rendered as "here:".
    pub host_id: String,
    /// Highest journal `seq` represented by `room`.
    pub folded_seq: u64,
    /// Unrecognized `RoomEvent` tags in the journal — records whose *tag* this
    /// build cannot read, distinct from a recognized record carrying a field
    /// *value* it cannot read (which renders in its own row).
    pub unknown_records: usize,
    /// Wall-clock millis of the last read, rendered as "as of …".
    pub read_at_ms: Option<i64>,
    /// A journal read that failed. Shown instead of an empty list, which would
    /// read as "nothing happened".
    pub error: Option<String>,
    /// `true` when this session composed no orchestration journal at all.
    pub not_attached: bool,
    /// The merge-back policy the apply path uses, carried so the row's
    /// `(policy)` clause and the gate cannot disagree.
    pub policy: crate::domain::services::patch_review::MergeBackPolicy,
    /// The permission mode the fold was rendered under. Read from the same
    /// `SecurityPort` the orchestrator's apply path consults.
    pub permission_mode: crate::domain::models::PermissionMode,
}

impl Default for ArtifactsPanelState {
    fn default() -> Self {
        Self {
            room: None,
            host_id: String::new(),
            folded_seq: 0,
            unknown_records: 0,
            read_at_ms: None,
            error: None,
            not_attached: false,
            policy: crate::domain::services::patch_review::MergeBackPolicy::default(),
            permission_mode: crate::domain::models::PermissionMode::Plan,
        }
    }
}

impl ArtifactsPanelState {
    /// Every artifact in the fold, in the projection's own deterministic
    /// `ArtifactId`-lexicographic order. The fold preserves no other order, and
    /// re-sorting by recency here would be a second read model.
    #[must_use]
    pub fn artifacts(&self) -> Vec<&crate::domain::models::ArtifactRef> {
        self.room
            .as_ref()
            .map(|room| room.artifacts().values().collect())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn visible_len(&self) -> usize {
        self.room.as_ref().map_or(0, |room| room.artifacts().len())
    }

    /// Which of the four distinguishable zero-states applies, if any.
    #[must_use]
    pub fn zero_state(&self) -> Option<ArtifactsZeroState> {
        if self.not_attached {
            return Some(ArtifactsZeroState::NotAttached);
        }
        let room = self.room.as_ref()?;
        if room.artifacts().is_empty() {
            return Some(ArtifactsZeroState::NoArtifacts);
        }
        let patches: Vec<_> = room
            .artifacts()
            .values()
            .filter(|artifact| artifact.kind == crate::domain::models::ArtifactKind::Patch)
            .collect();
        if patches.is_empty() {
            return Some(ArtifactsZeroState::NoPatches);
        }
        patches
            .iter()
            .all(|artifact| {
                matches!(
                    artifact.review,
                    Some(crate::domain::models::ReviewStatus::Reviewed { .. })
                )
            })
            .then_some(ArtifactsZeroState::AllReviewed)
    }

    /// Store one freshly folded durable read.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_read(
        &mut self,
        room: crate::domain::models::OrchestrationRoom,
        host_id: String,
        max_seq: u64,
        unknown_records: usize,
        read_at_ms: i64,
        policy: crate::domain::services::patch_review::MergeBackPolicy,
        permission_mode: crate::domain::models::PermissionMode,
    ) {
        self.folded_seq = max_seq;
        self.room = Some(room);
        self.host_id = host_id;
        self.unknown_records = unknown_records;
        self.read_at_ms = Some(read_at_ms);
        self.policy = policy;
        self.permission_mode = permission_mode;
        self.error = None;
        self.not_attached = false;
    }
}

/// Paint-only feedback; pending_leader remains the menu-presence authority.
pub struct LeaderFeedback {
    pub command: Option<crate::adapters::tui::keymap::CommandId>,
    pub message: std::borrow::Cow<'static, str>,
}

/// State for the command palette overlay (Ctrl+P).
// Covers: UX-DR18
pub struct CommandPaletteState {
    pub filter_text: TextField,
    pub filtered_entries: Vec<PaletteEntry>,
    pub selected_index: usize,
    pub scroll_offset: usize,
    pub current_scope: Option<PaletteScope>,
    /// Incremented on a new frame opening; distinguishes an unchanged empty query.
    pub open_epoch: u64,
    /// Registry refusal shown until the next key handled by this palette.
    pub refusal: Option<&'static str>,
}

impl CommandPaletteState {
    pub fn new() -> Self {
        Self {
            filter_text: TextField::default(),
            filtered_entries: Vec::new(),
            selected_index: 0,
            scroll_offset: 0,
            current_scope: None,
            open_epoch: 0,
            refusal: None,
        }
    }

    /// Initialize a new frame's payload, independently of keyboard ownership.
    pub fn reset(&mut self) {
        self.open_epoch = self.open_epoch.wrapping_add(1);
        self.clear();
    }

    /// Release a closed frame's transient result/filter payload.
    pub fn clear(&mut self) {
        self.filter_text.clear();
        self.filtered_entries.clear();
        self.selected_index = 0;
        self.scroll_offset = 0;
        self.current_scope = None;
        self.refusal = None;
    }

    /// Navigate up or down in the result list (wraps around).
    pub fn navigate(&mut self, direction: Direction) {
        if self.filtered_entries.is_empty() {
            return;
        }
        match direction {
            Direction::Up => {
                if self.selected_index == 0 {
                    self.selected_index = self.filtered_entries.len() - 1;
                } else {
                    self.selected_index -= 1;
                }
            }
            Direction::Down => {
                self.selected_index = (self.selected_index + 1) % self.filtered_entries.len();
            }
            Direction::Left | Direction::Right => {}
        }
        // Keep selected item visible
        let max_visible = 12;
        if self.selected_index < self.scroll_offset {
            self.scroll_offset = self.selected_index;
        } else if self.selected_index >= self.scroll_offset + max_visible {
            self.scroll_offset = self.selected_index + 1 - max_visible;
        }
    }

    /// Update filter text and refresh filtered entries from the registry.
    #[allow(dead_code)]
    pub fn update_filter(&mut self, filter: String, entries: Vec<PaletteEntry>) {
        self.filter_text = filter.into();
        self.filtered_entries = entries;
        self.selected_index = 0;
        self.scroll_offset = 0;
    }

    /// Get the currently selected entry, if any.
    pub fn selected(&self) -> Option<&PaletteEntry> {
        self.filtered_entries.get(self.selected_index)
    }

    /// Execute the selected entry's action. Returns the action if an entry is selected.
    pub fn execute_selected(&self) -> Option<PaletteAction> {
        self.selected().map(|entry| entry.action.clone())
    }
}

impl Default for CommandPaletteState {
    fn default() -> Self {
        Self::new()
    }
}

/// State for the help overlay (? key or Ctrl+X, ?).
// Covers: FR108, UX-DR94
pub struct HelpOverlayState {
    /// Vertical scroll offset within the overlay content.
    pub scroll_offset: usize,
    /// Index of the currently highlighted category (reserved for future navigation).
    pub selected_category: usize,
    pub search: InspectorSearch,
    /// Filtered rows have their own position; clearing restores the unfiltered view.
    pub filtered_scroll_offset: usize,
}

impl HelpOverlayState {
    pub fn new() -> Self {
        Self {
            scroll_offset: 0,
            selected_category: 0,
            search: InspectorSearch::default(),
            filtered_scroll_offset: 0,
        }
    }

    /// Initialize the payload of a newly opened Help frame.
    pub fn reset(&mut self) {
        self.scroll_offset = 0;
        self.selected_category = 0;
        self.search = InspectorSearch::default();
        self.filtered_scroll_offset = 0;
    }

    pub(crate) fn list_scroll_mut(&mut self) -> &mut usize {
        if self.search.query.is_empty() {
            &mut self.scroll_offset
        } else {
            &mut self.filtered_scroll_offset
        }
    }
}

impl Default for HelpOverlayState {
    fn default() -> Self {
        Self::new()
    }
}

/// Story 14.3b — live snapshot of an in-flight fork-join wave, tracked so the
/// render path (14.3a SynthesisBlock/WaveStrip) can paint progress. Updated by
/// the 14.3 orchestration `AppEvent` handlers; `None` when no wave is active.
#[derive(Clone, Debug, Default)]
pub struct WaveViewState {
    /// The coordinator (root) agent that owns the wave.
    pub coordinator: crate::domain::models::AgentId,
    /// Total spokes dispatched in the wave.
    pub spoke_count: usize,
    /// Spokes that have reached a terminal state so far.
    pub completed_count: usize,
    /// `Some(true)` when the synthesis surfaced an honest-empty floor (AC7).
    pub honest_empty: Option<bool>,
    /// `true` once the wave was cancelled (WaveCancelled).
    pub cancelled: bool,
}

impl WaveViewState {
    /// PATCH-2 (14-3b review): the 4 fork-join wave-lifecycle handlers,
    /// extracted from the `event_loop.rs` match arms into pure, unit-testable
    /// methods. Each applies a DISTINCT delta (the render path paints the
    /// WaveStrip from this state). The event loop delegates to these; the
    /// per-handler unit tests + the 4 mutants (a handler whose body is dropped
    /// → its delta vanishes → its test goes red) prove non-vacuity (Murat
    /// vacuity ledger #3 — DF-309 four mutants, one per AppEvent arm).

    /// `AppEvent::ForkJoinStarted`: open the wave view (`None → Some`).
    pub fn started(coordinator: crate::domain::models::AgentId, spoke_count: usize) -> Self {
        Self {
            coordinator,
            spoke_count,
            completed_count: 0,
            honest_empty: None,
            cancelled: false,
        }
    }

    /// `AppEvent::SpokeCompleted`: bump the completed spoke count.
    pub fn record_spoke_completed(&mut self) {
        self.completed_count = self.completed_count.saturating_add(1);
    }

    /// `AppEvent::SynthesisReady`: record the honest-empty flag (AC7).
    pub fn record_synthesis_ready(&mut self, honest_empty: bool) {
        self.honest_empty = Some(honest_empty);
    }

    /// `AppEvent::WaveCancelled`: mark the wave cancelled.
    pub fn record_wave_cancelled(&mut self) {
        self.cancelled = true;
    }
}

/// TUI-specific state for rendering.
pub struct TuiState {
    pub active_tab_id: TabId,
    pub focus: FocusState,
    pub thread_inspector: Option<ThreadInspectorState>,
    /// Session-local outcome lines of item- and node-targeted acts (Story 21.8 A13): a
    /// re-opened thread shows them again.
    pub thread_outcomes: crate::adapters::tui::thread_inspector::ThreadOutcomes,
    pub recipient_actions: BTreeMap<
        (
            crate::adapters::daemon::protocol::RecipientAction,
            crate::adapters::daemon::protocol::RecipientActionTarget,
        ),
        RecipientActionState,
    >,
    pub recipient_context_ids: std::collections::BTreeSet<String>,
    /// Story 19.25 `AC3` — client-local fields left out of a retrospective
    /// preview, per item id. Cleared when the act settles or the card leaves.
    pub retro_left_out: HashMap<String, crate::domain::models::RetroFieldSet>,
    pub needs_redraw: bool,
    pub terminal_width: u16,
    pub terminal_height: u16,
    /// Height of the chat pane viewport (set after each render).
    /// Used for scroll clamping, message targeting, and status bar display.
    pub viewport_height: u16,
    pub input_buffer: String,
    pub cursor_position: usize,
    pub status: StatusState,
    /// Cumulative token usage for the current session.
    pub token_usage: Option<UsageInfo>,
    pub should_quit: bool,
    pub theme: Theme,
    /// Scroll position snapshot synced from `view_state.scroll_offset` before
    /// each render/input cycle. Read by app.rs boundary handlers and event_loop.
    /// NEVER write directly — use `dispatch_view_scroll` or `reconcile_fold_toggle`.
    pub(crate) scroll_snapshot: usize,
    /// Auto-scroll snapshot synced from `view_state.mode == Following`.
    /// Marked pub(crate) to prevent external crate access (tests migrate to view_state reads).
    pub(crate) auto_snapshot: bool,
    pub total_content_height: usize,
    /// Line offsets for each content block boundary in rendered view.
    pub block_boundaries: Vec<usize>,
    /// Line offsets for each message boundary (all roles) in rendered view.
    /// Drives the status-bar position counter and rewind/fork targeting.
    pub message_boundaries: Vec<usize>,
    /// Line offsets for each **user** message boundary in rendered view.
    /// Drives `{`/`}` jump-between-turn navigation.
    pub user_message_boundaries: Vec<usize>,
    /// Per-tab render states for height cache and related render metadata.
    /// Side-table pattern respects hexagonal architecture (TabState in domain/
    /// cannot hold adapter-side HeightCache).
    pub tab_render_states: HashMap<TabId, TabRenderState>,
    /// Pending anchor message index for resize scroll preservation.
    /// Set by resize handler, consumed by next render to recompute scroll_offset.
    pub pending_anchor: Option<usize>,
    /// Currently focused tool block id (set by chat pane render when a tool block
    /// is at the top of the viewport after J/K navigation).
    pub focused_item_id: Option<String>,
    /// Explicitly selected tool block id — set ONLY by user intent (the `Tab`
    /// cycle inside a focused turn or `Enter` inspecting a call), never by
    /// the render pass. The nearest-visible fallback in `find_focused_item`
    /// seats a block whenever any is on screen; copying must instead keep the
    /// last assistant message until a tool is explicitly selected.
    /// `resolve_copy_content` reads THIS field; `focused_item_id` stays the
    /// render-derived signal that drives `Enter`.
    pub selected_item_id: Option<String>,
    /// Active retry state for provider error recovery.
    pub retry_state: Option<RetryState>,
    /// Feedback blocks displayed in conversation, keyed by block ID.
    pub feedback_blocks: BTreeMap<String, FeedbackBlock>,
    /// Story 19.17: per-action rows remain typed beside their visible blocks.
    pub team_send_blocks:
        BTreeMap<String, Vec<crate::adapters::tui::handlers::team_command::TeamSendRow>>,
    pub team_send_next_id: u64,
    /// Source of the sent-thread read generations (`/team thread` and reopen).
    pub thread_read_generation: u64,
    /// The session decision queue (Story 21.6): every question the session asks,
    /// keyed `(kind, source, conversation)`; its items own the payloads the old
    /// per-kind slots held (a large image, a delete target).
    pub decisions: crate::adapters::tui::decision_queue::DecisionQueue,
    /// When the last printable key was typed, from the injected clock (RU-6: an
    /// arriving decision never takes focus from someone who is typing).
    pub last_printable_at: Option<Instant>,
    /// Story 16.6: pending vim `z`-prefix chord (za/zc/zo/zM/zR/zs/zz). AC10
    pub pending_z: bool,
    /// Story 16.6: pending vim `]`/`[` bracket chord (]]/[[/]P).
    /// Story 16.8 preflight rebinding (2026-05-03) added `]P` as the home for
    /// `JumpToLatestProseAnchor` (relocated from S16.6's `G` binding per ADR-16-03).
    /// AC10
    pub pending_bracket: Option<char>,
    /// Story 16.8: pending `g`-prefix chord for `gg` detection. AC2
    /// Single-`g` immediately fires ScrollToTop (legacy alias preserved) AND
    /// sets this flag; a follow-up `g` produces the idempotent `gg` chord.
    /// Reset on any non-`g` key or non-key event via the chord-reset guard.
    pub pending_g: bool,
    /// S16.8 AC15: Two-stage anchor-confirmation gate.  When the user is Pinned
    /// and emits a scroll-intent event, this field records the first tick's
    /// instant.  A second scroll-intent within 2000ms drops the anchor via
    /// `ViewEvent::DropAnchorAndScroll`.  Reset on `]]`/`[[` or mode change.
    pub pending_anchor_drop: Option<std::time::Instant>,
    /// Whether project context files were loaded (for status bar indicator).
    pub has_project_context: bool,
    /// Session-scoped input history for Up/Down and Ctrl+R.
    // Covers: UX-DR74
    pub input_history: InputHistory,
    /// Whether multi-line mode is active (toggled by Alt+M or `/ml`).
    // Covers: UX-DR76
    pub multiline_mode: bool,
    /// Story 11.4 (AC7) — whether memory-based context injection is enabled for
    /// this session. Toggled by `/context off` / `/context on`. Default `true`.
    /// Session-scoped: persists across `/new` (like `selected_model`, Q3). When
    /// `false`, the turn seam fully short-circuits — zero `MemoryPort` calls.
    /// Project context (CLAUDE.md) is unaffected (it is structural, persona-owned).
    pub context_injection_on: bool,
    /// Story 11.4 (Task 4.4) — the LAST assembled `ContextBundle`, cached so
    /// `/context show` and the status-bar token cost read it without re-running
    /// `assemble()`. `None` until the first injected turn.
    pub last_context_bundle: Option<crate::domain::models::ContextBundle>,
    /// Story 11.6 (Task 7) — the model's full context window for the active
    /// model, in tokens. Refreshed wherever the status bar recomputes it; read
    /// by `start_turn_inner` as the `WindowingAssembler` budget. `0` = unknown
    /// (guarded to `usize::MAX` → no trim). Distinct from `last_context_bundle`
    /// (Content tier) — this drives the Message-tier assembler budget.
    pub active_context_window: u32,
    /// Story 11.6 (Task 7) — the LAST Message-tier `AssembleDiagnostics` (group
    /// count, active group, tokens saved) from the context assembler, cached so
    /// `/context show` can render group info. Kept SEPARATE from
    /// `last_context_bundle` (the two tiers are distinct). `None` for the
    /// passthrough strategy / before the first windowed turn.
    pub last_assembler_diagnostics: Option<crate::domain::models::AssembleDiagnostics>,
    /// State for reverse search overlay (Ctrl+R).
    // Covers: UX-DR74
    pub reverse_search: ReverseSearchState,
    /// State for the within-conversation search overlay (Ctrl+F).
    // Covers: UX-DR86, Story 4-4 AC1-AC7
    pub search_state: SearchState,
    /// State for the cross-conversation search overlay (`/` in the History window).
    // Covers: UX-DR87, Story 4-4 AC5-AC7
    pub cross_search: CrossSearchState,
    /// Vertical scroll offset within the input box when lines exceed max height.
    pub input_scroll_offset: usize,
    /// Autocomplete popup state (/ commands and @ file mentions).
    // Covers: UX-DR75
    pub autocomplete: AutocompleteState,
    /// Resolved file mentions from autocomplete selections in the current input.
    /// Cleared on submit. Used at send time to attach file context.
    pub resolved_mentions: Vec<ResolvedMention>,
    /// Images queued for the next message submission (clipboard paste or file mention).
    // Covers: FR112
    pub pending_images: Vec<ImageAttachment>,
    /// Visual indicator for attached images shown in the input box.
    // Covers: FR112
    pub image_indicator: Option<String>,
    /// Command palette state (Ctrl+P).
    // Covers: UX-DR18
    pub command_palette: CommandPaletteState,
    /// Untimed pending leader: the context at Ctrl+X, never a second focus owner.
    pub pending_leader: Option<crate::adapters::tui::keymap::Context>,
    pub leader_feedback: Option<LeaderFeedback>,
    /// Shared frame inventory; only the presentation reducer mutates it.
    pub modal_stack: crate::adapters::tui::presentation::ModalStack,
    /// Frame-owned tool bodies and viewers; only the reducer creates/releases entries.
    pub tool_inspectors: crate::adapters::tui::tool_inspector::ToolInspectors,
    /// Frame-owned S-BLOCK payloads; only the reducer creates/releases entries.
    pub block_viewers: crate::adapters::tui::block_viewer::BlockViewers,
    /// S-QUIT payload, present exactly while its stack frame is open.
    pub quit_confirm: Option<crate::adapters::tui::quit_confirm::QuitConfirmState>,
    /// Session notification store (Story 21.1): every transient notice of the
    /// host lives here; producers call `notifications::notify`.
    pub notifications: crate::adapters::tui::notifications::NotificationStore,
    /// S-LOG payload, present exactly while its stack frame is open.
    pub notification_log: Option<crate::adapters::tui::notification_log::NotificationLogState>,
    /// Help overlay state (? key or Ctrl+X, ?).
    // Covers: FR108, UX-DR94
    pub help_overlay: HelpOverlayState,
    /// Model/provider selector state (Ctrl+X, M) (Story 7.2).
    pub model_selector: ModelSelectorState,
    /// Sign-in overlay (`/login`, Ctrl+X I, locked provider → sign in).
    pub login: LoginOverlayState,
    /// Profile switcher state (Ctrl+X, P) (Story 8.4 AC-1).
    pub profile_switcher: ProfileSwitcherState,
    /// Active-profile snapshot — identity color + name for status bar.
    /// Initialized at startup; updated on ProfileSwitched events.
    pub active_profile: Option<crate::domain::models::ActiveProfileSnapshot>,
    /// Session-scoped adapter overrides (Story 8.5 AC-5). BTreeMap for
    /// deterministic iteration in panel + tests. Single-writer rule: only
    /// the override handler and startup hook mutate this.
    pub session_overrides: std::collections::BTreeMap<
        crate::domain::models::PortDimension,
        crate::domain::models::AdapterRef,
    >,
    /// Usage/cost panel state (Ctrl+X, U) (Story 7.5 AC3).
    pub usage_panel: UsagePanelState,
    /// Story 19.16g — the transparency-log reminder's value state.
    pub log_awareness: LogAwarenessView,
    /// Story 19.16g — the active tab's unpresented `team-log` visit (saved
    /// and restored with the tab's feedback blocks).
    pub pending_log_visit: Option<crate::domain::models::LogVisitCandidate>,
    /// The active local tab is a read-only (subagent output) view: the thread frame offers no
    /// retract there (synced by the local event loop on every input and paint; always false
    /// in the attached host, which gates on its own read-only flag).
    pub local_read_only: bool,
    /// Durable room viewer panel state (Ctrl+X, R / `/room`) (Story 18.3a AC1).
    pub room_panel: RoomPanelState,
    /// The Mission Control hub's session-global payload (Story 21.7, A1).
    pub mission_control: crate::adapters::tui::mission_control::MissionControlState,
    /// The artifact window's payload (Story 21.7 A15): set by the reducer's opener, cleared
    /// when the frame closes.
    pub artifact_window: Option<crate::adapters::tui::artifact_window::ArtifactWindowState>,
    /// The agent window's payload (Story 21.7 A7): set by the reducer's opener, cleared when
    /// the frame closes.
    pub agent_window: Option<crate::adapters::tui::agent_window::AgentWindowState>,
    /// The task window's payload (Story 21.7 A8): set by the reducer's opener, cleared when
    /// the frame closes.
    pub task_window: Option<crate::adapters::tui::task_window::TaskWindowState>,
    /// The Adapters window's payload (Story 21.7 A21): the health snapshot taken at open.
    pub adapters_window: Option<crate::adapters::tui::adapters_window::AdaptersWindowState>,
    /// The History window's payload (Story 21.7 A21): a snapshot of the session index, set by
    /// the reducer's opener, cleared when the frame closes.
    pub history_window: Option<crate::adapters::tui::history_window::HistoryWindowState>,
    /// Durable artifact list panel state (Ctrl+X, E / `/artifacts`) (Story
    /// 18.3a-c AC3).
    pub artifacts_panel: ArtifactsPanelState,
    /// Daily budget warning state (Story 7.5 AC5). `None` when budget disabled.
    pub daily_budget: Option<DailyBudgetState>,
    /// `(day start, limit)` the user chose `Continue` on (A18): no re-raise while both hold.
    pub budget_acknowledged: Option<(i64, u64)>,
    /// Captured resolved-model from `start_turn_inner`, consumed-and-cleared on
    /// the first subsequent `ChunkAction::TurnComplete` to stamp `Turn.model`
    /// (Story 7.5 AC1.5).
    pub pending_resolved_model: Option<String>,
    /// Session-scoped active-model override (Story 7.2 AC3).
    /// `None` = use `config.model`. Runtime hot-swap — NOT a config mutation.
    pub selected_model: Option<String>,
    /// Cached multiplexer detection (tmux/screen) — set once at startup.
    // Covers: UX-DR62
    pub multiplexer_detected: bool,
    /// Whether the terminal is VS Code's integrated terminal — set once at startup.
    // Covers: Sprint Change Proposal 2026-04-08, AC#4, AC9 (3-6a-D2)
    pub is_vscode: bool,
    /// How many TUI sessions this user has started (loaded from global config).
    /// Used to fade contextual hints after the first N sessions.
    // Covers: UX-DR96
    pub session_count: u32,
    /// `[onboarding] persistent_hints` (explicit or from the team layer, Story 21.14 D17): the
    /// contextual status hints never fade with the session count.
    pub hints_persistent: bool,
    /// Current contextual status-bar hint for discoverability.
    /// `None` once session_count > theme.timing.status_hint_fade_sessions.
    // Covers: UX-DR93
    pub current_hint: Option<String>,
    /// Story 21.13 (D5): the local Stage's fenced-code cap in rows, from
    /// `[markdown] code_max_rows` (clamped to `[1, 1000]`, default 30).
    pub code_max_rows: u16,
    /// Story 21.9 (D9): `[markdown] reasoning` — how the local Stage shows
    /// committed reasoning (default folded).
    pub reasoning_display: crate::domain::models::ReasoningDisplay,
    /// Story 21.9 (D17): the workspace and live provider-lock query the local
    /// launchpad refresh reads. `None` (tests, previews) skips the refresh.
    pub launchpad_env: Option<crate::adapters::tui::launchpad::LaunchpadEnv>,
    /// Story 21.3 A12: (custom command name, discovery generation) already announced.
    pub shadows_announced: std::collections::BTreeSet<(String, u64)>,
    /// `Ctrl+X w` (OD-2, A16): the user wants the read-only monitor column. Default off; the
    /// column is painted only while this is set AND the terminal is at least 120 columns wide,
    /// and the preference is kept when the terminal narrows.
    // Covers: FR107, UX-DR20
    pub monitor: bool,
    /// The onboarding tour (Story 21.14 D1): session-global, survives tab switches. `None`
    /// unless the local event loop installed it (`TuiState::new` and the attached host never
    /// have a tour).
    pub tour: Option<crate::adapters::tui::tour::TourState>,
    /// Where the tour's "done" mark is written (`<config_dir>/state.toml`): installed only by
    /// the event loop at startup, `None` in tests, so no test can touch a real config directory.
    pub tour_persist: Option<crate::adapters::tui::tour::TourPersist>,
    /// Story 6-3: the task actions' state (the plan the Tasks tab resolves first, the reorder).
    pub task_panel_state: TaskPanelState,
    pub agent_panel_state: AgentPanelState,
    /// Currently selected row in the bookmark list panel (AC10). `0` when
    /// the panel is closed.
    // Covers: Story 4-4 AC10
    pub bookmark_list_selected: usize,
    /// Mirror of the active tab's bookmark count — kept in sync by the event
    /// loop whenever the panel opens or a bookmark is added / removed. The
    /// key-handler layer in `app.rs` reads this to clamp `j`/`Down` to the
    /// upper bound (Story 4-4 AC10 clamping requirement).
    pub bookmark_list_count: usize,
    /// Actual tab owning the live Bookmark frame, not its saved conversation mirror.
    pub bookmark_owning_tab: Option<TabId>,
    /// Single-entry undo buffer for bookmark list deletes (AC10 synthesis).
    /// Ownership is retained after ordinary close; expiry and Stage replacement
    /// discard it without restoring into another conversation or tab.
    // Covers: Story 4-4 AC10 amendment
    pub bookmark_undo_buffer: Option<BookmarkUndo>,
    /// Skill registry — populated by background discovery task (Story 5-1 AC6).
    /// Shared via Arc<tokio::sync::RwLock> between TuiState and SkillActivator
    /// so autocomplete and tool-dispatch see the same catalog (DF-159, Story 16-0 AC1).
    pub skill_registry: Arc<tokio::sync::RwLock<SkillRegistry>>,
    /// Synchronous cache of skill names for the hot input path (Story 16-0).
    /// Refreshed from the shared registry whenever skills are discovered.
    pub skill_name_cache: HashSet<String>,
    pub active_skill_count: usize,
    pub agent_registry: AgentRegistry,
    pub agent_suggestions: Vec<AutocompleteSuggestion>,
    pub active_agent_name: Option<String>,
    /// Pending agent activation queued while background discovery is in-flight.
    /// Set by `submit_message` when registry is not yet discovered;
    /// consumed by `AgentsDiscovered` handler in the event loop.
    pub pending_agent_activation: Option<(String, Option<String>)>,
    pub team_proposal_conversation_id: Option<String>,
    #[cfg(feature = "a2a")]
    pub team_proposal_surface_tab_count: usize,
    #[cfg(feature = "a2a")]
    pub team_proposal_withdrawal_pending: bool,
    /// Story 6-2a: pending AgentThenSubmit (synthetic task turn) queued
    /// when the event arrives while a stream is still active. Dispatched
    /// after the stream completes (TurnComplete handler).
    pub pending_agent_then_submit: Option<(String, bool)>,
    /// Which assistant-turn count the last plan reminder was injected at.
    /// `None` when no reminder is pending.
    pub pending_plan_reminder_at_turn: Option<u32>,
    /// Active plan file path when in Plan mode. `None` otherwise.
    pub plan_file_path: Option<std::path::PathBuf>,
    /// Story 7.4: whether a compaction operation is in progress.
    pub compacting: bool,
    /// Story 7.4: current context warning level.
    pub context_warn_level: ContextWarnLevel,
    /// Story 7.4: pending context carryover for fresh tab + summary injection.
    pub pending_context_carryover: Option<String>,
    /// Story 14.4 — last attributed subagent event delivered through AppEvent::Subagent.
    /// Minimal R1 surface: retained for handler/conformance observability; richer
    /// live-status widgets sequence after the bus substrate.
    pub last_subagent_envelope: Option<crate::domain::models::SubagentEnvelope>,
    /// Story 14.3b — live snapshot of the current fork-join wave (set by the
    /// 14.3 `AppEvent` handlers). `None` when no wave is active. The render
    /// path (14.3a) reads this to paint the WaveStrip/SynthesisBlock.
    pub wave_state: Option<WaveViewState>,
    /// Story 14.3a (F1+F2+F5) — the retained wave handle for drill/diverge/
    /// rerun/cancel. Domain trait object → adapters→domain only (the
    /// adapters→infra crossing from F5 is gone). NOT a field of WaveViewState
    /// (which is Clone/Default for the push-counter regime).
    pub wave_run: Option<std::sync::Arc<dyn crate::domain::ports::wave_handle::WaveHandle>>,
    /// Test seam: when set, the decision-frame `c` copy goes through this copier
    /// instead of the real clipboard (`None` in production).
    pub clipboard_copier: Option<
        std::sync::Arc<
            dyn Fn(&str) -> crate::adapters::tui::clipboard::ClipboardResult + Send + Sync,
        >,
    >,
    /// Story 14.3a — wave-overlay scroll state. The selected row in the
    /// virtual-scrolled WaveOverlay (j/k to scroll, 0-indexed).
    pub wave_overlay_selected: usize,
    /// Story 14.3a — slot currently being rerun (in-progress lamp for AC11).
    /// Set on SpokeRerunStarted, cleared on Replaced/Reverted terminal.
    pub rerunning_slot: Option<usize>,
    /// Story 14.3a — the wave-cancel root token. Held by the event loop so
    /// Ctrl-C / `/fanout cancel` can fire it (AC8). `None` when no wave is
    /// live. The token is also threaded into `run_wave` and rerun child tokens.
    pub wave_cancel: Option<tokio_util::sync::CancellationToken>,
    /// Story 14.3c — `d` opens the divergent comparison pivot; `s` returns to
    /// the synthesis surface. Separate from focus so `s` can be an involution
    /// back to the regular chat-pane wave surface.
    pub wave_diverge_open: bool,
    /// Frame subview uses the legacy ChatWave return keys after divergence.
    /// Presence belongs only to the WaveOverlay frame, never to this mode flag.
    pub wave_chat_keys: bool,
    /// P8 (review): the last-drilled spoke body, rendered inline in the chat
    /// pane instead of being dumped as a SystemNotice. `None` when no drill is
    /// open. Cleared on `CloseDivergeView` / `CloseWaveOverlay`.
    pub wave_drill_body: Option<(usize, String)>,
    /// D-B (AI-12.3): the identity of the currently-fanning-out `/fanout`
    /// spawn — the SINGLE SOURCE OF TRUTH for "a wave is in flight". Stamped
    /// at spawn (`begin_wave_fanout`), cleared at WaveRunReady /
    /// WaveCancelled / WaveTerminated. Identity-coupled (monotonic per spawn)
    /// so a stale bool can never survive: `is_wave_in_flight()` is derived,
    /// never shadowed. Replaces the old free-standing `wave_in_flight: bool`
    /// whose only defect was that it could desync from the spawn outcome.
    pub active_wave_id: Option<u64>,
    /// D-B (AI-12.3): monotonic wave-id source backing `active_wave_id`.
    /// Stays private — `begin_wave_fanout` is the only writer.
    wave_id_counter: u64,
}

impl TuiState {
    /// Sessions after which the contextual status hint fades: the theme's threshold, or never
    /// while `[onboarding] persistent_hints` is on. The one source for every hint call site.
    #[must_use]
    pub fn hint_fade_sessions(&self) -> u32 {
        if self.hints_persistent {
            u32::MAX
        } else {
            self.theme.timing.status_hint_fade_sessions
        }
    }

    /// Story 19.16g — before a draw: a visit painted by a frame that never completed was not
    /// presented, so the Messages tab's paint mark is cleared and the visit stays pending.
    pub fn restore_unflushed_log_visits(&mut self) {
        self.mission_draw_started();
    }

    /// Story 19.16g, re-homed by 21-7 (A13) — after a **successful** draw: hand the visit this
    /// frame actually presented to the persistence shell. The `/team log` visit counts only if
    /// Messages is the top frame (no leader menu over it) and its body was painted with at
    /// least one row; the flag is consumed once, so rerendering cannot acknowledge later rows.
    pub fn log_visits_presented(&mut self) {
        self.mission_draw_completed();
        let painted = self.mission_control.take_messages_painted();
        if painted
            && crate::adapters::tui::mission_control::is_top(self)
            && self.mission_control.tab == crate::adapters::tui::mission_control::McTab::Messages
            && self.pending_leader.is_none()
            && let Some(visit) = self.pending_log_visit.take()
        {
            self.log_awareness.presented.push(visit);
        }
    }

    /// D-B (AI-12.3): true while a `/fanout` wave is in flight (spawned but not
    /// yet delivered or cancelled). DERIVED from `active_wave_id` — never a
    /// shadow boolean. Gates the Ctrl-C wave-cancel branch so a completed
    /// wave's stale `wave_cancel` token can't intercept the next Ctrl-C.
    pub fn is_wave_in_flight(&self) -> bool {
        self.active_wave_id.is_some()
    }
    /// D-B (AI-12.3): stamp a fresh monotonic wave id + arm the lifecycle
    /// marker. Called at `/fanout` spawn. Returns the id so the spawn closure
    /// can correlate terminal events (defended against a stale late delivery).
    pub fn begin_wave_fanout(&mut self) -> u64 {
        // Derive the next id from the struct's own counter so it is unique per
        // TuiState (no global). Reads the stored counter, bumps it.
        let id = self.wave_id_counter.wrapping_add(1);
        self.wave_id_counter = id;
        self.active_wave_id = Some(id);
        id
    }
    /// D-B (AI-12.3): retire the in-flight marker. Called by WaveRunReady /
    /// WaveCancelled / WaveTerminated. `Some(id)` matches the stamp; any stale
    /// late delivery (id mismatch) is a no-op so a NEWER wave's marker can't be
    /// clobbered by an OLDER terminal.
    pub fn retire_wave_fanout(&mut self, id: Option<u64>) {
        match (self.active_wave_id, id) {
            (Some(live), Some(evt)) if live == evt => self.active_wave_id = None,
            (Some(_), None) => self.active_wave_id = None, // uncorrelated retire (Ctrl-C self-heal)
            _ => {} // stale or already-retired — leave the live marker alone
        }
    }

    // ─── 14.3c AI-12.3 remediation (DF-CR-14-3c-6): wave-lifecycle handlers ──
    //
    // The wave-cancel + rerun `event_loop.rs` `select!` arms, extracted into
    // sync, I/O-free methods (the 14.3b PATCH-2 pattern). The production arms
    // CALL these; the AC8/AC11 keystones DRIVE these — neither re-implements the
    // state transition inline. A handler whose body is mutated (wrong field /
    // wrong slot / dropped delta) makes its keystone go red.

    /// Ctrl-C / `/fanout cancel` ABORT TRIGGER (AC8). If a wave is in flight,
    /// fire the wave-cancel root token (cancel-all → every spoke child via the
    /// CancellationToken tree), clear the rerun lamp, and self-heal the in-flight
    /// marker. Returns `true` when a cancel was fired (the caller consumes the
    /// Ctrl-C instead of quitting). Gated on `is_wave_in_flight()` so a completed
    /// wave's stale token can't swallow the next Ctrl-C.
    pub fn request_wave_cancel(&mut self) -> bool {
        if !self.is_wave_in_flight() {
            return false;
        }
        if let Some(cancel) = self.wave_cancel.as_ref() {
            cancel.cancel();
        }
        self.rerunning_slot = None;
        self.retire_wave_fanout(None);
        self.needs_redraw = true;
        true
    }
    /// `AppEvent::Subagent` (Story 14.4): retain the attributed envelope and
    /// request a redraw. The event loop delegates here so the wiring guard can
    /// detect an orphaned AppEvent variant.
    pub fn handle_subagent_envelope(&mut self, envelope: crate::domain::models::SubagentEnvelope) {
        self.last_subagent_envelope = Some(envelope);
        self.needs_redraw = true;
    }

    /// `AppEvent::WaveCancelled` (AC8): mark the wave view cancelled + retire the
    /// in-flight marker.
    pub fn handle_wave_cancelled(&mut self) {
        if let Some(wave) = self.wave_state.as_mut() {
            wave.record_wave_cancelled();
        }
        self.retire_wave_fanout(None);
        self.needs_redraw = true;
    }

    /// `AppEvent::WaveRunReady` (AC11): retain (swap in) the new wave handle. If
    /// a rerun was in flight, clear the lamp and return `Some(slot)` so the
    /// caller emits the "Spoke {slot} updated" notice.
    ///
    /// **Does NOT cancel the swapped-out handle.** In the production CoW rerun
    /// path the prior and new handle SHARE the same `wave_cancel` token
    /// (`orchestrator/mod.rs:754` — `wave_cancel: prev.wave_cancel.clone()`), so
    /// a naive `old.cancel()` would also cancel the freshly-swapped-in handle.
    /// The no-leak property holds structurally instead: WaveRunReady only fires
    /// after the wave is terminal, and the in-flight guard blocks a concurrent
    /// wave. The `ac11_rerun_swap_leaves_new_handle_live` keystone guards this.
    pub fn handle_wave_run_ready(
        &mut self,
        handle: std::sync::Arc<dyn crate::domain::ports::wave_handle::WaveHandle>,
    ) -> Option<usize> {
        let rerun_slot = self.rerunning_slot.take();
        self.retire_wave_fanout(None);
        self.wave_run = Some(handle);
        self.needs_redraw = true;
        rerun_slot
    }

    /// `AppEvent::SpokeRerunStarted` (AC11): set the per-slot in-progress lamp.
    /// MUST NOT touch `completed_count` (counter-inert — LF4-b).
    pub fn handle_spoke_rerun_started(&mut self, slot: usize) {
        self.rerunning_slot = Some(slot);
        self.needs_redraw = true;
    }

    /// `AppEvent::SpokeRerunReverted` (AC11): clear the lamp IFF it matches the
    /// reverted slot (a stale revert for another slot must not clear a live lamp).
    pub fn handle_spoke_rerun_reverted(&mut self, slot: usize) {
        if self.rerunning_slot == Some(slot) {
            self.rerunning_slot = None;
        }
        self.needs_redraw = true;
    }

    /// `AppEvent::WaveTerminated` (D-B): correlated retire — a stale late delivery
    /// (`id` != the live marker) is a no-op so an older wave's terminal can't
    /// retire a NEWER live marker.
    ///
    /// F2 (AI-12.3 post-review party-mode): a TERMINATED wave is dead, so when
    /// this terminal matches the live wave we retire the full view — not just
    /// the lifecycle marker. Otherwise `wave_state` lingers after failure and
    /// the two guards diverge: the `/fanout` launch-guard (`wave_state.is_some()`,
    /// event_loop.rs) blocks the next fan-out while the cancel-guard
    /// (`is_wave_in_flight()` == `active_wave_id`) is already false — so Ctrl-C
    /// no longer cancels and falls through to `should_quit`, silently quitting
    /// the app. `WaveTerminated` is failure-only (success emits `WaveRunReady`),
    /// so clearing the view here never touches the success/rerun display; the
    /// id-match guard preserves the stale-late-delivery no-op.
    pub fn handle_wave_terminated(&mut self, id: u64) {
        let was_live = matches!(self.active_wave_id, Some(live) if live == id);
        self.retire_wave_fanout(Some(id));
        if was_live {
            self.wave_state = None;
            self.wave_cancel = None;
            self.rerunning_slot = None;
        }
        self.needs_redraw = true;
    }

    /// Story 14.3c: dismiss the fan-out wave display — drop the retained handle
    /// and clear all wave view state so the chat pane returns to normal. Driven
    /// by the Esc-unwind affordance once a wave is completed/cancelled. Idempotent.
    pub fn dismiss_wave(&mut self) {
        self.wave_run = None;
        self.wave_cancel = None;
        self.wave_state = None;
        self.rerunning_slot = None;
        self.wave_drill_body = None;
        self.wave_diverge_open = false;
        self.wave_chat_keys = false;
        self.wave_overlay_selected = 0;
        self.active_wave_id = None;
        self.needs_redraw = true;
    }
}

impl TuiState {
    #[allow(dead_code)]
    pub fn new(width: u16, height: u16) -> Self {
        Self::with_capability(width, height, ColorCapability::TrueColor)
    }

    pub fn with_capability(width: u16, height: u16, capability: ColorCapability) -> Self {
        Self {
            active_tab_id: 0,
            focus: super::presentation::reducer::initial_base(),
            thread_inspector: None,
            thread_outcomes: Default::default(),
            recipient_actions: BTreeMap::new(),
            recipient_context_ids: std::collections::BTreeSet::new(),
            retro_left_out: HashMap::new(),
            needs_redraw: true,
            terminal_width: width,
            terminal_height: height,
            viewport_height: height.saturating_sub(4),
            input_buffer: String::new(),
            cursor_position: 0,
            status: StatusState::Idle,
            token_usage: None,
            should_quit: false,
            theme: Theme::for_capability(capability),
            auto_snapshot: true,
            scroll_snapshot: 0,
            total_content_height: 0,
            block_boundaries: Vec::new(),
            message_boundaries: Vec::new(),
            user_message_boundaries: Vec::new(),
            tab_render_states: HashMap::new(),
            pending_anchor: None,
            focused_item_id: None,
            selected_item_id: None,
            retry_state: None,
            feedback_blocks: BTreeMap::new(),
            team_send_blocks: BTreeMap::new(),
            team_send_next_id: 0,
            thread_read_generation: 0,
            decisions: Default::default(),
            last_printable_at: None,
            pending_z: false,
            pending_bracket: None,
            pending_g: false,
            pending_anchor_drop: None,
            has_project_context: false,
            input_history: InputHistory::new(),
            multiline_mode: false,
            context_injection_on: true,
            last_context_bundle: None,
            active_context_window: 0,
            last_assembler_diagnostics: None,
            reverse_search: ReverseSearchState::new(),
            search_state: SearchState::new(),
            cross_search: CrossSearchState::new(),
            input_scroll_offset: 0,
            autocomplete: AutocompleteState::new(),
            resolved_mentions: Vec::new(),
            pending_images: Vec::new(),
            image_indicator: None,
            command_palette: CommandPaletteState::new(),
            pending_leader: None,
            leader_feedback: None,
            modal_stack: crate::adapters::tui::presentation::ModalStack::default(),
            tool_inspectors: Default::default(),
            block_viewers: Default::default(),
            quit_confirm: None,
            notifications: {
                let mut store = crate::adapters::tui::notifications::NotificationStore::default();
                store.set_capacity(crate::adapters::tui::widgets::toast_layer::capacity_for(
                    height.saturating_sub(5),
                ));
                store
            },
            notification_log: None,
            help_overlay: HelpOverlayState::new(),
            model_selector: ModelSelectorState::new(),
            login: LoginOverlayState::default(),
            profile_switcher: ProfileSwitcherState::new(),
            active_profile: None,
            usage_panel: UsagePanelState::new(),
            log_awareness: LogAwarenessView::default(),
            pending_log_visit: None,
            local_read_only: false,
            room_panel: RoomPanelState::default(),
            mission_control: crate::adapters::tui::mission_control::MissionControlState::default(),
            artifacts_panel: ArtifactsPanelState::default(),
            artifact_window: None,
            agent_window: None,
            task_window: None,
            adapters_window: None,
            history_window: None,
            daily_budget: None,
            budget_acknowledged: None,
            pending_resolved_model: None,
            selected_model: None,
            multiplexer_detected: false,
            is_vscode: false,
            session_count: 0,
            hints_persistent: false,
            current_hint: None,
            code_max_rows: 30,
            reasoning_display: crate::domain::models::ReasoningDisplay::Folded,
            launchpad_env: None,
            session_overrides: std::collections::BTreeMap::new(),
            shadows_announced: std::collections::BTreeSet::new(),
            monitor: false,
            tour: None,
            tour_persist: None,
            task_panel_state: TaskPanelState::default(),
            agent_panel_state: AgentPanelState::default(),
            bookmark_list_selected: 0,
            bookmark_list_count: 0,
            bookmark_owning_tab: None,
            bookmark_undo_buffer: None,
            skill_registry: Arc::new(tokio::sync::RwLock::new(SkillRegistry::new())),
            skill_name_cache: HashSet::new(),
            active_skill_count: 0,
            agent_registry: AgentRegistry::new(),
            agent_suggestions: Vec::new(),
            active_agent_name: None,
            pending_agent_activation: None,
            team_proposal_conversation_id: None,
            #[cfg(feature = "a2a")]
            team_proposal_surface_tab_count: 0,
            #[cfg(feature = "a2a")]
            team_proposal_withdrawal_pending: false,
            pending_agent_then_submit: None,
            pending_plan_reminder_at_turn: None,
            plan_file_path: None,
            compacting: false,
            context_warn_level: ContextWarnLevel::None,
            pending_context_carryover: None,
            last_subagent_envelope: None,
            wave_state: None,
            wave_run: None,
            clipboard_copier: None,
            wave_overlay_selected: 0,
            rerunning_slot: None,
            wave_cancel: None,
            wave_diverge_open: false,
            wave_chat_keys: false,
            wave_drill_body: None,
            active_wave_id: None,
            wave_id_counter: 0,
        }
    }

    pub async fn refresh_skill_name_cache(&mut self) {
        let guard = self.skill_registry.read().await;
        self.skill_name_cache = guard.skills().iter().map(|s| s.name.clone()).collect();
    }

    pub fn replace_agent_registry(&mut self, registry: AgentRegistry) {
        self.agent_registry = registry;
    }

    pub fn refresh_agent_suggestions(&mut self) {
        use crate::domain::models::autocomplete::AutocompleteSuggestion;
        let filter = self.autocomplete.filter_text.to_lowercase();
        let agents = self.agent_registry.filter(&filter);
        let has_agents = !agents.is_empty();
        let mut suggestions: Vec<AutocompleteSuggestion> =
            vec![AutocompleteSuggestion::AgentMention {
                name: "default".to_string(),
                description: if has_agents {
                    "Clear active agent — return to project-context persona".to_string()
                } else {
                    "No custom agents discovered — type @Agents/default to clear any active agent"
                        .to_string()
                },
            }];
        for def in agents {
            suggestions.push(AutocompleteSuggestion::AgentMention {
                name: def.name.clone(),
                description: def.description.clone(),
            });
        }
        self.agent_suggestions = suggestions;
    }

    /// Get or create the render state for a specific tab.
    pub fn tab_render_state(&mut self, tab_id: TabId) -> &mut TabRenderState {
        self.tab_render_states.entry(tab_id).or_default()
    }

    /// Public getter for scroll_snapshot (pub(crate) field access for external crates like tests).
    /// Read this value for the current scroll position. Write through dispatch_view_scroll.
    pub fn scroll_offset(&self) -> usize {
        self.scroll_snapshot
    }

    /// Public getter for auto_snapshot. True when view_state.mode == Following.
    pub fn auto_scroll(&self) -> bool {
        self.auto_snapshot
    }

    /// Public setter for scroll_snapshot. For test setup only. Production code
    /// must use `dispatch_view_scroll` which syncs from view_state.
    pub fn set_scroll_offset(&mut self, offset: usize) {
        self.scroll_snapshot = offset;
    }

    /// Public setter for auto_snapshot. For test setup only.
    pub fn set_auto_scroll(&mut self, auto: bool) {
        self.auto_snapshot = auto;
    }

    /// Record an explicit user pick of a tool block (story 19.9 review patch:
    /// copy/peek decoupled from render-derived focus). An explicit pick is
    /// also the focus, so both signals move together.
    pub fn select_item_explicitly(&mut self, id: String) {
        self.selected_item_id = Some(id.clone());
        self.focused_item_id = Some(id);
    }

    /// Clear BOTH tool-block signals. Used wherever the render-derived focus
    /// was already discarded (conversation reset, tab switch) so a stale
    /// explicit selection cannot leak across a conversation or tab boundary.
    pub fn clear_item_selection(&mut self) {
        self.selected_item_id = None;
        self.focused_item_id = None;
    }

    /// Resolve what content `c` (copy) should place on the clipboard.
    /// Priority: explicitly selected tool block output > last assistant
    /// message > empty. Reads `selected_item_id`, NOT the render-derived
    /// `focused_item_id`: the latter is rewritten every frame by
    /// `find_focused_item`, whose nearest-visible fallback seats some block
    /// whenever any is on screen, which after any tool-using turn would copy
    /// the tool block's raw output where the FR116 AC9 path (last assistant
    /// message) used to be the common case. Moved here from `event_loop.rs`
    /// (story 19.9 review patch) so the priority stays unit-testable outside
    /// the line-budgeted event loop.
    // Covers: FR116 (AC6, AC8, AC9)
    pub fn resolve_copy_content(
        &self,
        conversation: &crate::domain::models::Conversation,
    ) -> String {
        self.resolve_copy_content_in(conversation, None)
    }

    /// As [`Self::resolve_copy_content`], with the still-open turn whose
    /// committed prose parts can also hold a selected block.
    pub fn resolve_copy_content_in(
        &self,
        conversation: &crate::domain::models::Conversation,
        open_turn: Option<&crate::domain::models::Turn>,
    ) -> String {
        // Story 21.9: a selected reasoning part copies its original text, or
        // nothing when it can no longer be resolved.
        if let Some(id) = self
            .selected_item_id
            .as_deref()
            .filter(|id| crate::domain::models::turn::is_reasoning_item_id(id))
        {
            return crate::adapters::tui::block_viewer::reasoning_source_of(
                &conversation.turns,
                open_turn,
                id,
            )
            .unwrap_or_default();
        }
        // Story 21.13: a selected code/table block copies its source, or nothing
        // when the block can no longer be resolved: never another answer.
        if let Some(id) = self
            .selected_item_id
            .as_deref()
            .filter(|id| crate::domain::models::turn::is_block_item_id(id))
        {
            return crate::adapters::tui::block_viewer::source_of(
                &conversation.turns,
                open_turn,
                id,
                &self.theme,
            )
            .unwrap_or_default();
        }
        // AC8: If a tool block is explicitly selected, copy its output
        if let Some(tool_id) = &self.selected_item_id {
            // Find the tool result in conversation messages
            for cm in conversation.messages.iter().rev() {
                for tc in &cm.tool_calls {
                    if tc.id == *tool_id {
                        if let Some(result) = &tc.result {
                            return result.content.clone();
                        }
                    }
                }
            }
        }

        // AC9: Copy the last assistant message
        for cm in conversation.messages.iter().rev() {
            if cm.role == crate::domain::models::MessageRole::Assistant && !cm.content.is_empty() {
                return cm.content.clone();
            }
        }

        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story21_6_second_user_skill_trust_is_dropped_while_one_is_queued() {
        use crate::adapters::tui::decision_queue::raise_user_skill_trust;
        let mut state = TuiState::new(80, 24);
        let path = |n: &str| {
            std::path::PathBuf::from("/test/skills")
                .join(n)
                .join("SKILL.md")
        };
        assert!(
            raise_user_skill_trust(
                &mut state,
                "c1",
                "first".into(),
                path("first"),
                String::new()
            )
            .is_some()
        );
        assert!(
            raise_user_skill_trust(
                &mut state,
                "c1",
                "second".into(),
                path("second"),
                String::new()
            )
            .is_none()
        );
        assert_eq!(state.decisions.len(), 1);
    }

    #[test]
    fn story21_6_model_skill_trust_prompts_queue_fifo_beside_a_user_prompt() {
        use crate::adapters::tui::decision_queue::{
            raise_model_skill_trust, raise_user_skill_trust,
        };
        let mut state = TuiState::new(80, 24);
        let path = |n: &str| {
            std::path::PathBuf::from("/test/skills")
                .join(n)
                .join("SKILL.md")
        };
        let _ =
            raise_user_skill_trust(&mut state, "c1", "user".into(), path("user"), String::new());
        for name in ["m1", "m2"] {
            let (tx, _rx) = tokio::sync::oneshot::channel();
            let _ = raise_model_skill_trust(&mut state, name.into(), path(name), tx);
        }
        assert_eq!(state.decisions.len(), 3);
    }

    // ── HeightCache tests (Story 16-5) ──

    #[test]
    fn height_cache_get_set_roundtrip() {
        let mut cache = HeightCache::default();
        let key = HeightKey {
            turn_id: crate::domain::models::TurnId("t1".into()),
            expansion: true,
            summary_tier: SummaryTier::Tier1,
            terminal_width: 80,
        };
        let layout = CachedTurnLayout {
            height: 42,
            block_offsets: vec![0, 10, 20],
            tool_block_offsets: vec![],
            md_block_offsets: vec![],
            reasoning_offsets: vec![],
        };
        cache.set(key.clone(), layout.clone());
        assert_eq!(cache.get(&key).unwrap().height, 42);
        assert_eq!(cache.get(&key).unwrap().block_offsets, vec![0, 10, 20]);
    }

    #[test]
    fn height_cache_miss_returns_none() {
        let cache = HeightCache::default();
        let key = HeightKey {
            turn_id: crate::domain::models::TurnId("t1".into()),
            expansion: true,
            summary_tier: SummaryTier::Tier1,
            terminal_width: 80,
        };
        assert!(cache.get(&key).is_none());
    }

    #[test]
    fn height_cache_invalidate_all_clears_entries() {
        let mut cache = HeightCache::default();
        let key = HeightKey {
            turn_id: crate::domain::models::TurnId("t1".into()),
            expansion: true,
            summary_tier: SummaryTier::Tier1,
            terminal_width: 80,
        };
        cache.set(
            key.clone(),
            CachedTurnLayout {
                height: 5,
                block_offsets: vec![],
                tool_block_offsets: vec![],
                md_block_offsets: vec![],
                reasoning_offsets: vec![],
            },
        );
        cache.invalidate_all();
        assert!(cache.get(&key).is_none());
        assert_eq!(cache.last_seen_turn_count, 0);
    }

    #[test]
    fn height_cache_invalidate_turn_removes_only_target_turn() {
        let mut cache = HeightCache::default();
        let t1 = crate::domain::models::TurnId("t1".into());
        let t2 = crate::domain::models::TurnId("t2".into());
        cache.set(
            HeightKey {
                turn_id: t1.clone(),
                expansion: true,
                summary_tier: SummaryTier::Tier1,
                terminal_width: 80,
            },
            CachedTurnLayout {
                height: 1,
                block_offsets: vec![],
                tool_block_offsets: vec![],
                md_block_offsets: vec![],
                reasoning_offsets: vec![],
            },
        );
        cache.set(
            HeightKey {
                turn_id: t2.clone(),
                expansion: true,
                summary_tier: SummaryTier::Tier1,
                terminal_width: 80,
            },
            CachedTurnLayout {
                height: 2,
                block_offsets: vec![],
                tool_block_offsets: vec![],
                md_block_offsets: vec![],
                reasoning_offsets: vec![],
            },
        );
        cache.invalidate_turn(&t1);
        assert!(
            cache
                .get(&HeightKey {
                    turn_id: t1.clone(),
                    expansion: true,
                    summary_tier: SummaryTier::Tier1,
                    terminal_width: 80,
                })
                .is_none()
        );
        assert_eq!(
            cache
                .get(&HeightKey {
                    turn_id: t2.clone(),
                    expansion: true,
                    summary_tier: SummaryTier::Tier1,
                    terminal_width: 80,
                })
                .unwrap()
                .height,
            2
        );
    }

    #[test]
    fn height_cache_evict_turns_not_in_removes_stale() {
        let mut cache = HeightCache::default();
        let t1 = crate::domain::models::TurnId("t1".into());
        let t2 = crate::domain::models::TurnId("t2".into());
        let t3 = crate::domain::models::TurnId("t3".into());
        cache.set(
            HeightKey {
                turn_id: t1.clone(),
                expansion: true,
                summary_tier: SummaryTier::Tier1,
                terminal_width: 80,
            },
            CachedTurnLayout {
                height: 1,
                block_offsets: vec![],
                tool_block_offsets: vec![],
                md_block_offsets: vec![],
                reasoning_offsets: vec![],
            },
        );
        cache.set(
            HeightKey {
                turn_id: t2.clone(),
                expansion: true,
                summary_tier: SummaryTier::Tier1,
                terminal_width: 80,
            },
            CachedTurnLayout {
                height: 2,
                block_offsets: vec![],
                tool_block_offsets: vec![],
                md_block_offsets: vec![],
                reasoning_offsets: vec![],
            },
        );
        cache.set(
            HeightKey {
                turn_id: t3.clone(),
                expansion: true,
                summary_tier: SummaryTier::Tier1,
                terminal_width: 80,
            },
            CachedTurnLayout {
                height: 3,
                block_offsets: vec![],
                tool_block_offsets: vec![],
                md_block_offsets: vec![],
                reasoning_offsets: vec![],
            },
        );
        cache.evict_turns_not_in([&t1, &t3].into_iter());
        assert!(
            cache
                .get(&HeightKey {
                    turn_id: t1.clone(),
                    expansion: true,
                    summary_tier: SummaryTier::Tier1,
                    terminal_width: 80,
                })
                .is_some()
        );
        assert!(
            cache
                .get(&HeightKey {
                    turn_id: t2.clone(),
                    expansion: true,
                    summary_tier: SummaryTier::Tier1,
                    terminal_width: 80,
                })
                .is_none()
        );
        assert!(
            cache
                .get(&HeightKey {
                    turn_id: t3.clone(),
                    expansion: true,
                    summary_tier: SummaryTier::Tier1,
                    terminal_width: 80,
                })
                .is_some()
        );
    }

    #[test]
    fn height_cache_message_get_set_roundtrip() {
        let mut cache = HeightCache::default();
        let key = MessageHeightKey {
            msg_id: "m1".into(),
            terminal_width: 80,
            content_hash: 12345,
        };
        cache.set_message(key.clone(), 17);
        assert_eq!(cache.get_message(&key), Some(17));
    }

    #[test]
    fn height_cache_message_miss_returns_none() {
        let cache = HeightCache::default();
        let key = MessageHeightKey {
            msg_id: "m1".into(),
            terminal_width: 80,
            content_hash: 12345,
        };
        assert_eq!(cache.get_message(&key), None);
    }

    #[test]
    fn height_cache_invalidate_all_clears_message_entries() {
        let mut cache = HeightCache::default();
        let key = MessageHeightKey {
            msg_id: "m1".into(),
            terminal_width: 80,
            content_hash: 12345,
        };
        cache.set_message(key.clone(), 17);
        cache.invalidate_all();
        assert_eq!(cache.get_message(&key), None);
    }

    #[test]
    fn test_model_selector_provider_nav_wraps() {
        use crate::domain::models::provider::{ModelCapability, ModelDescriptor};
        let mut ms = ModelSelectorState::new();
        ms.columns = vec![
            ProviderColumn {
                provider_id: "a".to_string(),
                display_name: "A".to_string(),
                healthy: true,
                locked_hint: None,
                sign_in_hint: None,
                discovery: None,
                models: vec![ModelDescriptor {
                    model_id: "m1".to_string(),
                    display_name: "M1".to_string(),
                    provider_id: "a".to_string(),
                    context_window: 200_000,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: false,
                    wire_model_id: None,
                }],
            },
            ProviderColumn {
                provider_id: "b".to_string(),
                display_name: "B".to_string(),
                healthy: true,
                locked_hint: None,
                sign_in_hint: None,
                discovery: None,
                models: vec![ModelDescriptor {
                    model_id: "m2".to_string(),
                    display_name: "M2".to_string(),
                    provider_id: "b".to_string(),
                    context_window: 128_000,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: false,
                    wire_model_id: None,
                }],
            },
        ];
        ms.selected_provider = 0;
        ms.navigate_provider(Direction::Left);
        assert_eq!(ms.selected_provider, 1);
        ms.navigate_provider(Direction::Right);
        assert_eq!(ms.selected_provider, 0);
    }

    #[test]
    fn test_model_selector_model_nav_wraps() {
        use crate::domain::models::provider::ModelDescriptor;
        let mut ms = ModelSelectorState::new();
        ms.columns = vec![ProviderColumn {
            provider_id: "a".to_string(),
            display_name: "A".to_string(),
            healthy: true,
            locked_hint: None,
            sign_in_hint: None,
            discovery: None,
            models: vec![
                ModelDescriptor {
                    model_id: "m1".to_string(),
                    display_name: "M1".to_string(),
                    provider_id: "a".to_string(),
                    context_window: 200_000,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: false,
                    wire_model_id: None,
                },
                ModelDescriptor {
                    model_id: "m2".to_string(),
                    display_name: "M2".to_string(),
                    provider_id: "a".to_string(),
                    context_window: 128_000,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: false,
                    wire_model_id: None,
                },
            ],
        }];
        ms.selected_provider = 0;
        ms.selected_model = 0;
        ms.navigate_model(Direction::Up);
        assert_eq!(ms.selected_model, 1);
        ms.navigate_model(Direction::Down);
        assert_eq!(ms.selected_model, 0);
    }

    #[test]
    fn test_model_selector_single_provider_noop() {
        let mut ms = ModelSelectorState::new();
        ms.columns = vec![ProviderColumn {
            provider_id: "a".to_string(),
            display_name: "A".to_string(),
            healthy: true,
            locked_hint: None,
            sign_in_hint: None,
            discovery: None,
            models: vec![],
        }];
        ms.selected_provider = 0;
        ms.navigate_provider(Direction::Left);
        assert_eq!(ms.selected_provider, 0);
        ms.navigate_provider(Direction::Right);
        assert_eq!(ms.selected_provider, 0);
    }

    #[test]
    fn test_model_selector_selected_returns_pair() {
        use crate::domain::models::provider::ModelDescriptor;
        let mut ms = ModelSelectorState::new();
        ms.columns = vec![ProviderColumn {
            provider_id: "a".to_string(),
            display_name: "A".to_string(),
            healthy: true,
            locked_hint: None,
            sign_in_hint: None,
            discovery: None,
            models: vec![ModelDescriptor {
                model_id: "m1".to_string(),
                display_name: "M1".to_string(),
                provider_id: "a".to_string(),
                context_window: 200_000,
                capabilities: Default::default(),
                pricing_tier: None,
                stale: false,
                wire_model_id: None,
            }],
        }];
        ms.selected_provider = 0;
        ms.selected_model = 0;
        let (pid, model) = ms.selected().unwrap();
        assert_eq!(pid, "a");
        assert_eq!(model.model_id, "m1");
    }

    #[test]
    fn test_model_selector_open_seeds_selection() {
        use crate::adapters::tui::presentation::{Surface, is_open, reducer};
        use crate::domain::models::provider::ModelDescriptor;
        let mut state = TuiState::new(80, 24);
        reducer::open(&mut state, Surface::ModelSelector);
        assert!(reducer::initialize_surface(
            &mut state,
            Surface::ModelSelector
        ));
        let ms = &mut state.model_selector;
        ms.columns = vec![ProviderColumn {
            provider_id: "anthropic".to_string(),
            display_name: "Anthropic".to_string(),
            healthy: true,
            locked_hint: None,
            sign_in_hint: None,
            discovery: None,
            models: vec![
                ModelDescriptor {
                    model_id: "claude-opus".to_string(),
                    display_name: "Opus".to_string(),
                    provider_id: "anthropic".to_string(),
                    context_window: 200_000,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: false,
                    wire_model_id: None,
                },
                ModelDescriptor {
                    model_id: "claude-sonnet".to_string(),
                    display_name: "Sonnet".to_string(),
                    provider_id: "anthropic".to_string(),
                    context_window: 200_000,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: false,
                    wire_model_id: None,
                },
            ],
        }];
        ms.initialize(ms.columns.clone(), "anthropic", "claude-sonnet");
        assert_eq!(ms.columns.len(), 1);
        assert_eq!(ms.selected_provider, 0);
        assert_eq!(ms.selected_model, 1);
        assert!(is_open(&state, Surface::ModelSelector));
        assert_eq!(state.focus, FocusState::Input);
    }

    #[test]
    fn test_selected_model_initializes_none() {
        let state = TuiState::new(80, 24);
        assert!(state.selected_model.is_none());
    }

    // Story 7.6 AC9 — model selector search/filter tests
    use crate::domain::models::provider::ModelDescriptor;

    fn make_test_selector() -> ModelSelectorState {
        let mut ms = ModelSelectorState::new();
        ms.columns = vec![ProviderColumn {
            provider_id: "openrouter".to_string(),
            display_name: "OpenRouter".to_string(),
            healthy: true,
            locked_hint: None,
            sign_in_hint: None,
            discovery: None,
            models: vec![
                ModelDescriptor {
                    model_id: "anthropic/claude-3.5-sonnet".to_string(),
                    display_name: "Claude 3.5 Sonnet".to_string(),
                    provider_id: "openrouter".to_string(),
                    context_window: 200_000,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: false,
                    wire_model_id: None,
                },
                ModelDescriptor {
                    model_id: "openai/gpt-4o".to_string(),
                    display_name: "GPT-4o".to_string(),
                    provider_id: "openrouter".to_string(),
                    context_window: 128_000,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: false,
                    wire_model_id: None,
                },
                ModelDescriptor {
                    model_id: "google/gemini-flash".to_string(),
                    display_name: "Gemini Flash ★".to_string(),
                    provider_id: "openrouter".to_string(),
                    context_window: 1_000_000,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: false,
                    wire_model_id: None,
                },
            ],
        }];
        ms
    }

    #[test]
    fn catalog_refresh_preserves_provider_and_model_ids_after_reordering() {
        let mut tui = TuiState::new(80, 24);
        crate::adapters::tui::presentation::reducer::open(
            &mut tui,
            crate::adapters::tui::presentation::Surface::ModelSelector,
        );
        tui.model_selector = make_test_selector();
        let state = &mut tui.model_selector;
        state.selected_model = 1;
        let mut updated = state.columns[0].clone();
        updated.models.swap(0, 1);
        updated.discovery = Some(crate::domain::models::provider::DiscoveryStatus::Cached);
        let mut inserted = updated.clone();
        inserted.provider_id = "inserted".into();
        state.refresh_columns(vec![inserted, updated]);
        assert_eq!(
            tui.modal_stack.top().unwrap().surface,
            crate::adapters::tui::presentation::Surface::ModelSelector
        );
        assert_eq!(state.selected_provider, 1);
        assert_eq!(state.selected_model, 0);
        assert_eq!(state.selected().unwrap().1.model_id, "openai/gpt-4o");
        assert_eq!(tui.focus, FocusState::Input);
        assert_eq!(
            state.columns[1].discovery,
            Some(crate::domain::models::provider::DiscoveryStatus::Cached)
        );

        let mut empty = state.columns[1].clone();
        empty.models.clear();
        state.refresh_columns(vec![empty]);
        assert_eq!(state.selected_provider, 0);
        assert_eq!(state.selected_model, 0);
        assert!(state.selected().is_none());
        state.refresh_columns(Vec::new());
        assert_eq!(
            tui.modal_stack.top().unwrap().surface,
            crate::adapters::tui::presentation::Surface::ModelSelector
        );
        assert!(state.columns.is_empty());
        assert!(state.selected().is_none());
    }

    #[test]
    fn catalog_refresh_recomputes_search_and_retains_matching_model_id() {
        let mut tui = TuiState::new(80, 24);
        crate::adapters::tui::presentation::reducer::open(
            &mut tui,
            crate::adapters::tui::presentation::Surface::ModelSelector,
        );
        tui.model_selector = make_test_selector();
        let state = &mut tui.model_selector;
        state.search_active = true;
        state.search_query = "gpt".into();
        state.recompute_filter();
        let mut updated = state.columns[0].clone();
        updated.models.swap(0, 1);
        state.refresh_columns(vec![updated]);
        assert_eq!(state.search_query, "gpt");
        assert!(state.search_active);
        assert_eq!(state.filtered_indices, [0]);
        assert_eq!(state.selected().unwrap().1.model_id, "openai/gpt-4o");
        assert_eq!(
            tui.modal_stack.top().unwrap().surface,
            crate::adapters::tui::presentation::Surface::ModelSelector
        );
    }

    #[test]
    fn recompute_filter_finds_substring_in_model_id() {
        let mut ms = make_test_selector();
        ms.search_active = true;
        ms.search_query = "gpt".to_string();
        ms.recompute_filter();
        assert_eq!(ms.filtered_indices, vec![1]);
    }

    #[test]
    fn recompute_filter_case_insensitive() {
        let mut ms = make_test_selector();
        ms.search_active = true;
        ms.search_query = "CLAUde".to_string();
        ms.recompute_filter();
        assert_eq!(ms.filtered_indices, vec![0]);
    }

    #[test]
    fn recompute_filter_matches_display_name() {
        let mut ms = make_test_selector();
        ms.search_active = true;
        ms.search_query = "flash".to_string();
        ms.recompute_filter();
        assert_eq!(ms.filtered_indices, vec![2]);
    }

    #[test]
    fn recompute_filter_unicode_in_display_name() {
        let mut ms = make_test_selector();
        ms.search_active = true;
        ms.search_query = "★".to_string();
        ms.recompute_filter();
        assert_eq!(ms.filtered_indices, vec![2]);
    }

    #[test]
    fn recompute_filter_empty_query_returns_empty_indices() {
        let mut ms = make_test_selector();
        ms.search_active = true;
        ms.search_query = String::new();
        ms.recompute_filter();
        assert!(ms.filtered_indices.is_empty());
    }

    #[test]
    fn recompute_filter_no_match_returns_empty() {
        let mut ms = make_test_selector();
        ms.search_active = true;
        ms.search_query = "zzzzz".to_string();
        ms.recompute_filter();
        assert!(ms.filtered_indices.is_empty());
    }

    #[test]
    fn column_switch_clears_search() {
        let mut ms = make_test_selector();
        ms.columns.push(ProviderColumn {
            provider_id: "anthropic".to_string(),
            display_name: "Anthropic".to_string(),
            healthy: true,
            locked_hint: None,
            sign_in_hint: None,
            discovery: None,
            models: vec![ModelDescriptor {
                model_id: "claude-sonnet".to_string(),
                display_name: "Claude Sonnet".to_string(),
                provider_id: "anthropic".to_string(),
                context_window: 200_000,
                capabilities: Default::default(),
                pricing_tier: None,
                stale: false,
                wire_model_id: None,
            }],
        });
        ms.search_active = true;
        ms.search_query = "gpt".to_string();
        ms.filtered_indices = vec![1];
        ms.navigate_provider(Direction::Right);
        assert!(!ms.search_active);
        assert!(ms.search_query.is_empty());
        assert!(ms.filtered_indices.is_empty());
    }

    #[test]
    fn recompute_filter_clamps_selection_on_shrink() {
        let mut ms = make_test_selector();
        ms.search_active = true;
        ms.search_query = "a".to_string(); // matches all 3
        ms.recompute_filter();
        ms.selected_model = 2; // last index
        ms.search_query = "gpt".to_string(); // now only matches 1
        ms.recompute_filter();
        // After recompute, selected_model should be clamped to valid range
        assert_eq!(ms.selected_model, 0);
    }

    #[test]
    fn agent_panel_state_default_empty() {
        let state = AgentPanelState::default();
        assert!(state.cached_entries.is_empty());
        assert!(state.spool_tail_cache.is_empty());
    }

    #[test]
    fn which_key_chord_s_maps_to_panel_agents() {
        use crate::adapters::tui::keymap::{
            AvailCtx, CommandId, Host, Key, LeaderStep, leader_step, table::LeaderCmd,
        };
        let avail = AvailCtx::local(130);
        match leader_step(Host::Local, Key::Char('s'), &avail, "Input") {
            LeaderStep::Run(row) => {
                assert_eq!(row.command, CommandId::Leader(LeaderCmd::Agents));
            }
            other => panic!("'s' must run the Agents row, got {other:?}"),
        }
    }

    // ─── PATCH-2 (14-3b review): wave-lifecycle handler keystones ──────────
    // Each test drives ONE extracted handler directly and asserts its DISTINCT
    // delta. A mutant that drops a handler's body → its delta vanishes → its
    // test goes red (non-vacuous — the post-`started` defaults are
    // completed_count=0 / honest_empty=None / cancelled=false, so the
    // non-default values prove the handler fired; the `_ => needs_redraw`
    // catch-all cannot produce them).

    #[test]
    fn patch2_fork_join_started_opens_wave_view() {
        let coord = crate::domain::models::AgentId::root();
        let wave = WaveViewState::started(coord.clone(), 3);
        assert_eq!(wave.coordinator, coord);
        assert_eq!(wave.spoke_count, 3);
        assert_eq!(wave.completed_count, 0);
        assert_eq!(wave.honest_empty, None);
        assert!(!wave.cancelled);
    }

    #[test]
    fn patch2_spoke_completed_bumps_completed_count() {
        let mut wave = WaveViewState::started(crate::domain::models::AgentId::root(), 2);
        assert_eq!(wave.completed_count, 0);
        wave.record_spoke_completed();
        assert_eq!(wave.completed_count, 1);
        wave.record_spoke_completed();
        assert_eq!(wave.completed_count, 2);
    }

    #[test]
    fn patch2_synthesis_ready_records_honest_empty() {
        let mut wave = WaveViewState::started(crate::domain::models::AgentId::root(), 1);
        assert_eq!(wave.honest_empty, None);
        wave.record_synthesis_ready(true);
        assert_eq!(wave.honest_empty, Some(true));
        wave.record_synthesis_ready(false);
        assert_eq!(wave.honest_empty, Some(false));
    }

    #[test]
    fn patch2_wave_cancelled_marks_cancelled() {
        let mut wave = WaveViewState::started(crate::domain::models::AgentId::root(), 1);
        assert!(!wave.cancelled);
        wave.record_wave_cancelled();
        assert!(wave.cancelled);
    }

    /// AC1 turn-loop smoke: apply the 4 handlers in a full-wave sequence
    /// (Started → 2× SpokeCompleted → SynthesisReady → WaveCancelled) and
    /// assert each handler's DISTINCT delta survives into the final state.
    /// The non-default final values (completed_count=2, honest_empty=Some,
    /// cancelled=true) are ONLY producible by their own handler — the
    /// `_ => needs_redraw` catch-all leaves them at the `started` defaults.
    #[test]
    fn patch2_turn_loop_smoke_four_handlers_distinct_deltas() {
        let coord = crate::domain::models::AgentId::root();
        let mut wave = WaveViewState::started(coord.clone(), 2);
        assert_eq!(wave.spoke_count, 2);
        assert_eq!(wave.completed_count, 0);

        // Two spokes complete.
        wave.record_spoke_completed();
        wave.record_spoke_completed();
        assert_eq!(wave.completed_count, 2);

        // Synthesis lands (non-empty floor).
        wave.record_synthesis_ready(false);
        assert_eq!(wave.honest_empty, Some(false));

        // Wave cancelled mid/after.
        wave.record_wave_cancelled();
        assert!(wave.cancelled);

        // DISTINCTNESS: the final state carries every handler's signature.
        assert_eq!(wave.coordinator, coord);
        assert_eq!(wave.spoke_count, 2);
        assert_eq!(wave.completed_count, 2);
        assert_eq!(wave.honest_empty, Some(false));
        assert!(wave.cancelled);
    }

    // ─── D-B (AI-12.3) poisoned mutants — wave-lifecycle registry ─────────

    /// `is_wave_in_flight()` is DERIVED from `active_wave_id` (no shadow bool).
    /// `begin_wave_fanout` arms the marker; the accessor flips true. Kills a
    /// mutant that leaves `active_wave_id` unset (accessor always false).
    #[test]
    fn db_begin_fanout_arms_in_flight_marker() {
        let mut s = TuiState::new(80, 24);
        assert!(!s.is_wave_in_flight(), "no wave in flight at start");
        let id = s.begin_wave_fanout();
        assert!(id > 0, "wave id is monotonic from a nonzero seed");
        assert!(
            s.is_wave_in_flight(),
            "marker armed after begin_wave_fanout"
        );
    }

    /// `retire_wave_fanout(Some(id))` with the MATCHING id clears the marker
    /// (the normal WaveTerminated/WaveRunReady path). Kills a mutant where the
    /// retire is a no-op (marker stuck → next Ctrl-C swallowed).
    #[test]
    fn db_retire_matching_id_clears_marker() {
        let mut s = TuiState::new(80, 24);
        let id = s.begin_wave_fanout();
        assert!(s.is_wave_in_flight());
        s.retire_wave_fanout(Some(id));
        assert!(
            !s.is_wave_in_flight(),
            "matching-id retire MUST clear the marker (else a stale Ctrl-C swallow)"
        );
    }

    /// POISONED — the identity-coupling core: a STALE terminal (older wave's id)
    /// MUST NOT retire a NEWER live marker. Kills the "always clear" mutant
    /// (`active_wave_id = None` unconditionally) — under that mutant, a
    /// superseded wave's late terminal would retire the live wave's marker.
    #[test]
    fn db_stale_id_does_not_clobber_live_marker() {
        let mut s = TuiState::new(80, 24);
        let old_id = s.begin_wave_fanout(); // wave A
        let new_id = s.begin_wave_fanout(); // wave B (supersedes A)
        assert_ne!(old_id, new_id, "ids are monotonic + distinct");
        assert!(s.is_wave_in_flight());
        // A's STALE late terminal arrives after B is live.
        s.retire_wave_fanout(Some(old_id));
        assert!(
            s.is_wave_in_flight(),
            "a stale (older-wave) terminal MUST NOT retire the live marker"
        );
        assert_eq!(
            s.active_wave_id,
            Some(new_id),
            "the live marker is still wave B"
        );
        // B's OWN terminal clears it.
        s.retire_wave_fanout(Some(new_id));
        assert!(!s.is_wave_in_flight());
    }

    // ─── F2 (AI-12.3 post-review party-mode): terminated-wave view retire ────

    /// A TERMINATED wave must retire the full VIEW, not just the lifecycle
    /// marker — otherwise `wave_state` lingers after failure and the two guards
    /// diverge: the `/fanout` launch-guard (`wave_state.is_some()`) blocks the
    /// next fan-out while `is_wave_in_flight()` is already false, and Ctrl-C no
    /// longer cancels (it falls through to `should_quit` → silent app quit).
    /// RED on the pre-fix `handle_wave_terminated` (which cleared only the
    /// marker): `wave_state` stays `Some`, so the launch-guard still blocks.
    #[test]
    fn f2_terminated_wave_retires_full_view() {
        let mut s = TuiState::new(80, 24);
        let id = s.begin_wave_fanout();
        // ForkJoinStarted arms the view (the launch-guard reads this field).
        s.wave_state = Some(WaveViewState::started(
            crate::domain::models::AgentId::root(),
            3,
        ));
        s.wave_cancel = Some(tokio_util::sync::CancellationToken::new());
        assert!(s.wave_state.is_some());
        assert!(s.is_wave_in_flight());

        // The wave fails → WaveTerminated.
        s.handle_wave_terminated(id);

        // Lifecycle marker retired AND the view cleared → guards agree, no wedge.
        assert!(!s.is_wave_in_flight(), "marker retired");
        assert!(
            s.wave_state.is_none(),
            "F2: a terminated wave MUST clear wave_state (else the launch-guard \
             blocks the next /fanout while Ctrl-C no longer cancels → app quit)"
        );
        assert!(
            s.wave_cancel.is_none(),
            "F2: the dead wave's cancel token must be dropped"
        );
        assert!(
            !s.request_wave_cancel(),
            "F2: after termination request_wave_cancel must be a no-op (Ctrl-C \
             must not be swallowed, and the stale view must not bait should_quit)"
        );
        // A new fan-out is unblocked (the launch-guard's precondition is gone).
        let id2 = s.begin_wave_fanout();
        assert_ne!(id2, id);
        assert!(s.is_wave_in_flight());
    }

    /// POISONED — the id-correlation guard: a STALE terminal (older wave) must
    /// NOT wipe a NEWER live wave's view. Kills a mutant that clears `wave_state`
    /// unconditionally (a superseded wave's late terminal would destroy the live
    /// wave's display).
    #[test]
    fn f2_stale_terminated_id_does_not_wipe_live_view() {
        let mut s = TuiState::new(80, 24);
        let old_id = s.begin_wave_fanout(); // wave A
        let new_id = s.begin_wave_fanout(); // wave B (supersedes A)
        s.wave_state = Some(WaveViewState::started(
            crate::domain::models::AgentId::root(),
            3,
        ));
        // A's STALE late terminal arrives while B is live.
        s.handle_wave_terminated(old_id);
        assert!(
            s.wave_state.is_some(),
            "a stale terminal MUST NOT wipe the live wave's view"
        );
        assert!(s.is_wave_in_flight(), "the live marker is still wave B");
        assert_eq!(s.active_wave_id, Some(new_id));
    }
}

#[cfg(test)]
mod wave_frame_tests {
    use super::{TuiState, WaveViewState};
    use crate::adapters::tui::{
        app::{self, InputAction},
        handlers::wave_overlay::{apply, back_action, command, context},
        keymap::{Context, table_panels::PanelCmd},
        presentation::{self, Surface, reducer},
    };
    use crate::domain::models::orchestration::{
        DrillId, ForkJoinOutcome, SpokeResult, SynthesisView,
    };
    use crate::domain::models::tool_call::{ApprovalSource, RequestId};
    use crate::domain::models::{AgentId, FocusState, ToolRisk};
    use crate::domain::ports::wave_handle::{DrillBody, WaveHandle, WaveSnapshot};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    // Retained-data fixture only: this does not launch or simulate a run.
    #[derive(Debug, Default)]
    struct Retained {
        drills: AtomicUsize,
        cancelled: AtomicBool,
    }

    impl WaveHandle for Retained {
        fn snapshot(&self) -> WaveSnapshot {
            WaveSnapshot {
                spoke_count: 2,
                completed: 2,
                honest_empty: Some(false),
                cancelled: false,
                outcome: ForkJoinOutcome {
                    spokes: vec![
                        (
                            AgentId::root(),
                            SpokeResult::Completed {
                                summary: "retained result".into()
                            }
                        );
                        2
                    ],
                    synthesis: SynthesisView::default(),
                },
                resolve_count: self.resolve_count(),
                slots: self.slots(),
                rerun_counts: vec![0, 1],
            }
        }
        fn cancel(&self) {
            self.cancelled.store(true, Ordering::SeqCst);
        }
        fn drill(&self, slot: usize) -> Option<DrillBody> {
            if slot >= 2 {
                return None;
            }
            self.drills.fetch_add(1, Ordering::SeqCst);
            Some(DrillBody(format!("body for spoke {slot}")))
        }
        fn drill_id(&self, _: usize) -> Option<DrillId> {
            None
        }
        fn slots(&self) -> Vec<AgentId> {
            vec![AgentId::root(); 2]
        }
        fn rerun_count_for_slot(&self, slot: usize) -> u8 {
            u8::from(slot == 1)
        }
        fn resolve_count(&self) -> usize {
            self.drills.load(Ordering::SeqCst)
        }
    }

    fn retained() -> (TuiState, Arc<Retained>) {
        let mut state = TuiState::new(60, 16);
        state.input_buffer = "composer payload".into();
        let handle = Arc::new(Retained::default());
        state.wave_run = Some(handle.clone());
        (state, handle)
    }

    fn dispatch(state: &mut TuiState, cmd: PanelCmd) {
        let action = command(state, cmd);
        apply(state, "stage-a", &action);
    }

    #[test]
    fn story21_4_wave_frame_back_matrix_preserves_legacy_drill_and_divergence_returns() {
        for drilled in [false, true] {
            for from_list in [false, true] {
                let (mut state, handle) = retained();
                if from_list || drilled {
                    dispatch(&mut state, PanelCmd::WaveOpenOverlay);
                }
                if drilled {
                    state.wave_overlay_selected = 1;
                    apply(&mut state, "stage-a", &InputAction::WaveDrillSpoke(1));
                    assert_eq!(context(&state), Context::WaveOverlay);
                    assert_eq!(back_action(&state, false), InputAction::CloseWaveOverlay);
                    assert_eq!(handle.resolve_count(), 1);
                }
                dispatch(&mut state, PanelCmd::WaveOpenDiverge);
                let id = state.modal_stack.top().unwrap().id;
                assert_eq!(context(&state), Context::ChatWave);
                for ctrl_c in [false, true] {
                    assert_eq!(back_action(&state, ctrl_c), InputAction::CloseDivergeView);
                }
                apply(&mut state, "stage-a", &InputAction::CloseDivergeView);
                assert_eq!(presentation::is_open(&state, Surface::WaveOverlay), drilled);
                assert!(state.wave_run.is_some());
                assert_eq!(state.focus, FocusState::Input);
                if drilled {
                    assert_eq!(state.modal_stack.top().unwrap().id, id);
                    assert_eq!(
                        state.wave_drill_body.as_ref().unwrap().1,
                        "body for spoke 1"
                    );
                    assert_eq!(context(&state), Context::ChatWave);
                    assert_eq!(back_action(&state, false), InputAction::DismissWave);
                    assert_eq!(back_action(&state, true), InputAction::CloseWaveOverlay);
                    dispatch(&mut state, PanelCmd::WaveOpenOverlay);
                    assert_eq!(context(&state), Context::WaveOverlay);
                    assert!(state.wave_drill_body.is_some());
                    assert_eq!(
                        state.wave_overlay_selected, 0,
                        "legacy OpenWaveOverlay resets selection without discarding the drill body"
                    );
                }
            }
        }
    }

    #[test]
    fn story21_4_wave_frame_close_retains_globals_and_late_completion_cannot_reopen() {
        let (mut state, handle) = retained();
        state.wave_cancel = Some(tokio_util::sync::CancellationToken::new());
        let child = state.wave_cancel.as_ref().unwrap().child_token();
        state.wave_state = Some(WaveViewState::started(AgentId::root(), 2));
        state.active_wave_id = Some(7);
        state.rerunning_slot = Some(1);
        dispatch(&mut state, PanelCmd::WaveOpenFromChat);
        apply(&mut state, "stage-a", &InputAction::WaveDrillSpoke(1));
        let action = back_action(&state, true);
        apply(&mut state, "stage-a", &action);
        assert!(state.modal_stack.is_empty());
        assert!(state.wave_run.is_some() && state.wave_state.is_some());
        assert_eq!(state.active_wave_id, Some(7));
        assert_eq!(state.rerunning_slot, Some(1));
        assert!(!child.is_cancelled());
        assert!(!handle.cancelled.load(Ordering::SeqCst));
        assert!(state.wave_drill_body.is_none());
        assert!(!state.wave_chat_keys && !state.wave_diverge_open);
        state.handle_wave_run_ready(handle.clone());
        state.handle_spoke_rerun_started(1);
        state.handle_spoke_rerun_reverted(1);
        assert!(
            state.modal_stack.is_empty(),
            "completion has no open authority"
        );
        dispatch(&mut state, PanelCmd::WaveOpenFromChat);
        assert_eq!(
            state.modal_stack.top().unwrap().conversation_id.as_deref(),
            Some("stage-a")
        );
        assert_eq!(handle.resolve_count(), 1, "reopen is not a drill");
        apply(&mut state, "stage-a", &InputAction::DismissWave);
        assert!(state.modal_stack.is_empty());
        assert!(
            state.wave_run.is_none() && state.wave_state.is_none() && state.wave_cancel.is_none()
        );
        assert!(state.active_wave_id.is_none() && state.rerunning_slot.is_none());
        state.handle_wave_run_ready(handle);
        assert!(
            state.modal_stack.is_empty(),
            "even late data after dismiss cannot reopen"
        );
    }

    /// A permission that arrives while the user is typing waits in the decision
    /// queue (RU-6): the closing wave frames neither lose nor open it.
    fn queue_approval(state: &mut TuiState) -> RequestId {
        use crate::adapters::tui::decision_queue::{
            self, PermissionInput, PermissionPayload, Responder,
        };
        let id = RequestId::new();
        state.last_printable_at = Some(state.notifications.now());
        decision_queue::raise_permission(
            state,
            PermissionPayload::new(
                id.clone(),
                ApprovalSource::ForegroundTurn {
                    conversation_id: "stage-a".into(),
                },
                "Bash".into(),
                ToolRisk::Elevated,
                PermissionInput::Ready(Arc::new(
                    serde_json::json!({"command": "payload-safe-approval"}),
                )),
            ),
            Responder::Local,
        );
        id
    }

    fn assert_approval_waits(state: &TuiState, id: &RequestId) {
        use crate::adapters::tui::decision_queue::DecisionSubject;
        assert!(state.decisions.items().any(
            |item| matches!(&item.subject, DecisionSubject::Permission(p) if &p.request_id == id)
        ));
        assert!(
            !presentation::is_open(state, Surface::Decision),
            "a waiting approval is never opened by a wave frame closing"
        );
        assert_eq!(state.input_buffer, "composer payload");
    }

    #[test]
    fn story21_4_wave_frame_effects_reconcile_real_queued_approval_including_wrong_owner() {
        for effect in [
            InputAction::CloseWaveOverlay,
            InputAction::CloseDivergeView,
            InputAction::DismissWave,
            InputAction::WaveDrillSpoke(1),
        ] {
            let (mut state, handle) = retained();
            dispatch(&mut state, PanelCmd::WaveOpenOverlay);
            let id = queue_approval(&mut state);
            dispatch(&mut state, PanelCmd::WaveDiverge);
            assert!(!presentation::is_open(&state, Surface::Decision));
            if matches!(effect, InputAction::WaveDrillSpoke(_)) {
                // Wrong owning Stage closes safely without drilling its data.
                apply(&mut state, "stage-b", &effect);
                assert_eq!(handle.resolve_count(), 0);
                assert!(state.wave_run.is_some());
            } else {
                apply(&mut state, "stage-a", &effect);
            }
            assert!(state.modal_stack.is_empty());
            assert_approval_waits(&state, &id);
        }
        // Divergence -> retained drill is NOT a close and cannot promote early.
        let (mut state, _) = retained();
        dispatch(&mut state, PanelCmd::WaveOpenOverlay);
        let id = queue_approval(&mut state);
        apply(&mut state, "stage-a", &InputAction::WaveDrillSpoke(1));
        dispatch(&mut state, PanelCmd::WaveDiverge);
        apply(&mut state, "stage-a", &InputAction::CloseDivergeView);
        assert!(!presentation::is_open(&state, Surface::Decision));
        let action = back_action(&state, false);
        apply(&mut state, "stage-a", &action);
        assert_approval_waits(&state, &id);
    }

    #[test]
    fn story21_4_wave_frame_effects_do_not_touch_covering_or_unowned_payloads() {
        let (mut state, handle) = retained();
        dispatch(&mut state, PanelCmd::WaveOpenOverlay);
        reducer::open(&mut state, Surface::Help);
        for action in [
            InputAction::DismissWave,
            InputAction::CloseWaveOverlay,
            InputAction::WaveDrillSpoke(1),
        ] {
            assert!(apply(&mut state, "stage-a", &action));
            assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Help);
            assert!(state.wave_run.is_some());
        }
        assert_eq!(handle.resolve_count(), 0);
        assert!(!apply(
            &mut state,
            "stage-a",
            &InputAction::WaveRerunSpoke(0)
        ));
        let id = queue_approval(&mut state);
        apply(&mut state, "stage-b", &InputAction::OpenDivergeView);
        assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Help);
        assert!(!presentation::is_open(&state, Surface::WaveOverlay));
        assert!(!presentation::is_open(&state, Surface::Decision));
        assert!(state.wave_run.is_some());
        // Removing a stale covered Stage did not consume or promote the queue.
        app::with_card_focus_reconcile(&mut state, |state| {
            reducer::close(state, Surface::Help);
        });
        assert_approval_waits(&state, &id);
    }
}
