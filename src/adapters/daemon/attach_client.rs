//! Minimal headless attach client (Story 12.2b AC7).
//!
//! `rustain daemon attach` connects to the running daemon's Unix socket, performs
//! the [`protocol`](super::protocol) handshake, prints the conversation as it
//! streams, and submits a turn per line of stdin. Ctrl-D detaches cleanly (the
//! daemon and any in-flight turn keep running — AC4).
//!
//! This is deliberately line-based, not a TUI: the rich multi-channel attach TUI
//! (`run_attached`, history paging, dimmed origin prefixes, read-only indicator)
//! is Story 12.2c. 12.2b ships the daemon-side protocol + a usable client.

use anyhow::{Context, Result};
use std::path::Path;
use tokio::net::UnixStream;

use crate::domain::models::tool_call::RequestId;
use crate::domain::models::{ApprovalOutcome, StreamChunk};
use crate::infrastructure::runtime::event_bus::RawEventKind;

use super::protocol::{
    AttachMode, ClientFrame, DaemonFrame, PROTOCOL_VERSION, read_frame, write_frame,
};

/// Connect + attach to this workspace's daemon and run the line-based client loop.
pub async fn run_attach(workspace: &Path) -> Result<()> {
    let socket = crate::infrastructure::paths::daemon_socket_path(workspace)?;
    let stream = UnixStream::connect(&socket).await.with_context(|| {
        format!(
            "connecting to the daemon at {} — is it running? (`rustain daemon start`)",
            socket.display()
        )
    })?;
    let (mut read_half, mut write_half) = stream.into_split();

    write_frame(
        &mut write_half,
        &ClientFrame::Attach {
            protocol_version: PROTOCOL_VERSION,
            read_only_ok: false,
        },
    )
    .await?;

    // Handshake.
    match read_frame::<_, DaemonFrame>(&mut read_half).await? {
        Some(DaemonFrame::AttachAck {
            granted_mode,
            snapshot,
        }) => {
            for m in &snapshot.transcript {
                println!("{} {}", m.origin.as_prefix(), m.content);
            }
            if snapshot.blocked_actions_waiting > 0 {
                println!(
                    "\u{26a0} {} action(s) waiting on you (denied while no client was attached).",
                    snapshot.blocked_actions_waiting
                );
            }
            match granted_mode {
                AttachMode::ReadWrite => {
                    println!("— attached (writer). Type a message, Ctrl-D to detach. —")
                }
                AttachMode::ReadOnly => {
                    println!("— attached (read-only; another client holds the writer). —")
                }
            }
        }
        Some(DaemonFrame::Error(e)) => anyhow::bail!("daemon rejected attach: {e}"),
        other => anyhow::bail!("unexpected first frame from daemon: {other:?}"),
    }

    let mut reader = tokio::spawn(async move {
        loop {
            match read_frame::<_, DaemonFrame>(&mut read_half).await {
                Ok(Some(frame)) => match frame {
                    DaemonFrame::Event(raw) => {
                        if let RawEventKind::Provider(StreamChunk::Text { content, .. }) = &raw.kind
                        {
                            print!("{content}");
                            use std::io::Write;
                            let _ = std::io::stdout().flush();
                        }
                    }
                    DaemonFrame::ApprovalRequest {
                        request_id, tool, ..
                    } => {
                        println!(
                            "\n[APPROVAL REQUESTED] tool={tool} request_id={}\n\
                             Type: approve <id> | deny <id>",
                            request_id.0
                        );
                    }
                    DaemonFrame::Detached => return Ok::<(), anyhow::Error>(()),
                    DaemonFrame::Error(e) => eprintln!("\n[daemon error] {e}"),
                    _ => {}
                },
                Ok(None) => return Ok::<(), anyhow::Error>(()),
                Err(e) => {
                    eprintln!("\n[daemon protocol error] {e}");
                    anyhow::bail!(e);
                }
            }
        }
    });

    // stdin → UserMessage, a line at a time. Read on a blocking std thread (tokio's
    // `io-std` feature is not enabled — zero Cargo change, AC7) and forward lines
    // over a channel to the async writer.
    let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<String>(8);
    std::thread::spawn(move || {
        use std::io::BufRead;
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => {
                    if line_tx.blocking_send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        // Drop `line_tx` → the async loop sees the channel close (Ctrl-D / EOF).
    });
    while let Some(line) = line_rx.recv().await {
        if line.trim().is_empty() {
            println!("(empty input — type a message or Ctrl-D to detach)");
            continue;
        }
        let frame = if let Some(frame) = parse_approval_command(&line) {
            frame
        } else {
            ClientFrame::UserMessage {
                text: line,
                images: vec![],
            }
        };
        if write_frame(&mut write_half, &frame).await.is_err() {
            break;
        }
    }

    // EOF (Ctrl-D) → detach cleanly.
    let _ = write_frame(&mut write_half, &ClientFrame::Detach).await;
    match tokio::time::timeout(std::time::Duration::from_millis(100), &mut reader).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => {
            tracing::warn!(error = %e, "attach reader task stopped with protocol error")
        }
        Ok(Err(e)) => tracing::warn!(error = ?e, "attach reader task panicked"),
        Err(_) => tracing::debug!("attach reader timed out after detach"),
    }
    reader.abort();
    println!("\n— detached —");
    Ok(())
}

fn parse_approval_command(line: &str) -> Option<ClientFrame> {
    let mut parts = line.split_whitespace();
    let verb = parts.next()?;
    let id = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let outcome = match verb {
        "approve" => ApprovalOutcome::Once,
        "deny" => ApprovalOutcome::Reject { feedback: None },
        _ => return None,
    };
    Some(ClientFrame::ApprovalResponse {
        request_id: RequestId(id.to_owned()),
        outcome,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_approval_commands_into_response_frames() {
        match parse_approval_command("approve req-1").expect("approve command") {
            ClientFrame::ApprovalResponse {
                request_id,
                outcome: ApprovalOutcome::Once,
            } => assert_eq!(request_id.0, "req-1"),
            other => panic!("unexpected frame: {other:?}"),
        }

        match parse_approval_command("deny req-2").expect("deny command") {
            ClientFrame::ApprovalResponse {
                request_id,
                outcome: ApprovalOutcome::Reject { feedback: None },
            } => assert_eq!(request_id.0, "req-2"),
            other => panic!("unexpected frame: {other:?}"),
        }

        assert!(parse_approval_command("approve").is_none());
        assert!(parse_approval_command("approve req extra").is_none());
        assert!(parse_approval_command("hello daemon").is_none());
    }
}
