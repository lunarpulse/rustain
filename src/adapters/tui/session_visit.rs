//! Read-only retained session reader for one exact, host-authorized Main task result.
//! It shares text-window navigation and paint, never a live agent or its spool.

use crate::adapters::tui::tool_inspector::{ToolFrameKey, ToolTab, ToolTarget};
use crate::domain::models::conversation::HistorySource;
use crate::domain::models::{Conversation, MessageRole, PartId, TabId, TurnPart};

use crate::adapters::tui::app::InputAction;
use crate::adapters::tui::artifact_window::{
    self as text, ArtifactBody, ArtifactWindowState, Slot,
};
use crate::adapters::tui::keymap::table_mission::MissionCmd;
use crate::adapters::tui::keymap::{CommandId, Context, Host, Key};
use crate::adapters::tui::presentation::{Surface, reducer};
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::theme::Theme;
use crate::domain::models::AgentId;
use crate::domain::models::view_state::ScrollDelta;
use crate::domain::services::peer_text::sanitize_display_line;
use crate::infrastructure::subagent::session_history::{
    MainVisitKey, VisitBacking, VisitBody, VisitRead, VisitTurn,
};
use std::cell::Cell;
use std::fmt::{self, Write};
use tokio_util::sync::DropGuard;

/// The durable address to revalidate, even while another frame covers the Visit.
#[derive(Debug)]
pub(crate) struct VisitCursor {
    pub key: MainVisitKey,
    pub expires_at_ms: Option<i64>,
    /// Metadata only. A lost backing lease must clear the projected reader before use.
    pub backing: Option<VisitBacking>,
    // Never follow a rebound attempt under the same result address.
    attempt: Option<AgentId>,
}

/// Which frame's key started a pending open. The origin frame owns the pending open, so
/// dropping its payload (Back, retarget, close, quit) cancels the read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpeningOrigin {
    Inspector {
        frame_id: u64,
        tab: ToolTab,
    },
    Visit {
        frame_id: u64,
        cursor_key: MainVisitKey,
    },
}

impl OpeningOrigin {
    pub(crate) fn frame_id(&self) -> u64 {
        match self {
            Self::Inspector { frame_id, .. } | Self::Visit { frame_id, .. } => *frame_id,
        }
    }
}

/// One pending, cancelable open. It holds identities only, never a body, and cancels its
/// read when dropped.
#[derive(Debug)]
pub(crate) struct VisitOpening {
    pub(crate) generation: u64,
    pub(crate) key: MainVisitKey,
    pub(crate) origin: OpeningOrigin,
    pub(crate) tab_id: TabId,
    pub(crate) conversation_id: String,
    _cancel: DropGuard,
}

impl VisitOpening {
    pub(crate) fn new(
        generation: u64,
        key: MainVisitKey,
        origin: OpeningOrigin,
        tab_id: TabId,
        conversation_id: String,
        cancel: DropGuard,
    ) -> Self {
        Self {
            generation,
            key,
            origin,
            tab_id,
            conversation_id,
            _cancel: cancel,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Available,
    Partial,
    Missing,
    Denied,
    Expired,
    Changed,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Self::Available => "Read-only",
            Self::Partial => "Partial",
            Self::Missing => "Missing",
            Self::Denied => "Denied",
            Self::Expired => "Expired",
            Self::Changed => "Changed",
        }
    }

    fn note(self) -> &'static str {
        match self {
            Self::Available => "Committed worker turns · read-only",
            Self::Partial => "Partial history · only retained turns are shown",
            Self::Missing => "The exact source record is no longer available.",
            Self::Denied => "This session cannot read the source.",
            Self::Expired => "The source is outside its retention period.",
            Self::Changed => "The source no longer matches the verified record.",
        }
    }
}

fn unpack(read: VisitRead) -> (Status, Option<VisitBody>) {
    match read {
        VisitRead::Available(body) => (Status::Available, Some(body)),
        VisitRead::Partial(body) => (Status::Partial, Some(body)),
        VisitRead::Missing => (Status::Missing, None),
        VisitRead::Denied => (Status::Denied, None),
        VisitRead::Expired => (Status::Expired, None),
        VisitRead::Changed => (Status::Changed, None),
    }
}

#[derive(Debug)]
pub(crate) struct VisitWindowState {
    pub cursor: VisitCursor,
    pub text: ArtifactWindowState,
    status: Status,
    previous: Option<MainVisitKey>,
    next: Option<MainVisitKey>,
    /// A relation acts only when the widget painted the whole relation hint.
    pub(crate) relations_painted: Cell<bool>,
    /// A related open the key above started and that has not completed yet.
    pub(crate) opening: Option<VisitOpening>,
}

/// Only committed turn ordinals, tool names and authorized public text enter search/copy.
/// The Main result and report already have their own inspector; no spool or tool payloads.
fn write_turns(out: &mut impl Write, turns: &[VisitTurn]) -> fmt::Result {
    for (i, turn) in turns.iter().enumerate() {
        if i != 0 {
            out.write_str("\n\n")?;
        }
        write!(out, "Turn {}", turn.ordinal)?;
        if !turn.tool_names.is_empty() {
            out.write_str(" · Tools: ")?;
            for (i, name) in turn.tool_names.iter().enumerate() {
                if i != 0 {
                    out.write_str(", ")?;
                }
                out.write_str(name)?;
            }
        }
        if !turn.public_text.is_empty() {
            out.write_char('\n')?;
            out.write_str(&turn.public_text)?;
        }
    }
    Ok(())
}

// Revalidation before a key or paint must not keep a second transcript or format a new
// owned body on every pass. Compare the exact projected bytes against the reader in place.
struct Compare<'a>(&'a [u8]);
impl Write for Compare<'_> {
    fn write_str(&mut self, part: &str) -> fmt::Result {
        self.0 = self.0.strip_prefix(part.as_bytes()).ok_or(fmt::Error)?;
        Ok(())
    }
}

fn reader(status: Status, body: Option<&VisitBody>) -> ArtifactWindowState {
    let source = match body {
        Some(body) => {
            let mut source = String::new();
            write_turns(&mut source, &body.turns).expect("writing to a String is infallible");
            ArtifactBody::Source(source.into_bytes())
        }
        None => ArtifactBody::Unavailable(status.label().into()),
    };
    let mut text = ArtifactWindowState::new(String::new(), status.note().into(), source)
        .with_empty_text("No committed worker turns retained.");
    text.set_literal(true);
    text.prepare_visit_search();
    text
}

fn relation(key: &MainVisitKey, candidate: Option<MainVisitKey>) -> Option<MainVisitKey> {
    candidate.filter(|other| {
        other.conversation_id == key.conversation_id
            && (other.turn_id != key.turn_id || other.result != key.result)
    })
}

impl VisitWindowState {
    /// Build the reader. The asynchronous open runs this off the event loop; given a layout
    /// (theme and body width of the painting frame) it also lays the body out once, so the
    /// first paint reuses the cache.
    pub(crate) fn prepare(
        key: MainVisitKey,
        read: VisitRead,
        layout: Option<(Theme, u16)>,
    ) -> Self {
        #[cfg(test)]
        crate::infrastructure::runtime::visit_open::probe::prepared();
        let (status, mut body) = unpack(read);
        let text = reader(status, body.as_ref());
        let previous = relation(&key, body.as_mut().and_then(|b| b.previous.take()));
        let next = relation(&key, body.as_mut().and_then(|b| b.next.take()));
        let window = Self {
            cursor: VisitCursor {
                key,
                expires_at_ms: body.as_ref().and_then(|b| b.expires_at_ms),
                backing: body.as_mut().map(|b| std::mem::take(&mut b.backing)),
                attempt: body.map(|b| b.attempt),
            },
            text,
            status,
            previous,
            next,
            relations_painted: Cell::new(false),
            opening: None,
        };
        if let Some((theme, width)) = layout {
            drop(window.text.rows(&theme, width));
        }
        window
    }

    pub(crate) fn title(&self) -> String {
        match (&self.status, &self.cursor.attempt) {
            (Status::Available | Status::Partial, Some(id)) => format!(
                "Session Visit · {} · {}",
                sanitize_display_line(id.as_str()),
                self.status.label()
            ),
            _ => format!("Session Visit · {}", self.status.label()),
        }
    }

    pub(crate) fn relations(&self) -> (bool, bool) {
        (self.previous.is_some(), self.next.is_some())
    }

    /// On a visibility or body change drop *all* old bytes: raw, layout cache, filter and
    /// painted key availability. An unchanged read retains the exact browse position.
    fn update(&mut self, read: VisitRead) -> bool {
        let (mut status, mut body) = unpack(read);
        if let (Some(expected), Some(actual)) = (&self.cursor.attempt, &body)
            && expected != &actual.attempt
        {
            status = Status::Changed;
            body = None;
        }
        let same = self.status == status
            && match (&self.text.body, body.as_ref()) {
                (ArtifactBody::Unavailable(_), None) => true,
                (ArtifactBody::Source(bytes), Some(body)) => {
                    let mut cmp = Compare(bytes);
                    write_turns(&mut cmp, &body.turns).is_ok() && cmp.0.is_empty()
                }
                _ => false,
            };
        if !same {
            self.text = reader(status, body.as_ref());
            self.status = status;
        }
        let prev = relation(
            &self.cursor.key,
            body.as_mut().and_then(|b| b.previous.take()),
        );
        let next = relation(&self.cursor.key, body.as_mut().and_then(|b| b.next.take()));
        let links_changed = self.previous != prev || self.next != next;
        if links_changed {
            self.previous = prev;
            self.next = next;
        }
        self.cursor.expires_at_ms = body.as_ref().and_then(|b| b.expires_at_ms);
        self.cursor.backing = body.as_mut().map(|b| std::mem::take(&mut b.backing));
        if self.cursor.attempt.is_none() {
            self.cursor.attempt = body.map(|b| b.attempt);
        }
        if !same || links_changed {
            self.relations_painted.set(false);
        }
        !same || links_changed
    }
}

pub(crate) const SLOT: Slot = Slot {
    surface: Surface::SessionVisit,
    context: Context::SessionVisit,
    copy: MissionCmd::CopySource,
    noun: "session visit text",
    get: |state| state.session_visit.as_ref().map(|v| &v.text),
    get_mut: |state| state.session_visit.as_mut().map(|v| &mut v.text),
};

/// A deliberate relation read revalidates its source *before* checking this edge and
/// resolving its target. Never infer an edge from nearby rows, even under the same root.
pub(crate) fn can_visit_related(state: &TuiState, target: &MainVisitKey) -> bool {
    !state.local_read_only
        && text::is_top_in(state, &SLOT)
        && state
            .session_visit
            .as_ref()
            .is_some_and(|v| v.previous.as_ref() == Some(target) || v.next.as_ref() == Some(target))
}

pub(crate) fn open_visit(state: &mut TuiState, window: VisitWindowState) {
    if state.local_read_only {
        return;
    }
    if let Some(v) = &state.session_visit {
        if !text::is_top_in(state, &SLOT) {
            return;
        }
        // Both production entries open a different key: an Inspector origin has no cursor,
        // and a related target differs by `relation`.
        if v.cursor.key == window.cursor.key || !can_visit_related(state, &window.cursor.key) {
            return;
        }
    } else if !matches!(
        state.modal_stack.top().map(|f| f.surface),
        Some(Surface::ToolInspector(_))
    ) {
        return;
    }
    reducer::open_session_visit(state, window);
}

pub(crate) fn refresh_visit(state: &mut TuiState, read: VisitRead) {
    if let Some(v) = &mut state.session_visit
        && v.update(read)
    {
        if let Some(frame) = state
            .modal_stack
            .all_frames_mut()
            .find(|f| f.surface == Surface::SessionVisit)
        {
            frame.tool_title = Some(v.title());
        }
        state.needs_redraw = true;
    }
}

pub(crate) fn on_visit_key(state: &mut TuiState, key: Key, host: Host) -> InputAction {
    use crate::adapters::tui::keymap::table_overlay::OverlayCmd;

    text::on_key_in(state, key, host, &SLOT, &mut |state, cmd| {
        let v = state.session_visit.as_ref()?;
        let target = match cmd {
            CommandId::Mission(MissionCmd::CopySource) => {
                return Some(if v.text.live().copy && v.text.has_source() {
                    InputAction::CopySessionVisit
                } else {
                    InputAction::Consumed
                });
            }
            CommandId::Overlay(OverlayCmd::InspectorSearch) => {
                return Some(if v.text.live().search && v.text.has_source() {
                    InputAction::SearchSessionVisit
                } else {
                    InputAction::Consumed
                });
            }
            CommandId::Mission(MissionCmd::PreviousVisit) => v.previous.as_ref(),
            CommandId::Mission(MissionCmd::NextVisit) => v.next.as_ref(),
            _ => return None,
        };
        Some(
            target
                .filter(|_| v.relations_painted.get())
                .map_or(InputAction::Consumed, |key| {
                    InputAction::OpenSessionVisit(key.clone())
                }),
        )
    })
}

/// Runtime verifies the open source and backing lease in the same dispatch
/// before this effect. No stale or queued clipboard payload is retained.
pub(crate) fn copy_visit(state: &mut TuiState) -> InputAction {
    if !state.local_read_only
        && text::is_top_in(state, &SLOT)
        && state
            .session_visit
            .as_ref()
            .is_some_and(|v| !v.text.search.editing && v.text.live().copy && v.text.has_source())
    {
        text::copy_in(state, &SLOT);
    }
    InputAction::Consumed
}

/// The same checked lease authorizes entry into the native filter editor.
pub(crate) fn begin_visit_search(state: &mut TuiState) {
    if !state.local_read_only
        && text::is_top_in(state, &SLOT)
        && let Some(v) = state.session_visit.as_mut()
        && v.text.live().search
        && v.text.has_source()
    {
        v.text.search.open();
        state.needs_redraw = true;
    }
}

pub(crate) fn on_visit_wheel(state: &mut TuiState, delta: ScrollDelta) {
    text::on_wheel_in(state, &SLOT, delta);
}

pub(crate) fn visit_search_editing(state: &TuiState) -> bool {
    text::search_editing_in(state, &SLOT)
}

pub(crate) fn visit_search_changed(state: &mut TuiState) {
    text::search_changed_in(state, &SLOT);
}

pub(crate) fn accept_visit_search(state: &mut TuiState) {
    // Keep a no-match filter in the reader (the native body displays "no line matches").
    // Unlike the roster reader, never echo a Visit query into the persistent notice log.
    if let Some(v) = state.session_visit.as_mut() {
        v.text.search.accept();
        state.needs_redraw = true;
    }
}

pub(crate) fn clear_visit_search(state: &mut TuiState) {
    text::clear_search_in(state, &SLOT);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::turn::{PartId, TurnId};
    use crate::infrastructure::subagent::session_history::hash_body;
    use tokio_util::sync::CancellationToken;

    fn key(root: &str, part: u64) -> MainVisitKey {
        MainVisitKey {
            conversation_id: root.into(),
            turn_id: TurnId("turn".into()),
            invocation: PartId(part),
            result: PartId(part + 1),
            revision: hash_body("literal"),
        }
    }

    fn record(attempt: AgentId, next: Option<MainVisitKey>) -> VisitBody {
        VisitBody {
            attempt,
            previous: None,
            next,
            report: None,
            expires_at_ms: Some(123),
            backing: VisitBacking::default(),
            turns: vec![VisitTurn {
                ordinal: 1,
                tool_names: vec!["Read".into()],
                public_text: "retained text".into(),
            }],
        }
    }

    #[test]
    fn revocation_drops_body_filter_layout_and_edges_without_following_a_successor() {
        let original = AgentId::new();
        let body = record(original.clone(), Some(key("main", 3)));
        let mut visit =
            VisitWindowState::prepare(key("main", 1), VisitRead::Available(body.clone()), None);
        visit.text.search.open();
        visit.text.search.query.set("retained");
        visit.relations_painted.set(true);
        let theme = TuiState::new(60, 16).theme;
        assert!(!visit.text.rows(&theme, 48).is_empty());
        assert_eq!(visit.text.layouts_built(), 1);
        visit.text.set_live(text::Live {
            copy: true,
            search: true,
            ..Default::default()
        });
        assert!(!visit.update(VisitRead::Available(body)));
        assert_eq!(visit.text.search.query.as_str(), "retained");
        assert!(visit.cursor.backing.is_some());

        assert!(visit.update(VisitRead::Denied));
        assert!(!visit.title().contains(original.as_str()));
        assert!(!visit.text.has_source());
        assert_eq!(visit.text.layouts_built(), 0);
        assert_eq!(visit.text.live(), text::Live::default());
        assert!(!visit.text.search.editing);
        assert!(visit.text.search.query.is_empty());
        assert_eq!(visit.cursor.expires_at_ms, None);
        assert!(visit.cursor.backing.is_none());
        assert_eq!(visit.relations(), (false, false));
        assert!(!visit.relations_painted.get());
        assert!(visit.update(VisitRead::Available(record(AgentId::new(), None))));
        assert_eq!(visit.title(), "Session Visit · Changed");
        assert!(!visit.text.has_source());
        assert!(visit.cursor.backing.is_none());
    }

    #[test]
    fn related_visit_reuses_one_frame_and_back_restores_exact_caller_at_stack_cap() {
        let mut state = TuiState::new(60, 16);
        state.input_buffer = "parked draft".into();
        state.cursor_position = 4;
        let base = state.focus.clone();
        for surface in [Surface::Help, Surface::UsagePanel, Surface::CommandPalette] {
            reducer::open(&mut state, surface);
        }
        reducer::open_tool(
            &mut state,
            ToolTarget {
                conversation_id: "main".into(),
                tool_call_id: "selected".into(),
            },
            "Tool · task #1".into(),
            8,
        );
        let caller = state.modal_stack.top().unwrap().clone();
        let target = key("main", 3);
        let mut first = record(AgentId::new(), Some(target.clone()));
        first.previous = Some(key("other", 7));
        open_visit(
            &mut state,
            VisitWindowState::prepare(key("main", 1), VisitRead::Available(first), None),
        );
        let frame_id = state.modal_stack.top().unwrap().id;
        assert!(!can_visit_related(&state, &key("other", 7)));
        assert!(can_visit_related(&state, &target));
        assert_eq!(
            on_visit_key(&mut state, Key::Right, Host::Local),
            InputAction::Consumed
        );
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 16)).unwrap();
        term.draw(|f| {
            let area = f.area();
            crate::adapters::tui::widgets::agent_window::render_visit(
                f,
                area,
                &state,
                state.modal_stack.top().unwrap().title(),
                true,
            );
        })
        .unwrap();
        assert_eq!(
            on_visit_key(&mut state, Key::Right, Host::Local),
            InputAction::OpenSessionVisit(target.clone())
        );
        assert_eq!(
            on_visit_key(&mut state, Key::Left, Host::Local),
            InputAction::Consumed
        );
        assert_eq!(
            on_visit_key(&mut state, Key::Char('c'), Host::Local),
            InputAction::CopySessionVisit
        );
        assert_eq!(
            on_visit_key(&mut state, Key::Char('/'), Host::Local),
            InputAction::SearchSessionVisit
        );
        assert!(
            !visit_search_editing(&state),
            "explicit reload precedes searching"
        );
        let second = AgentId::new();
        open_visit(
            &mut state,
            VisitWindowState::prepare(
                target.clone(),
                VisitRead::Partial(record(second.clone(), None)),
                None,
            ),
        );
        let frame = state.modal_stack.top().unwrap();
        assert_eq!(frame.id, frame_id);
        assert_eq!(state.modal_stack.len(), 4);
        assert!(frame.title().contains(second.as_str()));
        assert!(frame.title().contains("Partial"));
        assert_eq!(visit_cursor(&state).unwrap().key, target);
        assert_eq!(
            on_visit_key(&mut state, Key::Char('p'), Host::Local),
            InputAction::Consumed
        );
        assert_eq!(
            on_visit_key(&mut state, Key::Char('x'), Host::Local),
            InputAction::Consumed
        );
        on_visit_key(&mut state, Key::Esc, Host::Local);
        assert_eq!(state.modal_stack.top(), Some(&caller));
        assert!(state.session_visit.is_none());
        assert_eq!(
            (state.input_buffer.as_str(), state.cursor_position),
            ("parked draft", 4)
        );
        assert_eq!(state.focus, base);
    }

    #[test]
    fn compact_and_search_paints_revoke_relation_disclosure() {
        let mut state = TuiState::new(60, 16);
        reducer::open_tool(
            &mut state,
            ToolTarget {
                conversation_id: "main".into(),
                tool_call_id: "selected".into(),
            },
            "Tool · task #1".into(),
            8,
        );
        let target = key("main", 3);
        open_visit(
            &mut state,
            VisitWindowState::prepare(
                key("main", 1),
                VisitRead::Available(record(AgentId::new(), Some(target.clone()))),
                None,
            ),
        );
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 16)).unwrap();
        let mut draw = |state: &TuiState, width| {
            term.draw(|frame| {
                crate::adapters::tui::widgets::agent_window::render_visit(
                    frame,
                    ratatui::layout::Rect::new(0, 0, width, 16),
                    state,
                    state.modal_stack.top().unwrap().title(),
                    true,
                );
            })
            .unwrap();
        };
        draw(&state, 60);
        assert!(
            state
                .session_visit
                .as_ref()
                .unwrap()
                .relations_painted
                .get()
        );
        draw(&state, 10);
        assert!(
            !state
                .session_visit
                .as_ref()
                .unwrap()
                .relations_painted
                .get()
        );
        assert_eq!(
            on_visit_key(&mut state, Key::Right, Host::Local),
            InputAction::Consumed
        );
        draw(&state, 60);
        begin_visit_search(&mut state);
        assert!(visit_search_editing(&state));
        draw(&state, 60);
        assert!(
            !state
                .session_visit
                .as_ref()
                .unwrap()
                .relations_painted
                .get()
        );
        clear_visit_search(&mut state);
        draw(&state, 60);
        assert_eq!(
            on_visit_key(&mut state, Key::Right, Host::Local),
            InputAction::OpenSessionVisit(target)
        );
    }

    #[test]
    fn unavailable_disclosures_never_retain_source_or_authorized_edges() {
        for (read, label) in [
            (VisitRead::Missing, "Missing"),
            (VisitRead::Denied, "Denied"),
            (VisitRead::Expired, "Expired"),
            (VisitRead::Changed, "Changed"),
        ] {
            let attempt = AgentId::new();
            let mut visit = VisitWindowState::prepare(
                key("main", 1),
                VisitRead::Partial(record(attempt.clone(), Some(key("main", 3)))),
                None,
            );
            visit.text.search.open();
            visit.text.search.query.set("retained");
            assert!(visit.update(read));
            assert_eq!(visit.title(), format!("Session Visit · {label}"));
            assert!(!visit.title().contains(attempt.as_str()));
            assert!(!visit.text.has_source());
            assert!(visit.text.search.query.is_empty());
            assert!(!visit.text.search.editing);
            assert!(visit.cursor.backing.is_none());
            assert_eq!(visit.relations(), (false, false));
        }
    }

    fn layout() -> (Theme, u16) {
        (TuiState::new(60, 16).theme, 48)
    }

    fn opening(origin: OpeningOrigin) -> (VisitOpening, CancellationToken) {
        let token = CancellationToken::new();
        let pending = VisitOpening::new(
            1,
            key("main", 1),
            origin,
            0,
            "main".into(),
            token.clone().drop_guard(),
        );
        (pending, token)
    }

    fn tool_frame(state: &mut TuiState) -> u64 {
        reducer::open_tool(
            state,
            ToolTarget {
                conversation_id: "main".into(),
                tool_call_id: "selected".into(),
            },
            "Tool · task #1".into(),
            8,
        );
        state.modal_stack.top().unwrap().id
    }

    fn painted(state: &TuiState) -> String {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 16)).unwrap();
        term.draw(|frame| {
            let area = frame.area();
            crate::adapters::tui::widgets::agent_window::render_visit(
                frame,
                area,
                state,
                state.modal_stack.top().unwrap().title(),
                true,
            );
        })
        .unwrap();
        let buffer = term.backend().buffer();
        (0..16_u16)
            .map(|y| {
                (0..60_u16)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn visit_async_open_prepare_builds_every_status_and_prewarms_only_the_layout() {
        let attempt = AgentId::new();
        let readable = "Turn 1 · Tools: Read\nretained text";
        let cases = [
            (
                VisitRead::Available(record(attempt.clone(), None)),
                Status::Available,
            ),
            (
                VisitRead::Partial(record(attempt.clone(), None)),
                Status::Partial,
            ),
            (VisitRead::Missing, Status::Missing),
            (VisitRead::Denied, Status::Denied),
            (VisitRead::Expired, Status::Expired),
            (VisitRead::Changed, Status::Changed),
        ];
        for (read, status) in cases {
            let plain = VisitWindowState::prepare(key("main", 1), read.clone(), None);
            let warmed = VisitWindowState::prepare(key("main", 1), read, Some(layout()));
            assert_eq!(plain.status, status);
            assert_eq!(warmed.status, status);
            assert_eq!(plain.title(), warmed.title());
            assert!(plain.opening.is_none() && warmed.opening.is_none());
            assert!(plain.text.body == warmed.text.body);
            assert_eq!(plain.relations(), warmed.relations());
            assert_eq!(plain.cursor.expires_at_ms, warmed.cursor.expires_at_ms);
            assert_eq!(plain.text.layouts_built(), 0);
            assert_eq!(warmed.text.layouts_built(), 1);
            let (theme, width) = layout();
            drop(warmed.text.rows(&theme, width));
            assert_eq!(
                warmed.text.layouts_built(),
                1,
                "{status:?}: the first paint reuses the prewarmed layout"
            );
            drop(warmed.text.rows(&theme, width - 1));
            assert_eq!(warmed.text.layouts_built(), 2);
            if matches!(status, Status::Available | Status::Partial) {
                assert!(plain.title().contains(attempt.as_str()));
                assert!(plain.title().ends_with(status.label()));
                assert!(matches!(
                    &plain.text.body,
                    ArtifactBody::Source(bytes) if bytes.as_slice() == readable.as_bytes()
                ));
                assert_eq!(plain.cursor.expires_at_ms, Some(123));
                assert!(plain.cursor.backing.is_some());
            } else {
                assert_eq!(plain.title(), format!("Session Visit · {}", status.label()));
                assert!(!plain.text.has_source());
                assert!(plain.cursor.backing.is_none());
            }
        }
    }

    /// A row that reached the 128-turn cap resolves `Partial` with turns 1..=127: slot 128 is
    /// a durable cap marker and is never a transcript turn. The async `prepare` must paint the
    /// literal bytes the base reader painted.
    #[test]
    fn visit_async_open_partial_row_without_the_slot_128_marker_keeps_the_base_bytes() {
        let attempt = AgentId::new();
        let mut body = record(attempt.clone(), None);
        body.turns = (1..=127)
            .map(|ordinal| VisitTurn {
                ordinal,
                tool_names: Vec::new(),
                public_text: format!("t{ordinal}"),
            })
            .collect();
        let expected = (1..=127)
            .map(|n| format!("Turn {n}\nt{n}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        assert!(expected.starts_with("Turn 1\nt1\n\nTurn 2\nt2\n\nTurn 3\nt3"));
        assert!(expected.ends_with("Turn 126\nt126\n\nTurn 127\nt127"));
        assert!(!expected.contains("Turn 128"));
        for layout in [None, Some(layout())] {
            let window =
                VisitWindowState::prepare(key("main", 1), VisitRead::Partial(body.clone()), layout);
            assert_eq!(window.status, Status::Partial);
            assert!(window.title().ends_with("Partial"));
            assert!(matches!(
                &window.text.body,
                ArtifactBody::Source(bytes) if bytes.as_slice() == expected.as_bytes()
            ));
        }
    }

    #[test]
    fn visit_async_open_prepared_reader_can_cross_from_the_reader_thread() {
        fn send<T: Send>() {}
        send::<VisitWindowState>();
        send::<Theme>();
    }

    #[test]
    fn visit_async_open_cancel_opening_acts_only_on_the_top_frame() {
        let mut state = TuiState::new(60, 16);
        let id = tool_frame(&mut state);
        let (pending, token) = opening(OpeningOrigin::Inspector {
            frame_id: id,
            tab: ToolTab::Output,
        });
        state.tool_inspectors.get_mut(&id).unwrap().visit_opening = Some(pending);
        reducer::open(&mut state, Surface::Help);
        state.needs_redraw = false;
        assert!(!cancel_opening(&mut state), "a key above never cancels it");
        assert!(!token.is_cancelled());
        assert!(pending_open(&state).is_some());
        assert!(!state.needs_redraw);

        reducer::close(&mut state, Surface::Help);
        assert!(cancel_opening(&mut state));
        assert!(
            token.is_cancelled(),
            "dropping the pending open cancels the read"
        );
        assert!(pending_open(&state).is_none());
        assert!(state.needs_redraw);
        assert!(!cancel_opening(&mut state), "nothing is pending any more");
    }

    #[test]
    fn visit_async_open_related_cue_replaces_the_relation_hint_and_keeps_relations_inert() {
        let mut state = TuiState::new(60, 16);
        tool_frame(&mut state);
        let target = key("main", 3);
        open_visit(
            &mut state,
            VisitWindowState::prepare(
                key("main", 1),
                VisitRead::Available(record(AgentId::new(), Some(target.clone()))),
                None,
            ),
        );
        let frame_id = state.modal_stack.top().unwrap().id;
        let (pending, token) = opening(OpeningOrigin::Visit {
            frame_id,
            cursor_key: key("main", 1),
        });
        state.session_visit.as_mut().unwrap().opening = Some(pending);
        assert!(painted(&state).contains(" [Esc] Cancel  Opening related source… "));
        assert!(
            !state
                .session_visit
                .as_ref()
                .unwrap()
                .relations_painted
                .get()
        );
        assert_eq!(
            on_visit_key(&mut state, Key::Right, Host::Local),
            InputAction::Consumed,
            "the relation key is inert while the open is pending"
        );

        assert!(cancel_opening(&mut state));
        assert!(token.is_cancelled());
        assert!(!painted(&state).contains("Cancel"));
        assert!(
            state
                .session_visit
                .as_ref()
                .unwrap()
                .relations_painted
                .get()
        );
        assert_eq!(
            on_visit_key(&mut state, Key::Right, Host::Local),
            InputAction::OpenSessionVisit(target)
        );
    }
}

fn visit_source<'a>(
    conversation: &'a Conversation,
    target: &ToolTarget,
) -> Option<(
    &'a crate::domain::models::Turn,
    PartId,
    PartId,
    &'a crate::domain::models::ToolOutput,
)> {
    if target.conversation_id != conversation.id {
        return None;
    }
    let mut turns = conversation.turns.iter().filter(|turn| {
        turn.parts.iter().any(|part| matches!(part, TurnPart::ToolInvocation { id, .. }
            if crate::domain::models::turn::tool_call_id_matches(&turn.id, *id, &target.tool_call_id)))
    });
    let turn = turns.next()?;
    if turns.next().is_some()
        || turn.role != MessageRole::Assistant
        || turn.history_source != HistorySource::Local
        || conversation
            .turns
            .iter()
            .filter(|other| other.id == turn.id)
            .count()
            != 1
    {
        return None;
    }
    let mut calls = turn.parts.iter().filter_map(|part| match part {
        TurnPart::ToolInvocation { id, tool, .. }
            if crate::domain::models::turn::tool_call_id_matches(
                &turn.id,
                *id,
                &target.tool_call_id,
            ) =>
        {
            Some((*id, tool))
        }
        _ => None,
    });
    let (invocation, tool) = calls.next()?;
    if calls.next().is_some() || tool != "task" {
        return None;
    }
    let mut results = turn.parts.iter().filter_map(|part| match part {
        TurnPart::ToolResult { id, refs, output } if *refs == invocation => Some((*id, output)),
        _ => None,
    });
    let (result, output) = results.next()?;
    // Old/no-link journals have no availability metadata. Never infer authority
    // from result text; canonical resolution still rechecks the journal link.
    let deadline = *turn.visit_deadlines.get(&result)?;
    if results.next().is_some()
        || output.is_error
        || turn
            .parts
            .iter()
            .filter(|part| matches!(part, TurnPart::ToolResult { id, .. } if *id == result))
            .count()
            != 1
        || crate::infrastructure::subagent::session_history::SessionHistory::check_expiry(Some(
            deadline,
        ))
        .is_some()
    {
        return None;
    }
    Some((turn, invocation, result, output))
}

/// Check an already-pinned result on repaint without examining its raw bytes.
pub(crate) fn visit_revision_is_current(
    conversation: &Conversation,
    target: &ToolTarget,
    key: &MainVisitKey,
    revision: Option<u64>,
) -> bool {
    visit_source(conversation, target).is_some_and(|(turn, invocation, result, _)| {
        key.conversation_id == conversation.id
            && key.turn_id == turn.id
            && key.invocation == invocation
            && key.result == result
            && revision == Some(turn.parts.revision())
    })
}

/// Resolve a committed, host-authorized task result without choosing an ambiguous identity.
pub(crate) fn visit_key(conversation: &Conversation, target: &ToolTarget) -> Option<MainVisitKey> {
    let (turn, invocation, result, output) = visit_source(conversation, target)?;
    Some(MainVisitKey {
        conversation_id: conversation.id.clone(),
        turn_id: turn.id.clone(),
        invocation,
        result,
        revision: crate::infrastructure::subagent::session_history::hash_body(&output.content),
    })
}

pub(crate) fn visit_cursor(state: &TuiState) -> Option<&VisitCursor> {
    state.session_visit.as_ref().map(|visit| &visit.cursor)
}

/// Whether `read` is readable and its fresh edges name `target` after the same `relation`
/// filter the reader applies.
pub(crate) fn edge_names(read: &VisitRead, key: &MainVisitKey, target: &MainVisitKey) -> bool {
    let (VisitRead::Available(body) | VisitRead::Partial(body)) = read else {
        return false;
    };
    [&body.previous, &body.next]
        .into_iter()
        .any(|edge| relation(key, edge.clone()).as_ref() == Some(target))
}

/// The pending-open slot of the top frame's payload.
pub(crate) fn top_slot(state: &mut TuiState) -> Option<&mut Option<VisitOpening>> {
    let surface = state.modal_stack.top()?.surface;
    match surface {
        Surface::ToolInspector(ToolFrameKey(id)) => state
            .tool_inspectors
            .get_mut(&id)
            .map(|tool| &mut tool.visit_opening),
        Surface::SessionVisit => state.session_visit.as_mut().map(|visit| &mut visit.opening),
        _ => None,
    }
}

/// The one pending open, whichever frame owns it (covered frames included).
pub(crate) fn pending_open(state: &TuiState) -> Option<&VisitOpening> {
    state
        .tool_inspectors
        .values()
        .find_map(|tool| tool.visit_opening.as_ref())
        .or_else(|| {
            state
                .session_visit
                .as_ref()
                .and_then(|visit| visit.opening.as_ref())
        })
}

/// Drop every pending open; dropping cancels its read.
pub(crate) fn clear_pending(state: &mut TuiState) {
    for tool in state.tool_inspectors.values_mut() {
        tool.visit_opening = None;
    }
    if let Some(visit) = state.session_visit.as_mut() {
        visit.opening = None;
    }
}

/// `Esc` / `Ctrl+C` while the top frame's open is pending: cancel it and do nothing else.
pub(crate) fn cancel_opening(state: &mut TuiState) -> bool {
    let cancelled = top_slot(state).is_some_and(|slot| slot.take().is_some());
    state.needs_redraw |= cancelled;
    cancelled
}

pub(crate) fn ignores_raw_key(state: &TuiState, key: &crossterm::event::KeyEvent) -> bool {
    text::ignores_raw_key_in(state, &SLOT, key)
}
