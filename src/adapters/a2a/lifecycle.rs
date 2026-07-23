//! A2A task lifecycle driver: `message/send` → `tasks/get` poll loop → terminal.
//!
//! Ruling 3 (Story 17.4b): polling is the **mainline** path — `capabilities.streaming`
//! is `false` on 80% of the measured population. `tasks/get` polling is
//! spec-sanctioned ("the client then periodically polls by calling `tasks/get`
//! until the task reaches a terminal state"), works for the whole population, and
//! is deterministically testable without SSE framing. `message/stream` +
//! `tasks/resubscribe` are OUT OF SCOPE (see DF-17-4b-STREAMING in deferred-work).
//!
//! R-C: `input-required` / `auth-required` is NOT a resumable local state — there
//! is no mechanism for a human to answer a waiting node. The driver issues a real
//! `tasks/cancel` to the peer (never leak an open task onto their host) and hands
//! the caller an [`LifecycleOutcome::InputRequired`] carrying `taskId`/`contextId`
//! for the node layer to journal + terminate as a named `Failed`.

use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::domain::models::RapTaskState;

use super::error::A2aError;
use super::task::A2aTaskState;

/// Snapshot of a remote A2A task, parsed from a `message/send` or `tasks/get`
/// result value.
#[derive(Debug, Clone)]
pub struct TaskSnapshot {
    pub id: String,
    pub context_id: Option<String>,
    pub state: A2aTaskState,
    /// The full task result value (status.message, artifacts, history).
    pub result: serde_json::Value,
}

impl TaskSnapshot {
    /// Parse a JSON-RPC `result` value into a task snapshot. A message-shaped
    /// result (a fast agent answering without a task) is treated as an immediate
    /// completion. Any other shape without a `status.state` is a typed refusal.
    pub fn from_result(value: serde_json::Value) -> Result<Self, A2aError> {
        if let Some(state_str) = value
            .get("status")
            .and_then(|status| status.get("state"))
            .and_then(|state| state.as_str())
        {
            let state = A2aTaskState::from_wire(state_str)?;
            let id = value
                .get("id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| A2aError::MalformedResponse {
                    reason: "task result has no non-empty id".to_owned(),
                })?;
            let context_id = value
                .get("contextId")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            return Ok(Self {
                id,
                context_id,
                state,
                result: value,
            });
        }

        // Message-shaped response: agent answered immediately, no task to poll.
        if value.get("kind").and_then(serde_json::Value::as_str) == Some("message")
            || value.get("parts").is_some()
        {
            let id = value
                .get("taskId")
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.is_empty())
                .or_else(|| {
                    value
                        .get("messageId")
                        .and_then(serde_json::Value::as_str)
                        .filter(|id| !id.is_empty())
                })
                .unwrap_or("message")
                .to_owned();
            let context_id = value
                .get("contextId")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            return Ok(Self {
                id,
                context_id,
                state: A2aTaskState::Completed,
                result: value,
            });
        }

        Err(A2aError::MalformedResponse {
            reason: "result is neither a task (status.state) nor a message".to_owned(),
        })
    }
}

/// Transport seam the lifecycle driver runs through. Implemented by the live
/// `A2aClientAdapter` (via [`super::driver::TaskClient`]) and by scripted mocks /
/// wiremock cassettes in tests.
#[async_trait]
pub trait A2aTaskTransport: Send + Sync {
    async fn message_send(&self, message: serde_json::Value)
    -> Result<serde_json::Value, A2aError>;
    async fn tasks_get(&self, task_id: &str) -> Result<serde_json::Value, A2aError>;
    async fn tasks_cancel(&self, task_id: &str) -> Result<serde_json::Value, A2aError>;
}

/// Bounded poll configuration. `max_status_updates` mirrors the domain
/// `MAILBOX_CAP` (64) so an inbound-status flood cannot run unbounded.
#[derive(Debug, Clone)]
pub struct PollConfig {
    pub interval: Duration,
    pub deadline: Duration,
    pub max_status_updates: usize,
}

impl Default for PollConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(500),
            deadline: Duration::from_secs(300),
            max_status_updates: crate::infrastructure::subagent::node_tree::MAILBOX_CAP,
        }
    }
}

/// The result of driving a task to a decision point.
#[derive(Debug, Clone)]
pub enum LifecycleOutcome {
    /// The task reached a terminal RAP state (`Completed`/`Failed`/`Canceled`/`Rejected`).
    Terminal {
        state: RapTaskState,
        task: TaskSnapshot,
    },
    /// The peer asked for input/auth. The driver already sent `tasks/cancel`; the
    /// node layer journals `taskId`/`contextId` and terminates as a named `Failed`.
    InputRequired { task: TaskSnapshot },
}

/// Drive a delegated task from dispatch to a decision point via `message/send`
/// then `tasks/get` polling. `on_transition` fires on every legal RAP state hop
/// (the node layer projects these onto `NodeState`). Cancellation (owner kill)
/// issues a real `tasks/cancel` to the peer and returns `Canceled`.
pub async fn drive_task<T>(
    transport: &T,
    message: serde_json::Value,
    config: &PollConfig,
    cancel: &CancellationToken,
    on_transition: impl FnMut(RapTaskState),
) -> Result<LifecycleOutcome, A2aError>
where
    T: A2aTaskTransport + ?Sized,
{
    let snapshot = TaskSnapshot::from_result(transport.message_send(message).await?)?;
    poll_from_snapshot(transport, snapshot, config, cancel, on_transition).await
}

/// Drive an ALREADY-DISPATCHED task to terminal, starting from a known snapshot.
/// Used both by [`drive_task`] (after `message/send`) and by restart
/// reconciliation (after a fresh `tasks/get` on a recovered task id) — the poll
/// loop is identical, only the seed differs (Ruling 4 / AC5).
pub async fn poll_from_snapshot<T>(
    transport: &T,
    snapshot: TaskSnapshot,
    config: &PollConfig,
    cancel: &CancellationToken,
    mut on_transition: impl FnMut(RapTaskState),
) -> Result<LifecycleOutcome, A2aError>
where
    T: A2aTaskTransport + ?Sized,
{
    let mut current: Option<RapTaskState> = None;
    if let Some(outcome) = step(transport, &snapshot, &mut current, &mut on_transition).await? {
        return Ok(outcome);
    }

    let started = tokio::time::Instant::now();
    for _ in 0..config.max_status_updates {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                transport.tasks_cancel(&snapshot.id).await?;
                on_transition(RapTaskState::Canceled);
                return Ok(LifecycleOutcome::Terminal {
                    state: RapTaskState::Canceled,
                    task: snapshot,
                });
            }
            () = tokio::time::sleep(config.interval) => {}
        }
        if started.elapsed() >= config.deadline {
            return Err(A2aError::MalformedResponse {
                reason: format!(
                    "task {} did not reach a terminal state within the poll deadline",
                    snapshot.id
                ),
            });
        }
        let polled = TaskSnapshot::from_result(transport.tasks_get(&snapshot.id).await?)?;
        if let Some(outcome) = step(transport, &polled, &mut current, &mut on_transition).await? {
            return Ok(outcome);
        }
    }
    Err(A2aError::MalformedResponse {
        reason: format!(
            "task {} exceeded the {} status-update budget without terminating",
            snapshot.id, config.max_status_updates
        ),
    })
}

/// Apply one snapshot to the RAP FSM. Returns `Some(outcome)` when a decision
/// point is reached (terminal or input-required), `None` to keep polling.
async fn step<T>(
    transport: &T,
    snapshot: &TaskSnapshot,
    current: &mut Option<RapTaskState>,
    on_transition: &mut impl FnMut(RapTaskState),
) -> Result<Option<LifecycleOutcome>, A2aError>
where
    T: A2aTaskTransport + ?Sized,
{
    // `unknown` holds current state and keeps polling (Ruling 4).
    let Some(next) = snapshot.state.to_rap() else {
        return Ok(None);
    };

    match current {
        None => {
            *current = Some(next);
            on_transition(next);
        }
        Some(prev) if *prev != next => {
            prev.transition_or_err(next)
                .map_err(|error| A2aError::MalformedResponse {
                    reason: format!("peer reported an illegal task-state sequence: {error}"),
                })?;
            on_transition(next);
        }
        Some(_) => {}
    }

    match next {
        RapTaskState::InputRequired | RapTaskState::AuthRequired => {
            // R-C: close the peer's task before we terminate locally. A failed
            // cancel is a transport failure, never a successful local refusal.
            transport.tasks_cancel(&snapshot.id).await?;
            Ok(Some(LifecycleOutcome::InputRequired {
                task: snapshot.clone(),
            }))
        }
        state if state.is_terminal() => Ok(Some(LifecycleOutcome::Terminal {
            state,
            task: snapshot.clone(),
        })),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;

    use super::*;

    /// Scripted transport: `message/send` returns the first script entry, each
    /// `tasks/get` returns the next. Records `tasks/cancel` calls.
    struct ScriptedTransport {
        script: Mutex<std::collections::VecDeque<serde_json::Value>>,
        cancels: Mutex<Vec<String>>,
        cancel_error: bool,
    }

    impl ScriptedTransport {
        fn new(states: &[&str]) -> Self {
            let script = states
                .iter()
                .map(|state| {
                    serde_json::json!({
                        "kind": "task",
                        "id": "task-1",
                        "contextId": "ctx-1",
                        "status": {"state": state}
                    })
                })
                .collect();
            Self {
                script: Mutex::new(script),
                cancels: Mutex::new(Vec::new()),
                cancel_error: false,
            }
        }
        fn with_cancel_error(mut self) -> Self {
            self.cancel_error = true;
            self
        }
        fn next(&self) -> serde_json::Value {
            self.script.lock().pop_front().expect("script exhausted")
        }
    }

    #[async_trait]
    impl A2aTaskTransport for ScriptedTransport {
        async fn message_send(
            &self,
            _message: serde_json::Value,
        ) -> Result<serde_json::Value, A2aError> {
            Ok(self.next())
        }
        async fn tasks_get(&self, _task_id: &str) -> Result<serde_json::Value, A2aError> {
            Ok(self.next())
        }
        async fn tasks_cancel(&self, task_id: &str) -> Result<serde_json::Value, A2aError> {
            self.cancels.lock().push(task_id.to_owned());
            if self.cancel_error {
                return Err(A2aError::Request("cancel failed".to_owned()));
            }
            Ok(serde_json::json!({"kind":"task","id":task_id,"status":{"state":"canceled"}}))
        }
    }

    fn fast_config() -> PollConfig {
        PollConfig {
            interval: Duration::from_millis(1),
            deadline: Duration::from_secs(5),
            max_status_updates: 64,
        }
    }
    #[tokio::test]
    async fn drives_submitted_working_completed_to_terminal() {
        let transport = ScriptedTransport::new(&["submitted", "working", "completed"]);
        let mut seen = Vec::new();
        let outcome = drive_task(
            &transport,
            serde_json::json!({}),
            &fast_config(),
            &CancellationToken::new(),
            |state| seen.push(state),
        )
        .await
        .expect("lifecycle completes");
        assert!(matches!(
            outcome,
            LifecycleOutcome::Terminal {
                state: RapTaskState::Completed,
                ..
            }
        ));
        assert_eq!(
            seen,
            vec![
                RapTaskState::Submitted,
                RapTaskState::Working,
                RapTaskState::Completed
            ]
        );
    }

    #[tokio::test]
    async fn unknown_state_holds_and_keeps_polling() {
        let transport = ScriptedTransport::new(&["working", "unknown", "completed"]);
        let mut seen = Vec::new();
        let outcome = drive_task(
            &transport,
            serde_json::json!({}),
            &fast_config(),
            &CancellationToken::new(),
            |state| seen.push(state),
        )
        .await
        .expect("unknown is held, not fatal");
        assert!(matches!(
            outcome,
            LifecycleOutcome::Terminal {
                state: RapTaskState::Completed,
                ..
            }
        ));
        // `unknown` produced no transition.
        assert_eq!(seen, vec![RapTaskState::Working, RapTaskState::Completed]);
    }

    #[tokio::test]
    async fn input_required_cancels_the_peer_and_returns_input_required() {
        let transport = ScriptedTransport::new(&["working", "input-required"]);
        let outcome = drive_task(
            &transport,
            serde_json::json!({}),
            &fast_config(),
            &CancellationToken::new(),
            |_| {},
        )
        .await
        .expect("input-required is a decision point, not an error");
        assert!(matches!(outcome, LifecycleOutcome::InputRequired { .. }));
        assert_eq!(
            transport.cancels.lock().as_slice(),
            ["task-1"],
            "R-C: the peer's task must be cancelled, never leaked"
        );
    }

    #[tokio::test]
    async fn owner_cancellation_issues_tasks_cancel_and_returns_canceled() {
        let transport = ScriptedTransport::new(&["working", "working", "working"]);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let transitions = Mutex::new(Vec::new());
        let outcome = drive_task(
            &transport,
            serde_json::json!({}),
            &fast_config(),
            &cancel,
            |state| transitions.lock().push(state),
        )
        .await
        .expect("cancellation is a clean terminal");
        assert!(matches!(
            outcome,
            LifecycleOutcome::Terminal {
                state: RapTaskState::Canceled,
                ..
            }
        ));
        assert_eq!(transport.cancels.lock().as_slice(), ["task-1"]);
        assert_eq!(
            transitions.lock().as_slice(),
            [RapTaskState::Working, RapTaskState::Canceled],
            "owner cancellation must project the terminal state"
        );
    }
    #[tokio::test]
    async fn remote_cancel_failure_is_never_reported_as_local_success() {
        let input_required =
            ScriptedTransport::new(&["working", "input-required"]).with_cancel_error();
        let error = drive_task(
            &input_required,
            serde_json::json!({}),
            &fast_config(),
            &CancellationToken::new(),
            |_| {},
        )
        .await
        .expect_err("input-required cleanup failure must surface");
        assert!(matches!(error, A2aError::Request(_)));

        let owner_cancel = ScriptedTransport::new(&["working"]).with_cancel_error();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = drive_task(
            &owner_cancel,
            serde_json::json!({}),
            &fast_config(),
            &cancel,
            |_| {},
        )
        .await
        .expect_err("owner cancellation cleanup failure must surface");
        assert!(matches!(error, A2aError::Request(_)));
    }

    #[tokio::test]
    async fn illegal_peer_reported_sequence_is_refused() {
        // working -> submitted is illegal on the RAP FSM and both snapshots are
        // non-terminal, so the second poll must exercise transition validation.
        let transport = ScriptedTransport::new(&["working", "submitted"]);
        let error = drive_task(
            &transport,
            serde_json::json!({}),
            &fast_config(),
            &CancellationToken::new(),
            |_| {},
        )
        .await
        .expect_err("illegal peer transition must be refused");
        assert!(
            matches!(
                error,
                A2aError::MalformedResponse { ref reason }
                    if reason.contains("illegal task-state sequence")
            ),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn camelcase_state_from_peer_is_refused() {
        let transport = ScriptedTransport::new(&["inputRequired"]);
        let error = drive_task(
            &transport,
            serde_json::json!({}),
            &fast_config(),
            &CancellationToken::new(),
            |_| {},
        )
        .await
        .expect_err("camelCase wire spelling must be refused");
        assert!(matches!(error, A2aError::UnknownTaskState { .. }));
    }
}
