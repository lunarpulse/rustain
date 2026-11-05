//! Attached-client presentation only. Socket senders, request correlation and
//! interruption remain in the runtime; shared state owns every editable field.

use std::collections::{HashMap, VecDeque};

#[derive(Default)]
pub(crate) struct AttachedPresentation {
    pub(crate) consent: VecDeque<PendingConsentApproval>,
    pub(crate) consolidation_token: Option<crate::adapters::daemon::protocol::ProposalToken>,
    pub(crate) retract_confirm: Option<(String, String)>,
    pub(crate) approvals: AttachedApprovals,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingConsentApproval {
    pub(crate) request_id: crate::domain::models::RequestId,
    pub(crate) tool: String,
    pub(crate) input_preview: String,
}

#[derive(Default)]
pub(crate) struct AttachedApprovals {
    pub(crate) order: std::collections::VecDeque<crate::domain::models::tool_call::RequestId>,
    pub(crate) pending: HashMap<crate::domain::models::tool_call::RequestId, AttachedApproval>,
    pub(crate) review: Option<crate::domain::models::tool_call::RequestId>,
    pub(crate) feedback: Option<(crate::domain::models::tool_call::RequestId, String)>,
    pub(crate) painted: Option<crate::domain::models::tool_call::RequestId>,
    pub(crate) scroll: usize,
    pub(crate) scroll_max: usize,
    pub(crate) presentation: Option<AttachedApprovalPresentation>,
    /// `Esc` / `Ctrl+C` answered with a cue (A8): the host posts it, never a decision.
    pub(crate) cue: bool,
}

pub(crate) struct AttachedApproval {
    pub(crate) tool: String,
    pub(crate) source: crate::domain::models::tool_call::ApprovalSource,
    pub(crate) risk: crate::domain::models::ToolRisk,
    pub(crate) summary: String,
    pub(crate) assembly: Option<crate::adapters::daemon::protocol::ExactInputAssembly>,
    pub(crate) value: Option<serde_json::Value>,
}

pub(crate) struct AttachedApprovalPresentation {
    pub(crate) id: crate::domain::models::tool_call::RequestId,
    pub(crate) width: u16,
    pub(crate) queued: usize,
    pub(crate) reviewing: bool,
    pub(crate) ready: bool,
    pub(crate) header: Vec<ratatui::text::Line<'static>>,
    pub(crate) body: Vec<ratatui::text::Line<'static>>,
    pub(crate) footer: Vec<ratatui::text::Line<'static>>,
}
