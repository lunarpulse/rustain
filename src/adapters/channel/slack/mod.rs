//! Slack channel adapter (Story C-12): app DMs over outbound Socket Mode.
//!
//! The adapter is one daemon-owned task pair behind [`ChannelPort`]:
//!
//! * the **connection loop** opens a Socket Mode connection, acknowledges every
//!   envelope at once (Slack redelivers an unacknowledged one), admits only
//!   allowlisted human senders in app DMs ([`proto::handle_frame`]) and offers
//!   the sanitized text to a small bounded queue;
//! * the **reply worker** takes queued DMs one at a time, sends a
//!   [`ChannelTurnRequest`] to the daemon turn queue, waits for the *one*
//!   whole-turn reply that the daemon resolves at terminal settlement (C-2b),
//!   and posts it to the admitted DM only.
//!
//! Credentials are the two environment tokens read at composition; they never
//! reach a log, a config file or a `Debug` string. There is no listener, no
//! webhook and no Slack route for unattended notices: [`ChannelPort::notify`]
//! stays the trait's no-op, so a cron job with `forward = true` cannot post
//! to Slack.
//!
//! Policies fixed by the Task 0 review (conservative defaults, owner may revise):
//! * **Bounds:** frames over 256 KiB are dropped; at most 8 admitted DMs wait
//!   for a turn (overflow is dropped with a log line, no reply); 256 message
//!   identities are remembered for replay suppression; a reply is capped at
//!   30,000 characters and posted in ordered chunks of at most 3,000.
//! * **Sends:** at most once per chunk. A failed or timed-out `chat.postMessage`
//!   is not retried (its outcome may be ambiguous and a retry could duplicate);
//!   the remaining chunks of that reply are abandoned and logged.
//! * **Cancellation:** shutdown cancels the connection loop, a pending
//!   reconnect sleep, a waiting turn and an in-flight post; nothing is sent
//!   after cancellation.
//! * **Retries (NFR60):** after the first loss or failed attempt, at most five
//!   reconnect attempts with exponential backoff (1, 2, 4, 8, 16 s); a received
//!   `hello` resets the budget; the sixth consecutive failure is `Offline`.

#[cfg(test)]
#[path = "fixture_tests.rs"]
pub(crate) mod fixture;
pub mod proto;
pub mod transport;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::domain::errors::{AdapterCompositionError, TransitionError};
use crate::domain::models::{
    ChannelKind, ChannelLinkState, ChannelTurnRequest, HealthSummary, PortDimension,
};
use crate::domain::ports::ChannelPort;
use crate::domain::services::channel_allowlist::is_slack_user_id;

use proto::{Action, DEDUPE_CAPACITY, DedupeWindow, InboundMessage, handle_frame, split_reply};
pub use transport::SlackHttpTransport;

/// Compose the daemon Slack adapter from the profile's `[channels.config]`, the
/// two credential values the caller read from the environment, and the daemon's
/// turn queue. Performs no I/O. Every failure names only the missing
/// requirement, never a value.
pub fn compose(
    config: Option<&toml::Value>,
    app_token: Option<String>,
    bot_token: Option<String>,
    turn_tx: Option<mpsc::UnboundedSender<ChannelTurnRequest>>,
) -> Result<Arc<dyn ChannelPort>, AdapterCompositionError> {
    let missing = |missing_field: &str| AdapterCompositionError::MissingComposeContext {
        port: PortDimension::Channels,
        name: "slack".into(),
        missing_field: missing_field.into(),
    };
    let allowed = config
        .and_then(|conf| conf.get("allowed_user_ids"))
        .and_then(toml::Value::as_array)
        .ok_or_else(|| missing("allowed_user_ids required in [channels.config]"))?
        .iter()
        .map(|value| value.as_str().map(str::to_string))
        .collect::<Option<Vec<String>>>()
        .ok_or_else(|| missing("allowed_user_ids must contain only Slack user ID strings"))?;
    let app_token = app_token
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| missing("SLACK_APP_TOKEN env var required for slack adapter"))?;
    let bot_token = bot_token
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| missing("SLACK_BOT_TOKEN env var required for slack adapter"))?;
    let turn_tx = turn_tx.ok_or_else(|| {
        missing("channel_turn_tx not wired (daemon-only; TUI mode cannot use slack adapter)")
    })?;
    Ok(Arc::new(SlackChannelAdapter::new(
        app_token, bot_token, allowed, turn_tx,
    )?))
}

/// Reconnect attempts allowed after a loss or failed attempt (NFR60).
pub const MAX_RECONNECT_RETRIES: u32 = 5;
/// Admitted DMs allowed to wait for a turn; more are dropped.
pub const PENDING_TURN_CAPACITY: usize = 8;
const BACKOFF_CAP_SECS: u64 = 30;
const HELLO_TIMEOUT: Duration = Duration::from_secs(30);
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(45);
const POST_TIMEOUT: Duration = Duration::from_secs(20);
const SHUTDOWN_JOIN: Duration = Duration::from_secs(5);

/// A redacted transport failure. It carries a short fixed description or a
/// Slack API error code (`[a-z_]+`), never a token, URL, header or payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackError(pub String);

impl std::fmt::Display for SlackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SlackError {}

/// What one read from the socket produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketEvent {
    /// A text frame (an envelope, `hello` or `disconnect`).
    Text(String),
    /// Control traffic (ping, pong, binary): the peer is alive, nothing to handle.
    Activity,
    /// The peer closed the connection.
    Closed,
}

/// One open Socket Mode connection.
#[async_trait::async_trait]
pub trait SlackSocket: Send {
    /// The next event from the peer.
    async fn next_event(&mut self) -> Result<SocketEvent, SlackError>;
    /// Send one text frame (an acknowledgement).
    async fn send_text(&mut self, text: String) -> Result<(), SlackError>;
}

/// The two Slack operations the adapter needs. The production implementation is
/// [`SlackHttpTransport`]; a typed local fixture implements it for offline
/// evidence. It is a seam, not a switch: nothing in config or environment
/// selects another implementation.
#[async_trait::async_trait]
pub trait SlackTransport: Send + Sync + 'static {
    /// Bootstrap with the app-level token (`apps.connections.open`) and open the
    /// WebSocket it names.
    async fn connect(&self) -> Result<Box<dyn SlackSocket>, SlackError>;
    /// `chat.postMessage` with the bot token to the admitted DM `channel`.
    async fn post_message(&self, channel: &str, text: &str) -> Result<(), SlackError>;
}

/// Slack app-DM channel adapter.
pub struct SlackChannelAdapter {
    transport: Arc<dyn SlackTransport>,
    allowed_user_ids: Arc<Vec<String>>,
    turn_tx: mpsc::UnboundedSender<ChannelTurnRequest>,
    shutdown: CancellationToken,
    link: watch::Sender<ChannelLinkState>,
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl SlackChannelAdapter {
    /// Compose the production adapter from the two env tokens and the stored
    /// allowlist. Performs no I/O.
    pub fn new(
        app_token: String,
        bot_token: String,
        allowed_user_ids: Vec<String>,
        turn_tx: mpsc::UnboundedSender<ChannelTurnRequest>,
    ) -> Result<Self, AdapterCompositionError> {
        let transport = SlackHttpTransport::new(app_token, bot_token).map_err(|e| {
            AdapterCompositionError::MissingComposeContext {
                port: PortDimension::Channels,
                name: "slack".into(),
                missing_field: format!("slack transport unavailable: {e}"),
            }
        })?;
        Self::with_transport(Arc::new(transport), allowed_user_ids, turn_tx)
    }

    /// Compose over any [`SlackTransport`] (typed local fixtures).
    pub fn with_transport(
        transport: Arc<dyn SlackTransport>,
        allowed_user_ids: Vec<String>,
        turn_tx: mpsc::UnboundedSender<ChannelTurnRequest>,
    ) -> Result<Self, AdapterCompositionError> {
        let invalid = |missing_field: &str| AdapterCompositionError::MissingComposeContext {
            port: PortDimension::Channels,
            name: "slack".into(),
            missing_field: missing_field.into(),
        };
        if allowed_user_ids.is_empty() {
            return Err(invalid(
                "allowed_user_ids must list at least one Slack user ID",
            ));
        }
        if !allowed_user_ids.iter().all(|id| is_slack_user_id(id)) {
            return Err(invalid("allowed_user_ids must contain only Slack user IDs"));
        }
        let (link, _) = watch::channel(ChannelLinkState::Connecting);
        Ok(Self {
            transport,
            allowed_user_ids: Arc::new(allowed_user_ids),
            turn_tx,
            shutdown: CancellationToken::new(),
            link,
            handle: Arc::new(Mutex::new(None)),
        })
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// The current redacted link state.
    pub fn link_state(&self) -> ChannelLinkState {
        *self.link.borrow()
    }
}

#[async_trait::async_trait]
impl ChannelPort for SlackChannelAdapter {
    fn health_snapshot(&self) -> HealthSummary {
        match self.link_state() {
            ChannelLinkState::Connected => HealthSummary::healthy("connected"),
            ChannelLinkState::Connecting => {
                HealthSummary::degraded("connecting", "wait for the Slack connection")
            }
            ChannelLinkState::Reconnecting => {
                HealthSummary::degraded("reconnecting", "wait for Slack reconnect")
            }
            ChannelLinkState::Offline => {
                HealthSummary::error("offline", "restart daemon or fix Slack config")
            }
        }
    }

    fn health_updates(&self) -> Option<watch::Receiver<ChannelLinkState>> {
        Some(self.link.subscribe())
    }

    async fn start_loop(&self) -> Result<(), TransitionError> {
        let mut slot = self.handle.lock().await;
        if slot.as_ref().is_some_and(|h| !h.is_finished()) {
            return Ok(());
        }
        let transport = self.transport.clone();
        let allowed = self.allowed_user_ids.clone();
        let turn_tx = self.turn_tx.clone();
        let cancel = self.shutdown.clone();
        let link = self.link.clone();
        *slot = Some(tokio::spawn(async move {
            run_adapter(transport, allowed, turn_tx, cancel, link).await;
        }));
        Ok(())
    }

    async fn shutdown_loop(&self) -> Result<(), TransitionError> {
        self.shutdown.cancel();
        if let Some(handle) = self.handle.lock().await.take() {
            let mut handle = handle;
            if tokio::time::timeout(SHUTDOWN_JOIN, &mut handle)
                .await
                .is_err()
            {
                handle.abort();
                let _ = handle.await;
            }
        }
        Ok(())
    }
}

async fn run_adapter(
    transport: Arc<dyn SlackTransport>,
    allowed: Arc<Vec<String>>,
    turn_tx: mpsc::UnboundedSender<ChannelTurnRequest>,
    cancel: CancellationToken,
    link: watch::Sender<ChannelLinkState>,
) {
    let (pending_tx, pending_rx) = mpsc::channel::<InboundMessage>(PENDING_TURN_CAPACITY);
    let worker = run_reply_worker(transport.clone(), turn_tx, pending_rx, cancel.clone());
    let connection = run_connection_loop(transport, allowed, pending_tx, cancel.clone(), link);
    tokio::join!(worker, connection);
}

/// Seconds to wait before retry number `retry` (1-based): 1, 2, 4, 8, 16, then 30.
pub fn backoff_secs(retry: u32) -> u64 {
    2_u64
        .saturating_pow(retry.saturating_sub(1))
        .min(BACKOFF_CAP_SECS)
}

fn set_link(link: &watch::Sender<ChannelLinkState>, state: ChannelLinkState) {
    link.send_if_modified(|current| {
        if *current == state {
            false
        } else {
            *current = state;
            true
        }
    });
}

enum SessionEnd {
    /// The connection reached `hello` and was later lost or told to reconnect.
    LostAfterHello,
    /// The attempt failed before `hello`.
    Failed,
}

async fn run_connection_loop(
    transport: Arc<dyn SlackTransport>,
    allowed: Arc<Vec<String>>,
    pending_tx: mpsc::Sender<InboundMessage>,
    cancel: CancellationToken,
    link: watch::Sender<ChannelLinkState>,
) {
    let mut seen = DedupeWindow::new(DEDUPE_CAPACITY);
    let mut retries: u32 = 0;
    let mut lost_before = false;
    loop {
        set_link(
            &link,
            if lost_before {
                ChannelLinkState::Reconnecting
            } else {
                ChannelLinkState::Connecting
            },
        );
        let end = tokio::select! {
            () = cancel.cancelled() => return,
            end = run_session(&*transport, &allowed, &pending_tx, &mut seen, &link) => end,
        };
        match end {
            SessionEnd::LostAfterHello => retries = 0,
            SessionEnd::Failed => {}
        }
        lost_before = true;
        set_link(&link, ChannelLinkState::Reconnecting);
        if retries >= MAX_RECONNECT_RETRIES {
            tracing::error!("slack: reconnect budget spent, channel offline");
            set_link(&link, ChannelLinkState::Offline);
            return;
        }
        retries += 1;
        let delay = Duration::from_secs(backoff_secs(retries));
        tracing::warn!(
            attempt = retries,
            delay_secs = delay.as_secs(),
            "slack: reconnecting"
        );
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = cancel.cancelled() => return,
        }
    }
}

async fn run_session(
    transport: &dyn SlackTransport,
    allowed: &[String],
    pending_tx: &mpsc::Sender<InboundMessage>,
    seen: &mut DedupeWindow,
    link: &watch::Sender<ChannelLinkState>,
) -> SessionEnd {
    let mut socket = match tokio::time::timeout(CONNECT_TIMEOUT, transport.connect()).await {
        Ok(Ok(socket)) => socket,
        Ok(Err(error)) => {
            tracing::warn!(%error, "slack: connect failed");
            return SessionEnd::Failed;
        }
        Err(_) => {
            tracing::warn!("slack: connect timed out");
            return SessionEnd::Failed;
        }
    };
    let mut hello = false;
    loop {
        let wait = if hello { IDLE_TIMEOUT } else { HELLO_TIMEOUT };
        let frame = match tokio::time::timeout(wait, socket.next_event()).await {
            Ok(Ok(SocketEvent::Text(frame))) => frame,
            Ok(Ok(SocketEvent::Activity)) => continue,
            Ok(Ok(SocketEvent::Closed)) => {
                tracing::warn!("slack: connection closed by peer");
                return ended(hello);
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, "slack: connection error");
                return ended(hello);
            }
            Err(_) => {
                tracing::warn!("slack: connection idle too long");
                return ended(hello);
            }
        };
        let handled = handle_frame(&frame, allowed, seen);
        if let Some(ack) = handled.ack {
            if let Err(error) = socket.send_text(ack).await {
                tracing::warn!(%error, "slack: acknowledgement failed");
                return ended(hello);
            }
        }
        match handled.action {
            Action::Hello => {
                hello = true;
                set_link(link, ChannelLinkState::Connected);
            }
            Action::Disconnect => {
                tracing::info!("slack: server requested a reconnect");
                return ended(hello);
            }
            Action::Dispatch(message) => {
                if pending_tx.try_send(message).is_err() {
                    tracing::warn!("slack: too many pending turns; dropping an admitted DM");
                }
            }
            Action::Drop(reason) => {
                tracing::debug!(?reason, "slack: frame not dispatched");
            }
            Action::Ignore => {}
        }
    }
}

fn ended(hello: bool) -> SessionEnd {
    if hello {
        SessionEnd::LostAfterHello
    } else {
        SessionEnd::Failed
    }
}

async fn run_reply_worker(
    transport: Arc<dyn SlackTransport>,
    turn_tx: mpsc::UnboundedSender<ChannelTurnRequest>,
    mut pending_rx: mpsc::Receiver<InboundMessage>,
    cancel: CancellationToken,
) {
    loop {
        let message = tokio::select! {
            () = cancel.cancelled() => return,
            message = pending_rx.recv() => match message {
                Some(message) => message,
                None => return,
            },
        };
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let request = ChannelTurnRequest {
            text: message.text,
            origin: ChannelKind::Slack,
            response_tx,
        };
        if turn_tx.send(request).is_err() {
            tracing::warn!("slack: daemon channel turn receiver closed");
            continue;
        }
        let response = tokio::select! {
            () = cancel.cancelled() => return,
            response = response_rx => match response {
                Ok(response) => response,
                Err(_) => {
                    tracing::warn!("slack: response channel closed");
                    continue;
                }
            },
        };
        for chunk in split_reply(&response) {
            let sent = tokio::select! {
                () = cancel.cancelled() => return,
                sent = tokio::time::timeout(POST_TIMEOUT, transport.post_message(&message.channel, &chunk)) => sent,
            };
            match sent {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(%error, "slack: reply chunk not sent; abandoning the rest");
                    break;
                }
                Err(_) => {
                    tracing::warn!("slack: reply chunk timed out; abandoning the rest");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
