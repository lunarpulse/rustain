//! Skill trust as a queue item (Story 21.6 6-C): a workspace skill that waits for the
//! user's trust before it is activated.
//!
//! The model-driven prompt (`AppEvent::SkillTrustPrompt`, answered through a oneshot)
//! and the slash-driven one (`AppEvent::AskActivateSkill`, completed by an event) are ONE
//! FIFO kind. Inspect is a sub-context of the frame: the whole skill file scrolls in the
//! body, `Esc` returns to the undecided prompt.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::oneshot;

use super::handle::ReplyHandle;
use super::{
    Arrival, DecisionDraft, DecisionKey, DecisionKind, DecisionSubject, Raised, raise, withdraw_key,
};
use crate::adapters::tui::state::TuiState;
use crate::domain::models::SkillTrustResponse;

/// Where the answer goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillOrigin {
    /// The model activated the skill and waits on the reply.
    Model {
        reply: ReplyHandle<SkillTrustResponse>,
    },
    /// The user ran `/<skill> <arguments>`: the answer completes the activation.
    User {
        arguments: String,
        conversation_id: String,
    },
}

/// One skill awaiting trust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillTrustPayload {
    pub skill_name: String,
    pub skill_file: PathBuf,
    pub origin: SkillOrigin,
    /// The skill file's content, read once on the first `i` (never on the paint path).
    pub inspect_content: Option<String>,
    /// The file reader is open (a sub-context: `Esc` returns to the prompt).
    pub inspecting: bool,
}

impl SkillTrustPayload {
    /// Answer a model-driven prompt. False for a user-driven one, or when nobody waits.
    pub fn reply_model(&self, response: SkillTrustResponse) -> bool {
        match &self.origin {
            SkillOrigin::Model { reply } => reply.send(response),
            SkillOrigin::User { .. } => false,
        }
    }
}

/// Distinguishes two model prompts for the same skill (FIFO, never merged).
static MODEL_PROMPTS: AtomicU64 = AtomicU64::new(0);

fn payload(item: &super::DecisionItem) -> Option<&SkillTrustPayload> {
    match &item.subject {
        DecisionSubject::SkillTrust(payload) => Some(payload),
        _ => None,
    }
}

/// A model-driven prompt nobody waits for any more (its turn's receiver is gone).
pub(super) fn is_orphan(subject: &DecisionSubject) -> bool {
    matches!(
        subject,
        DecisionSubject::SkillTrust(p)
            if matches!(&p.origin, SkillOrigin::Model { reply } if reply.is_closed())
    )
}

/// The conversation a user-driven prompt will activate its skill in.
pub(super) fn user_origin(subject: &DecisionSubject) -> Option<&str> {
    match subject {
        DecisionSubject::SkillTrust(p) => match &p.origin {
            SkillOrigin::User {
                conversation_id, ..
            } => Some(conversation_id.as_str()),
            SkillOrigin::Model { .. } => None,
        },
        _ => None,
    }
}

fn payload_mut<'a>(
    state: &'a mut TuiState,
    key: &DecisionKey,
) -> Option<&'a mut SkillTrustPayload> {
    match &mut state.decisions.item_mut(key)?.subject {
        DecisionSubject::SkillTrust(payload) => Some(payload),
        _ => None,
    }
}

/// Raise a model-driven prompt (Automatic arrival); prompts queue FIFO.
pub fn raise_model_skill_trust(
    state: &mut TuiState,
    skill_name: String,
    skill_file: PathBuf,
    tx: oneshot::Sender<SkillTrustResponse>,
) -> Raised {
    let nonce = MODEL_PROMPTS.fetch_add(1, Ordering::Relaxed);
    let draft = DecisionDraft::new(DecisionKind::SkillTrust, "")
        .source(format!("model:{nonce}:{}", skill_file.display()))
        .summary(skill_name.clone())
        .subject(DecisionSubject::SkillTrust(Box::new(SkillTrustPayload {
            skill_name,
            skill_file,
            origin: SkillOrigin::Model {
                reply: ReplyHandle::new(tx),
            },
            inspect_content: None,
            inspecting: false,
        })));
    raise(state, draft, Arrival::Automatic)
}

/// Raise the prompt of a slash-driven activation (the user started it: Requested
/// arrival). `None` while another user-driven prompt waits: the legacy drop, the caller
/// says so with its Info notice.
pub fn raise_user_skill_trust(
    state: &mut TuiState,
    conversation_id: &str,
    skill_name: String,
    skill_file: PathBuf,
    arguments: String,
) -> Option<Raised> {
    // A stale prompt (its source settled elsewhere) does not block a new user request.
    if state.decisions.items().any(|item| {
        !item.is_stale()
            && payload(item).is_some_and(|p| matches!(p.origin, SkillOrigin::User { .. }))
    }) {
        return None;
    }
    let draft = DecisionDraft::new(DecisionKind::SkillTrust, "")
        .source(format!("user:{}", skill_file.display()))
        .summary(skill_name.clone())
        .subject(DecisionSubject::SkillTrust(Box::new(SkillTrustPayload {
            skill_name,
            skill_file,
            origin: SkillOrigin::User {
                arguments,
                conversation_id: conversation_id.to_owned(),
            },
            inspect_content: None,
            inspecting: false,
        })));
    Some(raise(state, draft, Arrival::Requested))
}

/// Remove every waiting prompt (a turn was cancelled), returning the payloads so the
/// caller declines each (the model's reply, the user's activation). An item whose frame
/// is open turns stale instead and waits for Close.
pub fn drain_skill_trust(state: &mut TuiState) -> Vec<SkillTrustPayload> {
    let waiting: Vec<(DecisionKey, SkillTrustPayload)> = state
        .decisions
        .items()
        .filter_map(|item| payload(item).map(|p| (item.key(), p.clone())))
        .collect();
    for (key, _) in &waiting {
        withdraw_key(state, key, super::NO_LONGER_PENDING);
    }
    waiting.into_iter().map(|(_, payload)| payload).collect()
}

/// The focused prompt whose file has not been read yet: what the host reads on `i`.
#[must_use]
pub fn inspect_target(state: &TuiState) -> Option<(DecisionKey, PathBuf)> {
    let key = super::focused_key(state)?;
    let payload = payload(state.decisions.item(&key)?)?;
    payload
        .inspect_content
        .is_none()
        .then(|| (key, payload.skill_file.clone()))
}

/// The host read the file: the reader opens at its first row.
pub fn open_inspect(state: &mut TuiState, key: &DecisionKey, content: String) {
    let opened = payload_mut(state, key).map(|payload| {
        payload.inspect_content = Some(content);
        payload.inspecting = true;
    });
    if opened.is_some() {
        state.decisions.scroll = 0;
        *state.decisions.body_layout.get_mut() = None;
        state.needs_redraw = true;
    }
}

/// Back from the reader to the undecided prompt.
pub(super) fn leave_inspect(state: &mut TuiState, key: &DecisionKey) {
    if let Some(payload) = payload_mut(state, key) {
        payload.inspecting = false;
    }
    state.decisions.scroll = 0;
}

/// Open the reader on content already read (a second `i`).
pub(super) fn reopen_inspect(state: &mut TuiState, key: &DecisionKey) -> bool {
    match payload_mut(state, key) {
        Some(payload) if payload.inspect_content.is_some() => {
            payload.inspecting = true;
            state.decisions.scroll = 0;
            true
        }
        _ => false,
    }
}
