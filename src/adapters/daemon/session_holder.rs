//! `DaemonSessionHolder` — sealed implementation of `SessionHolderPort`.
//!
//! Lives inside the daemon adapter so it can reuse the private `pidfile` module
//! and the public `protocol` framing without exposing them beyond this crate
use std::path::Path;
use std::time::Duration;

use super::pidfile::{self, GuardOutcome};
use super::protocol::{ClientFrame, DaemonFrame, PROTOCOL_VERSION, read_frame, write_frame};
use crate::domain::models::channel_kind::ChannelKind;
use crate::domain::ports::{HeldSession, HolderState, SessionHolderPort};
use crate::infrastructure::paths;
const DAEMON_HOLDER_QUERY_TIMEOUT: Duration = Duration::from_secs(2);

/// Query the live daemon in a workspace for its held session id.
///
/// Non-Unix platforms always report `NoDaemon` (fail-open by absence of
/// daemon infrastructure — no pidfile/socket exists to query).
pub struct DaemonSessionHolder;

#[async_trait::async_trait]
impl SessionHolderPort for DaemonSessionHolder {
    async fn live_holder(&self, workspace: &Path) -> HolderState {
        live_holder_inner(workspace).await
    }
}

async fn live_holder_inner(workspace: &Path) -> HolderState {
    #[cfg(not(unix))]
    return HolderState::NoDaemon;

    #[cfg(unix)]
    {
        let pid_path = match paths::daemon_pid_path(workspace) {
            Ok(p) => p,
            Err(_) => return HolderState::NoDaemon,
        };

        match pidfile::check_running(&pid_path) {
            GuardOutcome::Free | GuardOutcome::Stale => HolderState::NoDaemon,
            GuardOutcome::Running(pf) => {
                let socket = match paths::daemon_socket_path(workspace) {
                    Ok(p) => p,
                    Err(_) => return HolderState::Unknown,
                };

                match tokio::time::timeout(
                    DAEMON_HOLDER_QUERY_TIMEOUT,
                    query_holder(&socket, pf.pid),
                )
                .await
                {
                    Ok(Ok(state)) => state,
                    Ok(Err(_)) => HolderState::Unknown,
                    Err(_) => HolderState::Unknown,
                }
            }
        }
    }
}

#[cfg(unix)]
async fn query_holder(socket: &Path, pid: u32) -> anyhow::Result<HolderState> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    let stream = UnixStream::connect(socket).await?;
    let (mut read_half, mut write_half) = stream.into_split();

    write_frame(
        &mut write_half,
        &ClientFrame::Attach {
            protocol_version: PROTOCOL_VERSION,
            read_only_ok: true,
        },
    )
    .await?;

    match read_frame::<_, DaemonFrame>(&mut read_half).await? {
        Some(DaemonFrame::AttachAck { snapshot, .. }) => {
            // Best-effort clean detach; ignore errors — we already have the snapshot.
            let _ = write_frame(&mut write_half, &ClientFrame::Detach).await;

            // Normalise an empty channel list to Terminal so human/JSON rendering
            // always has a channel name.
            let channels = if snapshot.channels.is_empty() {
                vec![ChannelKind::Terminal]
            } else {
                snapshot.channels
            };

            Ok(HolderState::HeldBy(HeldSession {
                conversation_id: snapshot.conversation_id,
                pid,
                channels,
            }))
        }
        _ => {
            let _ = write_frame(&mut write_half, &ClientFrame::Detach).await;
            Ok(HolderState::Unknown)
        }
    }
}
