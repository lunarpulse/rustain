//! Session notification store (Story 21.1 A1/A2/A4/A6).
//!
//! One adapter-owned store behind every transient notice in BOTH hosts (the
//! local TUI and the attached client). Producers never write presentation
//! state: they call [`notify`] / [`notify_draft`]; the store owns the deadline,
//! coalescing, displacement, suppression, retention and the unseen-warning
//! counter. Time comes only from the injected [`Clock`] (RU-30) — there is no
//! per-notice task, no tick decrement and no pause under modals or tabs: a
//! toast is visible from arrival until `deadline` and absent from the first
//! frame drawn at or after it (`sweep` runs at the top of every draw).

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::adapters::tui::state::TuiState;
use crate::domain::clock::{Clock, SystemClock};
use crate::domain::models::{NoticeLevel, NotificationTtlSecs};

/// Retention ceiling: coalesced records kept for the log (RU-32).
pub const MAX_RECORDS: usize = 1_000;
/// Retention ceiling: payload bytes (message + source label) kept for the log.
pub const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
/// One message is bounded so a single record can never exceed the payload cap.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024;
/// An identical arrival within this window after the record expired is only
/// counted and logged — it never repaints a toast.
pub const SUPPRESSION_WINDOW: Duration = Duration::from_secs(5 * 60);
/// Visible toast ceiling before geometry narrows it (A5).
pub const MAX_VISIBLE_TOASTS: usize = 3;

/// Store severity (RU-15). `Success` is adapter-only: `NoticeLevel` is a wire
/// type and gains no variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NoticeSeverity {
    Info,
    Success,
    Warning,
    Error,
}

impl NoticeSeverity {
    /// RU-15 map: `Info → Info`, `Warning | Advisory → Warning`, `Error → Error`.
    #[must_use]
    pub fn from_level(level: NoticeLevel) -> Self {
        match level {
            NoticeLevel::Info => Self::Info,
            NoticeLevel::Warning | NoticeLevel::Advisory => Self::Warning,
            NoticeLevel::Error => Self::Error,
        }
    }

    /// The severity word, always painted (monochrome-safe).
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Info => "Info",
            Self::Success => "Success",
            Self::Warning => "Warning",
            Self::Error => "Error",
        }
    }

    /// True for the severities counted by the unseen badge.
    #[must_use]
    pub fn is_attention(self) -> bool {
        matches!(self, Self::Warning | Self::Error)
    }
}

/// The typed producer family of a notice (never inferred from text).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoticeSource {
    System,
    Input,
    Image,
    Clipboard,
    Export,
    Bookmark,
    Search,
    Tabs,
    Session,
    Model,
    Profile,
    Login,
    Accounts,
    Compaction,
    Permission,
    Config,
    Task,
    Skills,
    Plan,
    Palette,
    Chord,
    Help,
    Density,
    Adapter,
    Health,
    Approval,
    Team,
    Peer,
    Attached,
    Daemon,
    Context,
}

impl NoticeSource {
    /// Short lower-case label painted in the log row.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Input => "input",
            Self::Image => "image",
            Self::Clipboard => "clipboard",
            Self::Export => "export",
            Self::Bookmark => "bookmark",
            Self::Search => "search",
            Self::Tabs => "tabs",
            Self::Session => "session",
            Self::Model => "model",
            Self::Profile => "profile",
            Self::Login => "login",
            Self::Accounts => "accounts",
            Self::Compaction => "compaction",
            Self::Permission => "permission",
            Self::Config => "config",
            Self::Task => "task",
            Self::Skills => "skills",
            Self::Plan => "plan",
            Self::Palette => "palette",
            Self::Chord => "chord",
            Self::Help => "help",
            Self::Density => "density",
            Self::Adapter => "adapter",
            Self::Health => "health",
            Self::Approval => "approval",
            Self::Team => "team",
            Self::Peer => "peer",
            Self::Attached => "attached",
            Self::Daemon => "daemon",
            Self::Context => "context",
        }
    }
}

/// The only executable handles a record may carry (A8). Eviction drops the
/// record and with it the handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeAction {
    /// A turn-failure Error: re-send the last user message. `messages` is the
    /// failed conversation's message count when the turn failed: the handle is
    /// only eligible while the conversation still ends at that failed turn
    /// (S-LOG compares it with the active count), so a later good turn is never
    /// truncated by a retry of an older failure.
    RetryTurn { messages: usize },
    /// A compaction failure: re-trigger compaction.
    RetryCompaction,
    /// A context-window warning: S-LOG `K` (Compact…) and `n` (Start fresh…)
    /// raise the matching interim decision — they never act on their own.
    ContextDecision,
}

/// What a producer asks the store to record.
#[derive(Debug, Clone)]
pub struct NoticeDraft {
    pub source: NoticeSource,
    pub severity: NoticeSeverity,
    pub message: String,
    /// Conversation the notice is attributed to (`None` = session-global).
    pub conversation_id: Option<String>,
    pub action: Option<NoticeAction>,
}

impl NoticeDraft {
    #[must_use]
    pub fn new(source: NoticeSource, severity: NoticeSeverity, message: impl Into<String>) -> Self {
        Self {
            source,
            severity,
            message: message.into(),
            conversation_id: None,
            action: None,
        }
    }

    #[must_use]
    pub fn conversation(mut self, conversation_id: impl Into<String>) -> Self {
        self.conversation_id = Some(conversation_id.into());
        self
    }

    #[must_use]
    pub fn action(mut self, action: NoticeAction) -> Self {
        self.action = Some(action);
        self
    }
}

/// Where a record is in its presentation life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presentation {
    /// Painted as a toast until its deadline.
    Visible,
    /// Its TTL ran out (the 5-minute suppression window applies).
    Expired,
    /// Displaced, cleared or dismissed: log-only.
    Archived,
}

/// One coalesced record.
#[derive(Debug, Clone)]
pub struct NoticeRecord {
    pub id: u64,
    pub conversation_id: Option<String>,
    pub source: NoticeSource,
    pub severity: NoticeSeverity,
    pub message: String,
    pub first_at: Instant,
    pub last_at: Instant,
    pub first_wall_ms: i64,
    pub last_wall_ms: i64,
    /// Occurrences coalesced into this record (`×N`).
    pub count: u32,
    pub deadline: Instant,
    pub presentation: Presentation,
    pub action: Option<NoticeAction>,
}

impl NoticeRecord {
    fn payload_bytes(&self) -> usize {
        self.message.len() + self.source.label().len()
    }

    fn same_identity(&self, draft: &NoticeDraft, message: &str) -> bool {
        self.source == draft.source
            && self.severity == draft.severity
            && self.message == message
            && self.conversation_id == draft.conversation_id
    }
}

/// What `notify` did to the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyEffect {
    /// A toast appeared, changed or was displaced: repaint.
    Repaint,
    /// Counted and logged only (suppression window / archived directly).
    Silent,
}

/// Session-local, never persisted.
pub struct NotificationStore {
    clock: Arc<dyn Clock>,
    ttls: [Duration; 4],
    records: VecDeque<NoticeRecord>,
    next_id: u64,
    payload_bytes: usize,
    unseen: u64,
    log_open: bool,
    capacity: usize,
}

impl std::fmt::Debug for NotificationStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotificationStore")
            .field("records", &self.records.len())
            .field("unseen", &self.unseen)
            .finish_non_exhaustive()
    }
}

impl Default for NotificationStore {
    fn default() -> Self {
        Self::from_config(
            &crate::domain::models::NotificationsConfig::default(),
            Arc::new(SystemClock::default()),
        )
    }
}

impl NotificationStore {
    /// The one constructor both hosts use: a store on `clock` with the
    /// effective lifetimes of the `[notifications]` config (clamped to
    /// `[1, 60]` seconds).
    #[must_use]
    pub fn from_config(
        config: &crate::domain::models::NotificationsConfig,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::new(clock, config.clamped())
    }

    /// A store on `clock` with the effective (already clamped) lifetimes.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>, ttls: NotificationTtlSecs) -> Self {
        Self {
            clock,
            ttls: [
                Duration::from_secs(ttls.info),
                Duration::from_secs(ttls.success),
                Duration::from_secs(ttls.warning),
                Duration::from_secs(ttls.error),
            ],
            records: VecDeque::new(),
            next_id: 1,
            payload_bytes: 0,
            unseen: 0,
            log_open: false,
            capacity: MAX_VISIBLE_TOASTS,
        }
    }

    fn ttl(&self, severity: NoticeSeverity) -> Duration {
        match severity {
            NoticeSeverity::Info => self.ttls[0],
            NoticeSeverity::Success => self.ttls[1],
            NoticeSeverity::Warning => self.ttls[2],
            NoticeSeverity::Error => self.ttls[3],
        }
    }

    /// The injected clock's current instant.
    #[must_use]
    pub fn now(&self) -> Instant {
        self.clock.now()
    }

    /// Archive every visible toast whose deadline has been reached. Returns
    /// true when the screen changed. Each toast expires exactly once.
    pub fn sweep(&mut self) -> bool {
        let now = self.clock.now();
        let mut changed = false;
        for record in &mut self.records {
            if record.presentation == Presentation::Visible && now >= record.deadline {
                record.presentation = Presentation::Expired;
                changed = true;
            }
        }
        changed
    }

    /// Earliest deadline among visible toasts.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.records
            .iter()
            .filter(|r| r.presentation == Presentation::Visible)
            .map(|r| r.deadline)
            .min()
    }

    /// True when a visible toast's deadline has passed (ticks use this to ask
    /// for a redraw; the draw itself sweeps).
    #[must_use]
    pub fn deadline_passed(&self) -> bool {
        self.next_deadline()
            .is_some_and(|deadline| self.clock.now() >= deadline)
    }

    /// Visible toasts, oldest first (the newest paints at the bottom).
    pub fn visible(&self) -> impl Iterator<Item = &NoticeRecord> {
        self.records
            .iter()
            .filter(|r| r.presentation == Presentation::Visible)
    }

    /// Every retained record, oldest first.
    pub fn records(&self) -> impl DoubleEndedIterator<Item = &NoticeRecord> + ExactSizeIterator {
        self.records.iter()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    #[must_use]
    pub fn payload_bytes(&self) -> usize {
        self.payload_bytes
    }

    #[must_use]
    pub fn get(&self, id: u64) -> Option<&NoticeRecord> {
        self.records.iter().find(|r| r.id == id)
    }

    /// Warning/error occurrences not yet presented in S-LOG (the `Log !N`
    /// badge). Independent of retention and of toast paint.
    #[must_use]
    pub fn unseen(&self) -> u64 {
        self.unseen
    }

    /// S-LOG opened: everything is presented, the badge clears and stays
    /// clear while the log is open.
    pub fn log_presented(&mut self) {
        self.log_open = true;
        self.unseen = 0;
    }

    /// S-LOG closed.
    pub fn log_closed(&mut self) {
        self.log_open = false;
    }

    /// Visible-toast capacity from geometry (A5); a smaller pane archives the
    /// overflow by the displacement order.
    pub fn set_capacity(&mut self, capacity: usize) -> bool {
        let capacity = capacity.max(1);
        self.capacity = capacity;
        let mut changed = false;
        while self.visible().count() > capacity {
            let Some(victim) = self.pick_victim(None) else {
                break;
            };
            self.records[victim].presentation = Presentation::Archived;
            changed = true;
        }
        changed
    }

    /// Lowest-severity-then-oldest visible toast that `incoming` may displace.
    /// `None` incoming = geometry shrink (anything may go). An Error is only
    /// displaced by a newer Error; a lower-severity arrival never displaces a
    /// higher-severity toast.
    fn pick_victim(&self, incoming: Option<NoticeSeverity>) -> Option<usize> {
        self.records
            .iter()
            .enumerate()
            .filter(|(_, r)| r.presentation == Presentation::Visible)
            .filter(|(_, r)| match incoming {
                None => true,
                Some(severity) => r.severity <= severity,
            })
            .min_by_key(|(_, r)| (r.severity, r.first_at, r.id))
            .map(|(index, _)| index)
    }

    /// Record one notice. The single entry for every producer.
    pub fn notify(&mut self, draft: NoticeDraft) -> NotifyEffect {
        self.sweep();
        let now = self.clock.now();
        let wall = self.clock.wall_now_ms();
        let message = bound_message(draft.message.as_str());
        if draft.severity.is_attention() && !self.log_open {
            self.unseen = self.unseen.saturating_add(1);
        }

        // Coalesce into the newest identical record while it is visible, or
        // count silently while it is inside the suppression window (a record
        // that expired or was archived keeps suppressing identical arrivals).
        // A newer occurrence's action handle replaces the old one: the handle
        // always belongs to the newest failure.
        if let Some(index) = self
            .records
            .iter()
            .rposition(|r| r.same_identity(&draft, &message))
        {
            let record = &mut self.records[index];
            let effect = match record.presentation {
                Presentation::Visible => Some(NotifyEffect::Repaint),
                Presentation::Expired | Presentation::Archived
                    if now <= record.deadline + SUPPRESSION_WINDOW =>
                {
                    Some(NotifyEffect::Silent)
                }
                Presentation::Expired | Presentation::Archived => None,
            };
            if let Some(effect) = effect {
                if draft.action.is_some() {
                    record.action = draft.action;
                }
                record.count = record.count.saturating_add(1);
                record.last_at = now;
                record.last_wall_ms = wall;
                return effect;
            }
        }

        let mut record = NoticeRecord {
            id: self.next_id,
            conversation_id: draft.conversation_id,
            source: draft.source,
            severity: draft.severity,
            message,
            first_at: now,
            last_at: now,
            first_wall_ms: wall,
            last_wall_ms: wall,
            count: 1,
            deadline: now + self.ttl(draft.severity),
            presentation: Presentation::Visible,
            action: draft.action,
        };
        self.next_id += 1;

        let mut effect = NotifyEffect::Repaint;
        if self.visible().count() >= self.capacity {
            match self.pick_victim(Some(record.severity)) {
                Some(victim) => self.records[victim].presentation = Presentation::Archived,
                None => {
                    record.presentation = Presentation::Archived;
                    effect = NotifyEffect::Silent;
                }
            }
        }
        self.payload_bytes += record.payload_bytes();
        self.records.push_back(record);
        // Retention drops the oldest record that is no longer on screen first;
        // a still-visible toast goes only when nothing else is left (AC1: a
        // toast lives until its deadline). The newest record is never the victim.
        while self.records.len() > 1
            && (self.records.len() > MAX_RECORDS || self.payload_bytes > MAX_PAYLOAD_BYTES)
        {
            let last = self.records.len() - 1;
            let victim = self
                .records
                .iter()
                .take(last)
                .position(|r| r.presentation != Presentation::Visible)
                .unwrap_or(0);
            if let Some(evicted) = self.records.remove(victim) {
                self.payload_bytes -= evicted.payload_bytes();
            }
        }
        effect
    }

    /// Archive every visible toast (`Ctrl+X x`); sources and log rows stay.
    pub fn clear_toasts(&mut self) -> usize {
        let mut cleared = 0;
        for record in &mut self.records {
            if record.presentation == Presentation::Visible {
                record.presentation = Presentation::Archived;
                cleared += 1;
            }
        }
        cleared
    }

    /// Hide one toast's presentation (`x` in S-LOG). Returns true when a
    /// visible toast was archived; the record itself stays in the log.
    pub fn dismiss(&mut self, id: u64) -> bool {
        match self.records.iter_mut().find(|r| r.id == id) {
            Some(record) if record.presentation == Presentation::Visible => {
                record.presentation = Presentation::Archived;
                true
            }
            _ => false,
        }
    }

    /// Consume a record's action handle exactly once.
    pub fn take_action(&mut self, id: u64) -> Option<NoticeAction> {
        self.records
            .iter_mut()
            .find(|r| r.id == id)
            .and_then(|r| r.action.take())
    }
}

/// Bound a message to `MAX_MESSAGE_BYTES` on a char boundary.
fn bound_message(message: &str) -> String {
    if message.len() <= MAX_MESSAGE_BYTES {
        return message.to_owned();
    }
    let mut end = MAX_MESSAGE_BYTES - '…'.len_utf8();
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = message[..end].to_owned();
    out.push('…');
    out
}

/// Record a notice and ask for a repaint when the screen changes. The one
/// producer entry (A1): `notify(state, source, severity, message)`.
pub fn notify(
    state: &mut TuiState,
    source: NoticeSource,
    severity: NoticeSeverity,
    message: impl Into<String>,
) {
    notify_draft(state, NoticeDraft::new(source, severity, message));
}

/// [`notify`] with attribution and an optional action handle.
pub fn notify_draft(state: &mut TuiState, draft: NoticeDraft) {
    let unseen = state.notifications.unseen();
    let effect = state.notifications.notify(draft);
    // A silent arrival still repaints when the `Log !N` badge moved.
    if effect == NotifyEffect::Repaint
        || state.notifications.unseen() != unseen
        || crate::adapters::tui::presentation::is_open(
            state,
            crate::adapters::tui::presentation::Surface::NotificationLog,
        )
    {
        state.needs_redraw = true;
    }
}

/// The one painted line of a message (toast and S-LOG row): the first
/// non-blank line with control characters flattened, `…` when more lines
/// follow. A message that opens with blank lines is never painted empty.
#[must_use]
pub fn headline(message: &str) -> String {
    let mut lines = message.lines().skip_while(|l| l.trim().is_empty());
    let head = lines.next().unwrap_or("");
    let mut out: String = head
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if lines.next().is_some() {
        out.push('…');
    }
    out
}

/// First thing every draw does, in both hosts: expire the toasts whose deadline
/// has been reached, so no frame at or after a deadline paints them (A4).
pub fn begin_draw(state: &mut TuiState) {
    state.notifications.sweep();
}

/// Both ticks call this: a passed deadline asks for the redraw that sweeps.
/// No per-notice timer, no tick decrement (A4).
pub fn tick(state: &mut TuiState) {
    if state.notifications.deadline_passed() {
        state.needs_redraw = true;
    }
}

/// Info-severity shorthand.
pub fn info(state: &mut TuiState, source: NoticeSource, message: impl Into<String>) {
    notify(state, source, NoticeSeverity::Info, message);
}

/// Success-severity shorthand (typed local producers only, A2).
pub fn success(state: &mut TuiState, source: NoticeSource, message: impl Into<String>) {
    notify(state, source, NoticeSeverity::Success, message);
}

/// Warning-severity shorthand.
pub fn warning(state: &mut TuiState, source: NoticeSource, message: impl Into<String>) {
    notify(state, source, NoticeSeverity::Warning, message);
}

/// Error-severity shorthand.
pub fn error(state: &mut TuiState, source: NoticeSource, message: impl Into<String>) {
    notify(state, source, NoticeSeverity::Error, message);
}

/// Wall-clock `HH:MM:SS` (local time) of a unix-millisecond stamp.
#[must_use]
pub fn format_hms(wall_ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(wall_ms)
        .single()
        .map_or_else(
            || "--:--:--".to_owned(),
            |t| t.format("%H:%M:%S").to_string(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::clock::MockClock;

    fn store() -> (NotificationStore, Arc<MockClock>) {
        let clock = Arc::new(MockClock::new(Instant::now()));
        let config = crate::domain::models::NotificationsConfig::default();
        (
            NotificationStore::from_config(&config, clock.clone()),
            clock,
        )
    }

    fn draft(source: NoticeSource, severity: NoticeSeverity, message: &str) -> NoticeDraft {
        NoticeDraft::new(source, severity, message)
    }

    #[test]
    fn story21_1_severity_map_is_ru15() {
        assert_eq!(
            NoticeSeverity::from_level(NoticeLevel::Info),
            NoticeSeverity::Info
        );
        assert_eq!(
            NoticeSeverity::from_level(NoticeLevel::Warning),
            NoticeSeverity::Warning
        );
        assert_eq!(
            NoticeSeverity::from_level(NoticeLevel::Advisory),
            NoticeSeverity::Warning
        );
        assert_eq!(
            NoticeSeverity::from_level(NoticeLevel::Error),
            NoticeSeverity::Error
        );
    }

    #[test]
    fn story21_1_store_overflow_message_is_bounded_on_a_char_boundary() {
        let huge = "é".repeat(MAX_MESSAGE_BYTES);
        let bounded = bound_message(&huge);
        assert!(bounded.len() <= MAX_MESSAGE_BYTES);
        assert!(bounded.ends_with('…'));
    }

    #[test]
    fn story21_1_store_payload_accounting_follows_eviction() {
        let (mut s, _) = store();
        s.set_capacity(1);
        for i in 0..(MAX_RECORDS + 5) {
            s.notify(draft(
                NoticeSource::System,
                NoticeSeverity::Info,
                &format!("m{i}"),
            ));
        }
        assert_eq!(s.len(), MAX_RECORDS);
        let expected: usize = s.records().map(NoticeRecord::payload_bytes).sum();
        assert_eq!(s.payload_bytes(), expected);
    }

    fn retry(messages: usize) -> NoticeDraft {
        draft(
            NoticeSource::System,
            NoticeSeverity::Error,
            "provider failed",
        )
        .conversation("conv-a")
        .action(NoticeAction::RetryTurn { messages })
    }

    /// S1-2: the action handle belongs to the newest failure. `take_action`
    /// consumed the first handle, so an identical failure that coalesces into
    /// the same record must hand its own handle back (visible and expired).
    #[test]
    fn story21_1_coalesced_arrival_carries_the_newest_action_handle() {
        let (mut s, clock) = store();
        s.notify(retry(2));
        let id = s.records().next().unwrap().id;
        assert_eq!(
            s.take_action(id),
            Some(NoticeAction::RetryTurn { messages: 2 })
        );
        assert_eq!(s.take_action(id), None);
        s.notify(retry(4));
        assert_eq!(s.len(), 1, "visible: coalesced into the same record");
        assert_eq!(
            s.take_action(id),
            Some(NoticeAction::RetryTurn { messages: 4 })
        );

        // Expired within the suppression window: still the same record.
        s.notify(retry(6));
        clock.advance(Duration::from_secs(61));
        s.sweep();
        s.notify(retry(8));
        assert_eq!(s.len(), 1, "expired: coalesced silently");
        assert_eq!(s.records().next().unwrap().count, 4);
        assert_eq!(
            s.take_action(id),
            Some(NoticeAction::RetryTurn { messages: 8 })
        );
        // An occurrence without a handle never erases the existing one.
        s.notify(retry(10));
        s.notify(
            draft(
                NoticeSource::System,
                NoticeSeverity::Error,
                "provider failed",
            )
            .conversation("conv-a"),
        );
        assert_eq!(
            s.take_action(id),
            Some(NoticeAction::RetryTurn { messages: 10 })
        );
    }

    /// S1-9: a flood of distinct records never evicts a visible toast before
    /// its deadline; the oldest non-visible record goes first.
    #[test]
    fn story21_1_retention_never_evicts_a_visible_toast_before_its_deadline() {
        let (mut s, _) = store();
        s.notify(retry(2));
        let error_id = s.records().next().unwrap().id;
        for i in 0..(MAX_RECORDS + 5) {
            s.notify(draft(
                NoticeSource::System,
                NoticeSeverity::Info,
                &format!("flood {i}"),
            ));
        }
        assert!(s.len() <= MAX_RECORDS);
        assert!(
            s.visible().any(|r| r.id == error_id),
            "the Error is still on screen"
        );
        let expected: usize = s.records().map(NoticeRecord::payload_bytes).sum();
        assert_eq!(s.payload_bytes(), expected);
    }

    /// S1-11: a message that opens with blank lines paints its first real line.
    #[test]
    fn story21_1_headline_is_the_first_non_blank_line() {
        assert_eq!(headline("one\ntwo"), "one…");
        assert_eq!(headline("a\tb"), "a b");
        assert_eq!(headline(""), "");
        assert_eq!(headline("\n\nreal text"), "real text");
        assert_eq!(headline("  \nreal\nmore"), "real…");
        assert_eq!(headline("\n \n"), "");
    }
}
