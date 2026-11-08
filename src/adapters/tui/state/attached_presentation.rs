//! Attached-client presentation only. Socket senders, request correlation and
//! interruption remain in the runtime; shared state owns every editable field.
//! (Slice 6-B moved the approval and consent queues into the decision queue; the
//! consolidation token and the retract confirmation follow in 6-D.)

#[derive(Default)]
pub(crate) struct AttachedPresentation {
    pub(crate) consolidation_token: Option<crate::adapters::daemon::protocol::ProposalToken>,
    pub(crate) retract_confirm: Option<(String, String)>,
}
