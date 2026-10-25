//! Attached team frame family (Story 19.21c, A-15).
use super::*;
use crate::adapters::daemon::{protocol, team_proposal};

/// Leave a small, bounded request key so even a refusal frame is serializable.
pub(super) const MAX_TEAM_REQUEST_BYTES: usize = 128;
pub(super) enum TeamSendPayload {
    Human {
        recipients: Vec<String>,
        text: String,
        message_type: Option<crate::domain::models::SemanticMessageType>,
        subject: Option<String>,
        stories: Option<crate::domain::models::StoryManifest>,
        reply_to: Option<String>,
    },
    #[cfg(feature = "a2a")]
    Agent(Arc<crate::domain::models::TeamProposal>),
}

#[cfg(feature = "a2a")]
impl TeamSendPayload {
    pub(super) fn request(&self) -> crate::adapters::a2a::send::SendRequest<'_> {
        match self {
            Self::Human {
                recipients,
                text,
                message_type,
                subject,
                stories,
                reply_to,
            } => crate::adapters::a2a::send::SendRequest {
                recipients,
                text,
                message_type: *message_type,
                reply_to: reply_to.as_deref(),
                subject: subject.as_deref(),
                stories: stories.as_ref(),
                authorship: crate::domain::models::Authorship::HumanWritten,
                send_authorization: crate::domain::models::SendAuthorization::HumanDecided,
            },
            Self::Agent(proposal) => crate::adapters::a2a::send::SendRequest {
                recipients: &proposal.recipients,
                text: &proposal.body,
                message_type: Some(proposal.message_type),
                reply_to: None,
                subject: None,
                stories: None,
                authorship: crate::domain::models::Authorship::AgentComposed,
                send_authorization: crate::domain::models::SendAuthorization::HumanDecided,
            },
        }
    }
}

impl AttachServer {
    pub(super) async fn team_connection(
        &self,
        conn_id: u64,
    ) -> Option<(
        mpsc::Sender<DaemonFrame>,
        watch::Sender<Option<ProtocolError>>,
    )> {
        self.registry
            .lock()
            .await
            .conns
            .iter()
            .find(|conn| conn.id == conn_id)
            .map(|conn| (conn.tx.clone(), conn.terminal_tx.clone()))
    }

    pub(super) async fn send_team_frame(&self, conn_id: u64, frame: DaemonFrame) -> bool {
        let Some((tx, terminal_tx)) = self.team_connection(conn_id).await else {
            return false;
        };
        publish_team_frame_to_connection(
            &tx,
            &terminal_tx,
            frame,
            conn_id,
            #[cfg(any(test, feature = "test-instrumentation"))]
            self.team_send_test_hooks.as_ref(),
        )
    }
    /// Send a complete board as bounded fragments. `send().await` applies
    /// backpressure to this request only; it never fills the shared queue with
    /// an uncapped list or blocks send/approval workers.
    pub(super) async fn send_team_view(
        tx: mpsc::Sender<DaemonFrame>,
        terminal_tx: watch::Sender<Option<ProtocolError>>,
        _conn_id: u64,
        request: String,
        view: String,
        action: RecipientAction,
        #[cfg(any(test, feature = "test-instrumentation"))] hooks: Option<TeamSendTestHooks>,
    ) -> bool {
        let mut terminal_rx = terminal_tx.subscribe();
        let frame_base = DaemonFrame::TeamViewChunk {
            request: request.clone(),
            sequence: u32::MAX,
            view: String::new(),
        };
        // A JSON string can expand a control byte to six ASCII bytes. Account
        // for that worst case plus the exact escaped request/envelope overhead.
        let base_len = match serde_json::to_vec(&frame_base) {
            Ok(encoded) => encoded.len(),
            Err(error) => {
                tracing::warn!(%error, "daemon: could not encode board chunk envelope");
                return false;
            }
        };
        let Some(available) = (MAX_FRAME_BYTES as usize)
            .checked_sub(base_len)
            .map(|bytes| (bytes / 6).min(1024 * 1024))
        else {
            Self::refuse_team_board_stream(
                &tx,
                &mut terminal_rx,
                request,
                action,
                ProtocolError::Malformed(
                    "team board request leaves no room for a response frame".into(),
                ),
                #[cfg(any(test, feature = "test-instrumentation"))]
                _conn_id,
                #[cfg(any(test, feature = "test-instrumentation"))]
                hooks.as_ref(),
            )
            .await;
            return false;
        };
        if available < 4 {
            Self::refuse_team_board_stream(
                &tx,
                &mut terminal_rx,
                request,
                action,
                ProtocolError::Malformed(
                    "team board request leaves no room for a response frame".into(),
                ),
                #[cfg(any(test, feature = "test-instrumentation"))]
                _conn_id,
                #[cfg(any(test, feature = "test-instrumentation"))]
                hooks.as_ref(),
            )
            .await;
            return false;
        }

        let mut chunk_count = 0u32;
        let mut offset = 0;
        while offset < view.len() {
            offset = Self::next_view_chunk_end(&view, offset, available);
            let Some(next) = chunk_count.checked_add(1) else {
                Self::refuse_team_board_stream(
                    &tx,
                    &mut terminal_rx,
                    request,
                    action,
                    ProtocolError::Malformed("team board requires too many frames".into()),
                    #[cfg(any(test, feature = "test-instrumentation"))]
                    _conn_id,
                    #[cfg(any(test, feature = "test-instrumentation"))]
                    hooks.as_ref(),
                )
                .await;
                return false;
            };
            chunk_count = next;
        }
        let done = DaemonFrame::TeamViewDone {
            request: request.clone(),
            chunks: chunk_count,
        };
        match serde_json::to_vec(&done) {
            Ok(encoded) if encoded.len() <= MAX_FRAME_BYTES as usize => {}
            _ => {
                Self::refuse_team_board_stream(
                    &tx,
                    &mut terminal_rx,
                    request,
                    action,
                    ProtocolError::Malformed(
                        "team board completion frame exceeds the protocol limit".into(),
                    ),
                    #[cfg(any(test, feature = "test-instrumentation"))]
                    _conn_id,
                    #[cfg(any(test, feature = "test-instrumentation"))]
                    hooks.as_ref(),
                )
                .await;
                return false;
            }
        }

        let mut offset = 0;
        for sequence in 0..chunk_count {
            let end = Self::next_view_chunk_end(&view, offset, available);
            let chunk = DaemonFrame::TeamViewChunk {
                request: request.clone(),
                sequence,
                view: view[offset..end].to_owned(),
            };
            if !Self::send_team_frame_wait(
                &tx,
                &mut terminal_rx,
                chunk,
                #[cfg(any(test, feature = "test-instrumentation"))]
                _conn_id,
                #[cfg(any(test, feature = "test-instrumentation"))]
                hooks.as_ref(),
            )
            .await
            {
                return false;
            }
            if sequence + 1 < chunk_count
                && !Self::wait_for_team_output_drain(&tx, &mut terminal_rx).await
            {
                return false;
            }
            offset = end;
        }
        Self::send_team_frame_wait(
            &tx,
            &mut terminal_rx,
            done,
            #[cfg(any(test, feature = "test-instrumentation"))]
            _conn_id,
            #[cfg(any(test, feature = "test-instrumentation"))]
            hooks.as_ref(),
        )
        .await
    }

    pub(super) fn next_view_chunk_end(view: &str, start: usize, max_source_bytes: usize) -> usize {
        let mut end = start.saturating_add(max_source_bytes).min(view.len());
        while end > start && !view.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            view[start..]
                .char_indices()
                .nth(1)
                .map_or(view.len(), |(offset, _)| start + offset)
        } else {
            end
        }
    }
    pub(super) async fn wait_for_team_output_drain(
        tx: &mpsc::Sender<DaemonFrame>,
        terminal_rx: &mut watch::Receiver<Option<ProtocolError>>,
    ) -> bool {
        let capacity = tx.max_capacity();
        tokio::select! {
            biased;
            _ = terminal_rx.changed() => false,
            permits = tx.reserve_many(capacity) => match permits {
                Ok(permits) => {
                    drop(permits);
                    true
                }
                Err(_) => false,
            },
        }
    }

    pub(super) async fn send_team_frame_wait(
        tx: &mpsc::Sender<DaemonFrame>,
        terminal_rx: &mut watch::Receiver<Option<ProtocolError>>,
        frame: DaemonFrame,
        #[cfg(any(test, feature = "test-instrumentation"))] connection_id: u64,
        #[cfg(any(test, feature = "test-instrumentation"))] hooks: Option<&TeamSendTestHooks>,
    ) -> bool {
        if terminal_rx.borrow().is_some() {
            return false;
        }
        #[cfg(any(test, feature = "test-instrumentation"))]
        let publication_frame = hooks.map(|_| frame.clone());
        let sent = tokio::select! {
            biased;
            _ = terminal_rx.changed() => false,
            result = tx.send(frame) => result.is_ok(),
        };
        #[cfg(any(test, feature = "test-instrumentation"))]
        if sent && let (Some(hooks), Some(frame)) = (hooks, publication_frame.as_ref()) {
            hooks.record_publication(connection_id, frame);
        }
        sent
    }
    pub(super) async fn refuse_team_board_stream(
        tx: &mpsc::Sender<DaemonFrame>,
        terminal_rx: &mut watch::Receiver<Option<ProtocolError>>,
        request: String,
        action: RecipientAction,
        error: ProtocolError,
        #[cfg(any(test, feature = "test-instrumentation"))] conn_id: u64,
        #[cfg(any(test, feature = "test-instrumentation"))] hooks: Option<&TeamSendTestHooks>,
    ) {
        let _ = Self::send_team_frame_wait(
            tx,
            terminal_rx,
            DaemonFrame::RecipientActionResult {
                action,
                target: RecipientActionTarget::Request { request },
                outcome: RecipientActionOutcome::Refused { error },
            },
            #[cfg(any(test, feature = "test-instrumentation"))]
            conn_id,
            #[cfg(any(test, feature = "test-instrumentation"))]
            hooks,
        )
        .await;
    }

    pub(super) async fn refuse_team_request(
        &self,
        conn_id: u64,
        action: RecipientAction,
        request: String,
        error: ProtocolError,
    ) {
        let _ = self
            .send_team_frame(
                conn_id,
                DaemonFrame::RecipientActionResult {
                    action,
                    target: RecipientActionTarget::Request { request },
                    outcome: RecipientActionOutcome::Refused { error },
                },
            )
            .await;
    }

    pub(super) fn team_request_error(
        tier: ConnectionTier,
        mode: AttachMode,
        write_required: bool,
    ) -> Option<ProtocolError> {
        if tier != ConnectionTier::TrustedLocal {
            Some(ProtocolError::PeerVerification(
                "team commands are trusted-local only".to_owned(),
            ))
        } else if write_required && mode != AttachMode::ReadWrite {
            Some(ProtocolError::ReadOnly)
        } else {
            None
        }
    }

    pub(super) async fn handle_team_board(
        &self,
        conn_id: u64,
        tier: ConnectionTier,
        mode: AttachMode,
        request: String,
        selector: protocol::TeamBoardRequest,
    ) {
        let action = RecipientAction::TeamBoard;
        if let Some(error) = Self::team_request_error(tier, mode, false) {
            self.refuse_team_request(conn_id, action, request, error)
                .await;
            return;
        }
        if request.len() > MAX_TEAM_REQUEST_BYTES {
            self.refuse_team_request(
                conn_id,
                action,
                request,
                ProtocolError::Malformed(format!(
                    "team board request key exceeds {MAX_TEAM_REQUEST_BYTES} bytes"
                )),
            )
            .await;
            return;
        }
        #[cfg(feature = "a2a")]
        if let Some(runtime) = &self.a2a_runtime {
            #[cfg(any(test, feature = "test-instrumentation"))]
            let now = self
                .team_send_test_hooks
                .as_ref()
                .and_then(TeamSendTestHooks::board_now)
                .unwrap_or_else(std::time::Instant::now);
            #[cfg(not(any(test, feature = "test-instrumentation")))]
            let now = std::time::Instant::now();
            let view =
                crate::infrastructure::runtime::transparency_bridge::team_board_message_for_request_at(
                    runtime, selector, now,
                )
                .await;
            let Some((tx, terminal_tx)) = self.team_connection(conn_id).await else {
                return;
            };
            #[cfg(any(test, feature = "test-instrumentation"))]
            let hooks = self.team_send_test_hooks.clone();
            tokio::spawn(async move {
                let _ = Self::send_team_view(
                    tx,
                    terminal_tx,
                    conn_id,
                    request,
                    view,
                    action,
                    #[cfg(any(test, feature = "test-instrumentation"))]
                    hooks,
                )
                .await;
            });
            return;
        }
        #[cfg(not(feature = "a2a"))]
        let _ = selector;
        self.refuse_team_request(
            conn_id,
            action,
            request,
            ProtocolError::Internal("A2A is not enabled in this build".to_owned()),
        )
        .await;
    }

    pub(super) async fn handle_team_status(
        &self,
        conn_id: u64,
        tier: ConnectionTier,
        mode: AttachMode,
        request: String,
    ) {
        let action = RecipientAction::TeamStatus;
        if let Some(error) = Self::team_request_error(tier, mode, false) {
            self.refuse_team_request(conn_id, action, request, error)
                .await;
            return;
        }
        if !self.team_request_bound(conn_id, action, &request).await {
            return;
        }
        #[cfg(not(feature = "a2a"))]
        {
            self.refuse_team_request(
                conn_id,
                action,
                request,
                ProtocolError::Internal("A2A is not enabled in this build".to_owned()),
            )
            .await;
        }
        #[cfg(feature = "a2a")]
        {
            let Some(manager) = &self.pending_consent else {
                self.refuse_team_request(
                    conn_id,
                    action,
                    request,
                    ProtocolError::Internal("daemon consent projection is unavailable".to_owned()),
                )
                .await;
                return;
            };
            match manager.render_team_status(&self.team_peers).await {
                Ok(message) => {
                    let _ = self
                        .send_team_frame(
                            conn_id,
                            DaemonFrame::TeamStatusResult { request, message },
                        )
                        .await;
                }
                Err(error) => {
                    self.refuse_team_request(
                        conn_id,
                        action,
                        request,
                        ProtocolError::Internal(error),
                    )
                    .await;
                }
            }
        }
    }
    pub(super) async fn handle_team_revoke(
        &self,
        conn_id: u64,
        tier: ConnectionTier,
        mode: AttachMode,
        request: String,
        alias: String,
    ) {
        let action = RecipientAction::RevokeSenderConsent;
        if let Some(error) = Self::team_request_error(tier, mode, true) {
            self.refuse_team_request(conn_id, action, request, error)
                .await;
            return;
        }
        if !self.team_request_bound(conn_id, action, &request).await {
            return;
        }
        #[cfg(not(feature = "a2a"))]
        {
            let _ = alias;
            self.refuse_team_request(
                conn_id,
                action,
                request,
                ProtocolError::Internal("A2A is not enabled in this build".to_owned()),
            )
            .await;
        }
        #[cfg(feature = "a2a")]
        {
            let Some(manager) = &self.pending_consent else {
                self.refuse_team_request(
                    conn_id,
                    action,
                    request,
                    ProtocolError::Internal("daemon consent projection is unavailable".to_owned()),
                )
                .await;
                return;
            };
            match manager
                .revoke_sender_consent(&alias, &self.team_peers)
                .await
            {
                Ok(message) => {
                    let _ = self
                        .send_team_frame(
                            conn_id,
                            DaemonFrame::SenderConsentRevoked {
                                request,
                                alias,
                                message,
                            },
                        )
                        .await;
                }
                Err(error) => {
                    self.refuse_team_request(
                        conn_id,
                        action,
                        request,
                        ProtocolError::Internal(error),
                    )
                    .await;
                }
            }
        }
    }

    pub(super) async fn close_team_proposal(
        &self,
        conn_id: u64,
        proposal: String,
        outcome: protocol::TeamProposalCloseOutcome,
    ) {
        if let Some((tx, terminal)) = self.team_connection(conn_id).await {
            publish_team_frame(
                &tx,
                &terminal,
                DaemonFrame::TeamProposalClosed { proposal, outcome },
            );
        }
    }

    pub(super) async fn refuse_team_send(
        &self,
        conn_id: u64,
        request: String,
        error: ProtocolError,
        proposed: bool,
    ) {
        if proposed {
            self.close_team_proposal(
                conn_id,
                request,
                protocol::TeamProposalCloseOutcome::Refused {
                    reason: error.to_string(),
                },
            )
            .await;
        } else {
            self.refuse_team_request(conn_id, RecipientAction::TeamSend, request, error)
                .await;
        }
    }

    pub(super) async fn handle_team_send(
        &self,
        conn_id: u64,
        tier: ConnectionTier,
        mode: AttachMode,
        request: String,
        payload: Option<TeamSendPayload>,
        connection: Option<(
            mpsc::Sender<DaemonFrame>,
            watch::Sender<Option<ProtocolError>>,
        )>,
    ) {
        let proposed = payload.is_none();
        if let Some(error) = Self::team_request_error(tier, mode, true) {
            self.refuse_team_send(conn_id, request, error, proposed)
                .await;
            return;
        }
        if !proposed
            && !self
                .team_request_bound(conn_id, RecipientAction::TeamSend, &request)
                .await
        {
            return;
        }
        #[cfg(feature = "a2a")]
        {
            let Some(runtime) = self.a2a_runtime.clone() else {
                self.refuse_team_send(
                    conn_id,
                    request,
                    ProtocolError::Internal("A2A is not enabled in this build".to_owned()),
                    proposed,
                )
                .await;
                return;
            };
            let (tx, terminal_tx) = if let Some(connection) = connection {
                connection
            } else if let Some(connection) = self.team_connection(conn_id).await {
                connection
            } else {
                return;
            };
            use team_proposal::ProposalTakeError;
            let admitted = match payload {
                Some(payload) => self
                    .team_send_slots
                    .clone()
                    .try_acquire_owned()
                    .map(|permit| (payload, permit))
                    .map_err(|_| ProposalTakeError::Busy),
                None => self
                    .team_proposals
                    .take_for_send(&request, self.team_send_slots.clone())
                    .await
                    .map(|(proposal, permit)| (TeamSendPayload::Agent(proposal), permit)),
            };
            let (payload, permit) = match admitted {
                Ok(admitted) => admitted,
                Err(ProposalTakeError::Unknown) => {
                    self.close_team_proposal(
                        conn_id,
                        request,
                        protocol::TeamProposalCloseOutcome::Refused {
                            reason: "That proposal is no longer waiting. Nothing was sent."
                                .to_owned(),
                        },
                    )
                    .await;
                    return;
                }
                Err(ProposalTakeError::Busy) => {
                    self.team_send_saturation_refusals
                        .fetch_add(1, Ordering::AcqRel);
                    if proposed {
                        self.close_team_proposal(conn_id, request.clone(),
                            protocol::TeamProposalCloseOutcome::Refused {
                                reason: "Not sent yet — this daemon is busy sending. The proposal is still waiting.".to_owned(),
                            }).await;
                        self.team_proposals.reemit(&request, conn_id).await;
                    } else {
                        self.refuse_team_send(
                            conn_id,
                            request,
                            ProtocolError::Internal(
                                "daemon team-send limit reached; try again later".to_owned(),
                            ),
                            false,
                        )
                        .await;
                    }
                    return;
                }
            };
            let send_request = payload.request();
            if let Err(error) =
                crate::adapters::a2a::send::validate_send_request(&runtime, &send_request)
            {
                drop(permit);
                self.refuse_team_send(
                    conn_id,
                    request,
                    ProtocolError::Malformed(error.to_string()),
                    proposed,
                )
                .await;
                return;
            }
            if let Err(error) =
                crate::adapters::a2a::send::validate_story_holders(&runtime, &send_request).await
            {
                drop(permit);
                // A displaced holder is the client's request to fix; an
                // unreadable story history is this daemon's own failure.
                let error = match error {
                    crate::adapters::a2a::send::SendError::StoryHistory(_) => {
                        ProtocolError::Internal(error.to_string())
                    }
                    _ => ProtocolError::Malformed(error.to_string()),
                };
                self.refuse_team_send(conn_id, request, error, proposed)
                    .await;
                return;
            }
            let mut tasks = self.team_send_tasks.lock().await;
            if !self.team_send_accepting.load(Ordering::Acquire) {
                drop(tasks);
                drop(permit);
                self.refuse_team_send(
                    conn_id,
                    request,
                    ProtocolError::Internal("daemon is shutting down".to_owned()),
                    proposed,
                )
                .await;
                return;
            }
            let cancel = self.team_send_shutdown.child_token();
            let started_tx = tx.clone();
            let started_terminal = terminal_tx.clone();
            let settled_tx = tx;
            let settled_terminal = terminal_tx;
            let started_request = request.clone();
            let settled_request = request;
            let started_count = self.team_send_started_count.clone();
            let settled_count = self.team_send_settled_count.clone();
            #[cfg(any(test, feature = "test-instrumentation"))]
            let hooks = self.team_send_test_hooks.clone();
            #[cfg(any(test, feature = "test-instrumentation"))]
            let started_hooks = hooks.clone();
            #[cfg(any(test, feature = "test-instrumentation"))]
            let send_action_hooks = hooks.clone();
            #[cfg(any(test, feature = "test-instrumentation"))]
            let settled_hooks = hooks.clone();
            #[cfg(any(test, feature = "test-instrumentation"))]
            let completion_hooks = hooks;
            #[cfg(any(test, feature = "test-instrumentation"))]
            let completion_request = settled_request.clone();
            #[cfg(any(test, feature = "test-instrumentation"))]
            let cancel_for_completion = cancel.clone();
            let started_conn_id = conn_id;
            let settled_conn_id = conn_id;
            let task = tokio::spawn(async move {
                #[cfg(any(test, feature = "test-instrumentation"))]
                let send_permit = permit;
                #[cfg(not(any(test, feature = "test-instrumentation")))]
                let _send_permit = permit;
                #[cfg(any(test, feature = "test-instrumentation"))]
                if let Some(hooks) = send_action_hooks {
                    hooks.before_send_action().await;
                }
                let _ = crate::adapters::a2a::send::coordinate_send(
                    &runtime,
                    payload.request(),
                    cancel,
                    |action| {
                        started_count.fetch_add(1, Ordering::AcqRel);
                        let _ = publish_team_frame_to_connection(
                            &started_tx,
                            &started_terminal,
                            DaemonFrame::TeamSendStarted {
                                request: started_request.clone(),
                                action: action.clone(),
                            },
                            started_conn_id,
                            #[cfg(any(test, feature = "test-instrumentation"))]
                            started_hooks.as_ref(),
                        );
                    },
                    |index, outcome| {
                        settled_count.fetch_add(1, Ordering::AcqRel);
                        let _ = publish_team_frame_to_connection(
                            &settled_tx,
                            &settled_terminal,
                            DaemonFrame::TeamSendSettled {
                                request: settled_request.clone(),
                                index,
                                outcome: protocol::TeamSendOutcome::from(&outcome),
                            },
                            settled_conn_id,
                            #[cfg(any(test, feature = "test-instrumentation"))]
                            settled_hooks.as_ref(),
                        );
                    },
                )
                .await;
                #[cfg(any(test, feature = "test-instrumentation"))]
                {
                    drop(send_permit);
                    if let Some(hooks) = completion_hooks {
                        let kind = if cancel_for_completion.is_cancelled() {
                            TeamSendCompletionKind::Cancelled
                        } else {
                            TeamSendCompletionKind::Finished
                        };
                        hooks.record_completion(completion_request, kind);
                    }
                }
                #[cfg(not(any(test, feature = "test-instrumentation")))]
                drop(_send_permit);
            });
            tasks.retain(|task| !task.is_finished());
            tasks.push(task);
        }
        #[cfg(not(feature = "a2a"))]
        {
            let _ = connection;
            self.refuse_team_send(
                conn_id,
                request,
                ProtocolError::Internal("A2A is not enabled in this build".to_owned()),
                proposed,
            )
            .await;
            if let Some(TeamSendPayload::Human {
                recipients,
                text,
                message_type,
                subject,
                stories,
                reply_to,
            }) = payload
            {
                let _ = (recipients, text, message_type, subject, stories, reply_to);
            }
        }
    }
}

impl AttachServer {
    /// Story 19.21c: a read never borrows a send permit.
    pub(super) async fn handle_team_thread(
        &self,
        conn_id: u64,
        tier: ConnectionTier,
        mode: AttachMode,
        request: String,
        peer: String,
        item_id: String,
    ) {
        let action = RecipientAction::TeamThread;
        if let Some(error) = Self::team_request_error(tier, mode, false) {
            self.refuse_team_request(conn_id, action, request, error)
                .await;
            return;
        }
        if !self.team_request_bound(conn_id, action, &request).await {
            return;
        }
        #[cfg(feature = "a2a")]
        if let Some(runtime) = self.a2a_runtime.clone() {
            let Some((tx, terminal)) = self.team_connection(conn_id).await else {
                return;
            };
            let mut tasks = self.team_send_tasks.lock().await;
            if !self.team_send_accepting.load(Ordering::Acquire) {
                drop(tasks);
                self.refuse_team_request(
                    conn_id,
                    action,
                    request,
                    ProtocolError::Internal("daemon is shutting down".into()),
                )
                .await;
                return;
            }
            let cancel = self.team_send_shutdown.child_token();
            #[cfg(any(test, feature = "test-instrumentation"))]
            let hooks = self.team_send_test_hooks.clone();
            tasks.retain(|task| !task.is_finished());
            tasks.push(tokio::spawn(async move {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {},
                    _ = async {
                        let view = crate::infrastructure::runtime::transparency_bridge::team_thread_message(
                            &runtime, &peer, &item_id).await;
                        Self::send_team_view(tx, terminal, conn_id, request, view, action,
                            #[cfg(any(test, feature = "test-instrumentation"))]
                            hooks).await;
                    } => {}
                }
            }));
            return;
        }
        #[cfg(not(feature = "a2a"))]
        let _ = (peer, item_id);
        self.refuse_team_request(
            conn_id,
            action,
            request,
            ProtocolError::Internal("A2A is not enabled in this build".into()),
        )
        .await;
    }

    async fn team_request_bound(
        &self,
        conn_id: u64,
        action: RecipientAction,
        request: &str,
    ) -> bool {
        if request.len() <= MAX_TEAM_REQUEST_BYTES {
            return true;
        }
        self.refuse_team_request(
            conn_id,
            action,
            request.to_owned(),
            ProtocolError::Malformed(format!(
                "team request key exceeds {MAX_TEAM_REQUEST_BYTES} bytes"
            )),
        )
        .await;
        false
    }
}

/// Story 19.21c: the client answers a request, never supplies a task.
#[cfg(feature = "a2a")]
pub(super) enum RetractSlot {
    /// `admission` is unique per preview, so a late completion of an earlier
    /// preview that reused this request key can never upgrade the slot.
    Reading { request: String, admission: u64 },
    Ready {
        request: String,
        peer: String,
        item_id: String,
        task: Option<String>,
        armed: bool,
    },
}
#[cfg(feature = "a2a")]
impl RetractSlot {
    fn request(&self) -> &str {
        match self {
            Self::Reading { request, .. } | Self::Ready { request, .. } => request,
        }
    }
}
#[cfg(feature = "a2a")]
static RETRACT_ADMISSIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(all(feature = "a2a", any(test, feature = "test-instrumentation")))]
impl AttachServer {
    /// Story 19.21c test seam: live retract slots across every connection.
    pub async fn team_retract_slot_count(&self) -> usize {
        self.team_retracts.lock().await.len()
    }
}
const RETRACT_NOT_AWAITING: &str =
    "That retract is no longer awaiting your answer. Nothing was marked.";

impl AttachServer {
    pub(super) async fn handle_team_retract_preview(
        &self,
        conn_id: u64,
        tier: ConnectionTier,
        mode: AttachMode,
        request: String,
        peer: String,
        item_id: String,
    ) {
        let action = RecipientAction::TeamRetract;
        if let Some(error) = Self::team_request_error(tier, mode, true) {
            self.refuse_team_request(conn_id, action, request, error)
                .await;
            return;
        }
        if !self.team_request_bound(conn_id, action, &request).await {
            return;
        }
        #[cfg(feature = "a2a")]
        if let Some(runtime) = self.a2a_runtime.clone() {
            let mut tasks = self.team_send_tasks.lock().await;
            if !self.team_send_accepting.load(Ordering::Acquire) {
                drop(tasks);
                self.refuse_team_request(
                    conn_id,
                    action,
                    request,
                    ProtocolError::Internal("daemon is shutting down".into()),
                )
                .await;
                return;
            }
            // Paired locks always use registry -> slots, including cleanup.
            let registry = self.registry.lock().await;
            if !registry.conns.iter().any(|conn| conn.id == conn_id) {
                return;
            }
            let admission = RETRACT_ADMISSIONS.fetch_add(1, Ordering::Relaxed);
            self.team_retracts.lock().await.insert(
                conn_id,
                RetractSlot::Reading {
                    request: request.clone(),
                    admission,
                },
            );
            drop(registry);
            let registry = self.registry.clone();
            let slots = self.team_retracts.clone();
            let cancel = self.team_send_shutdown.child_token();
            #[cfg(any(test, feature = "test-instrumentation"))]
            let hooks = self.team_send_test_hooks.clone();
            tasks.retain(|task| !task.is_finished());
            tasks.push(tokio::spawn(async move {
                #[cfg(any(test, feature = "test-instrumentation"))]
                let completion_request = request.clone();
                let work = async {
                    let preview = crate::infrastructure::runtime::transparency_bridge::team_retract_preview(
                        &runtime, &peer, &item_id).await;
                    #[cfg(any(test, feature = "test-instrumentation"))]
                    if let Some(hooks) = &hooks { hooks.before_retract_preview(&request).await; }
                    let registry = registry.lock().await;
                    let Some(conn) = registry.conns.iter().find(|conn| conn.id == conn_id) else { return };
                    let mut slots = slots.lock().await;
                    if !matches!(slots.get(&conn_id), Some(RetractSlot::Reading { request: key, admission: current }) if key == &request && *current == admission) {
                        return;
                    }
                    let preview = match preview {
                        crate::domain::events::TeamRetractPreview::Card(card) => {
                            slots.insert(conn_id, RetractSlot::Ready {
                                request: request.clone(), peer: card.peer.clone(),
                                item_id: card.item_id.clone(), task: card.task, armed: card.armed,
                            });
                            protocol::TeamRetractPreviewView::Card {
                                peer: card.peer, item_id: card.item_id, body: card.body, armed: card.armed,
                            }
                        }
                        crate::domain::events::TeamRetractPreview::Answer(message) => {
                            slots.remove(&conn_id);
                            protocol::TeamRetractPreviewView::Answer { message }
                        }
                    };
                    publish_team_frame_to_connection(&conn.tx, &conn.terminal_tx,
                        DaemonFrame::TeamRetractPreviewed { request, preview }, conn_id,
                        #[cfg(any(test, feature = "test-instrumentation"))]
                        hooks.as_ref());
                };
                tokio::select! { biased; _ = cancel.cancelled() => {}, _ = work => {} }
                #[cfg(any(test, feature = "test-instrumentation"))]
                if let Some(hooks) = &hooks {
                    hooks.record_completion(completion_request, if cancel.is_cancelled() {
                        TeamSendCompletionKind::Cancelled
                    } else { TeamSendCompletionKind::Finished });
                }
            }));
            return;
        }
        #[cfg(not(feature = "a2a"))]
        let _ = (peer, item_id);
        self.refuse_team_request(
            conn_id,
            action,
            request,
            ProtocolError::Internal("A2A is not enabled in this build".into()),
        )
        .await;
    }

    pub(super) async fn handle_team_retract_decision(
        &self,
        conn_id: u64,
        tier: ConnectionTier,
        mode: AttachMode,
        request: String,
        confirm: bool,
    ) {
        let action = RecipientAction::TeamRetract;
        if let Some(error) = Self::team_request_error(tier, mode, true) {
            self.refuse_team_request(conn_id, action, request, error)
                .await;
            return;
        }
        if !self.team_request_bound(conn_id, action, &request).await {
            return;
        }
        // A writer grant moves `last_writer_conn_id` and clears every slot under
        // this lock. A detached writer's queued answer still resolves its own
        // target (OR-2); its slot is invalidated once its FIFO drains.
        let registry = self.registry.lock().await;
        if self.last_writer_conn_id.load(Ordering::SeqCst) != conn_id {
            drop(registry);
            self.refuse_team_request(
                conn_id,
                action,
                request,
                ProtocolError::Malformed(RETRACT_NOT_AWAITING.into()),
            )
            .await;
            return;
        }
        #[cfg(feature = "a2a")]
        if let Some(runtime) = self.a2a_runtime.clone() {
            let mut slots = self.team_retracts.lock().await;
            // Compare-and-take in one lock; a stale decision preserves a newer preview.
            let slot = if slots
                .get(&conn_id)
                .is_some_and(|slot| slot.request() == request)
            {
                slots.remove(&conn_id)
            } else {
                None
            };
            drop(slots);
            drop(registry);
            let Some(RetractSlot::Ready {
                peer,
                item_id,
                task,
                armed,
                ..
            }) = slot
            else {
                self.refuse_team_request(
                    conn_id,
                    action,
                    request,
                    ProtocolError::Malformed(RETRACT_NOT_AWAITING.into()),
                )
                .await;
                return;
            };
            if !confirm {
                return;
            }
            if !armed {
                self.refuse_team_request(
                    conn_id,
                    action,
                    request,
                    ProtocolError::Malformed(
                        "That retract is not armed. Nothing was marked.".into(),
                    ),
                )
                .await;
                return;
            }
            let Ok(permit) = self.team_send_slots.clone().try_acquire_owned() else {
                self.refuse_team_request(
                    conn_id,
                    action,
                    request,
                    ProtocolError::Internal(
                        "daemon team-send limit reached; try again later".into(),
                    ),
                )
                .await;
                return;
            };
            let connection = self.team_connection(conn_id).await;
            let mut tasks = self.team_send_tasks.lock().await;
            if !self.team_send_accepting.load(Ordering::Acquire) {
                drop(tasks);
                self.refuse_team_request(
                    conn_id,
                    action,
                    request,
                    ProtocolError::Internal("daemon is shutting down".into()),
                )
                .await;
                return;
            }
            let cancel = self.team_send_shutdown.child_token();
            #[cfg(any(test, feature = "test-instrumentation"))]
            let hooks = self.team_send_test_hooks.clone();
            tasks.retain(|task| !task.is_finished());
            tasks.push(tokio::spawn(async move {
                #[cfg(any(test, feature = "test-instrumentation"))]
                let completion_request = request.clone();
                let work = async {
                    // A confirmed daemon-owned dispatch survives detach.
                    let answer =
                        crate::infrastructure::runtime::transparency_bridge::team_retract_dispatch(
                            &runtime, &peer, &item_id, task,
                        )
                        .await;
                    // D5: the permit bounds the dispatch, not the origin's output drain.
                    drop(permit);
                    let Some((tx, terminal)) = connection else {
                        return;
                    };
                    if let Some(board) = answer.board
                        && !Self::send_team_view(
                            tx.clone(),
                            terminal.clone(),
                            conn_id,
                            request.clone(),
                            board,
                            action,
                            #[cfg(any(test, feature = "test-instrumentation"))]
                            hooks.clone(),
                        )
                        .await
                    {
                        return;
                    }
                    // The answer closes the same ordered, backpressured stream.
                    let mut terminal_rx = terminal.subscribe();
                    Self::send_team_frame_wait(
                        &tx,
                        &mut terminal_rx,
                        DaemonFrame::TeamRetractAnswered {
                            request,
                            message: answer.message,
                        },
                        #[cfg(any(test, feature = "test-instrumentation"))]
                        conn_id,
                        #[cfg(any(test, feature = "test-instrumentation"))]
                        hooks.as_ref(),
                    )
                    .await;
                };
                tokio::select! { biased; _ = cancel.cancelled() => {}, _ = work => {} }
                #[cfg(any(test, feature = "test-instrumentation"))]
                if let Some(hooks) = &hooks {
                    hooks.record_completion(
                        completion_request,
                        if cancel.is_cancelled() {
                            TeamSendCompletionKind::Cancelled
                        } else {
                            TeamSendCompletionKind::Finished
                        },
                    );
                }
            }));
            return;
        }
        drop(registry);
        #[cfg(not(feature = "a2a"))]
        let _ = confirm;
        self.refuse_team_request(
            conn_id,
            action,
            request,
            ProtocolError::Internal("A2A is not enabled in this build".into()),
        )
        .await;
    }
}
