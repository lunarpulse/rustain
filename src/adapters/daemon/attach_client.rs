//! Minimal headless attach client (Story 12.2b AC7).
//!
//! `rustain daemon attach` connects to the running daemon's Unix socket, performs
//! the [`protocol`](super::protocol) handshake, prints the conversation as it
//! streams, and submits a turn per line of stdin. Ctrl-D detaches cleanly (the
//! daemon and any in-flight turn keep running — AC4).
//!
//! This is deliberately line-based, not a TUI. Exact approval input is retained
//! locally per pending request and shown only by `review <id>`; reading it never
//! submits a turn or decides a request.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::Arc;
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::domain::models::tool_call::RequestId;
use crate::domain::models::{ApprovalOutcome, StreamChunk};
use crate::domain::services::approval_presentation::{BODY_DELIMITER, body_lines};
use crate::infrastructure::runtime::event_bus::RawEventKind;

use super::protocol::{
    ApprovalRefusal, AttachMode, AttachSnapshot, ClientFrame, ConnectionTier, DaemonFrame,
    ExactInputAssembly, WithheldToolInput, answer_attach_challenge, frame_too_large_notice,
    read_frame, write_frame,
};
use crate::adapters::rap::IdentityKeyStore;
use crate::adapters::tui::widgets::delimited_body::approval_body_rows;

struct PlainApproval {
    tool: String,
    transfer: Option<ExactInputAssembly>,
    ready: Option<serde_json::Value>,
}

/// The reader and stdin share one request table. Review holds the same lock as
/// completion/retirement, so it cannot read a partial or already-retired value.
struct PlainApprovals {
    writer: bool,
    requests: HashMap<String, PlainApproval>,
}

impl PlainApprovals {
    fn new(writer: bool) -> Self {
        Self {
            writer,
            requests: HashMap::new(),
        }
    }

    fn handle_frame(
        &mut self,
        frame: DaemonFrame,
        output: &mut impl Write,
    ) -> std::io::Result<Option<DaemonFrame>> {
        match frame {
            DaemonFrame::ApprovalRequest {
                request_id,
                tool,
                summary,
                input_bytes,
                ..
            } => {
                if self.writer {
                    writeln!(
                        output,
                        "\n[APPROVAL REQUESTED] tool={tool} request_id={}",
                        request_id.0
                    )?;
                    if let Some(bytes) = input_bytes {
                        writeln!(output, "  {summary}")?;
                        writeln!(output, "Type: approve <id> | deny <id> | review <id>")?;
                        self.requests.insert(
                            request_id.0,
                            PlainApproval {
                                tool,
                                transfer: Some(ExactInputAssembly::new(bytes)),
                                ready: None,
                            },
                        );
                    } else {
                        // Sender consent keeps its existing line-client surface.
                        writeln!(output, "Type: approve <id> | deny <id>")?;
                    }
                }
            }
            DaemonFrame::ApprovalInputChunk {
                request_id,
                sequence,
                data,
            } => {
                if let Some(request) = self.requests.get_mut(&request_id.0)
                    && let Some(transfer) = request.transfer.as_mut()
                    && transfer.push(sequence, &data).is_err()
                {
                    // A failed transfer can never become reviewable, even if
                    // apparently valid chunks arrive after the invalid one.
                    request.transfer = None;
                }
            }
            DaemonFrame::ApprovalInputComplete { request_id, chunks } => {
                if let Some(request) = self.requests.get_mut(&request_id.0)
                    && let Some(transfer) = request.transfer.take()
                {
                    request.ready = transfer.finish(chunks).ok();
                }
            }
            DaemonFrame::ApprovalClosed { request_id } => {
                self.requests.remove(&request_id.0);
            }
            DaemonFrame::ApprovalRefused { request_id, reason } => {
                if reason == ApprovalRefusal::NoLongerPending {
                    self.requests.remove(&request_id.0);
                }
                writeln!(output, "{reason}")?;
            }
            DaemonFrame::ToolEventWithheld { withheld, .. } => {
                writeln!(output, "{}", withheld.notice())?;
            }
            DaemonFrame::FrameTooLarge { frame, bytes } => {
                writeln!(output, "{}", frame_too_large_notice(&frame, bytes))?;
            }
            DaemonFrame::History {
                withheld_tool_inputs,
                ..
            } => print_withheld_notices(&withheld_tool_inputs, output)?,
            other => return Ok(Some(other)),
        }
        output.flush()?;
        Ok(None)
    }

    fn review(
        &self,
        id: &str,
        terminal_width: Option<usize>,
        output: &mut impl Write,
    ) -> std::io::Result<()> {
        let Some(request) = self.requests.get(id) else {
            writeln!(output, "No exact input available for {id}.")?;
            return output.flush();
        };
        let Some(input) = &request.ready else {
            writeln!(output, "  receiving exact input…")?;
            return output.flush();
        };
        writeln!(output, "--- exact input for {id} (not run yet) ---")?;
        let lines = body_lines(&request.tool, input);
        if let Some(width) = terminal_width {
            let (rows, _) = approval_body_rows(&lines, width.saturating_sub(2), usize::MAX);
            for row in rows {
                writeln!(output, "{BODY_DELIMITER}{}", row.text)?;
            }
        } else {
            for line in lines {
                writeln!(output, "{BODY_DELIMITER}{}", line.joined())?;
            }
        }
        writeln!(output, "--- end {id} ---")?;
        output.flush()
    }

    fn stdin_frame(
        &self,
        line: String,
        terminal_width: Option<usize>,
        output: &mut impl Write,
    ) -> std::io::Result<Option<ClientFrame>> {
        let mut parts = line.split_whitespace();
        let Some(verb) = parts.next() else {
            writeln!(output, "(empty input — type a message or Ctrl-D to detach)")?;
            return Ok(None);
        };
        if verb == "review" {
            if let Some(id) = parts.next()
                && parts.next().is_none()
            {
                self.review(id, terminal_width, output)?;
            } else {
                writeln!(output, "Usage: review <id>")?;
                output.flush()?;
            }
            return Ok(None);
        }
        Ok(Some(parse_approval_command(&line).unwrap_or(
            ClientFrame::UserMessage {
                text: line,
                images: vec![],
            },
        )))
    }
}

fn print_withheld_notices(
    withheld: &[WithheldToolInput],
    output: &mut impl Write,
) -> std::io::Result<()> {
    for input in withheld {
        writeln!(output, "{}", input.withheld.notice())?;
    }
    Ok(())
}

fn print_snapshot(
    snapshot: &AttachSnapshot,
    mode: AttachMode,
    output: &mut impl Write,
) -> std::io::Result<()> {
    for message in &snapshot.transcript {
        writeln!(output, "{} {}", message.origin.as_prefix(), message.content)?;
    }
    print_withheld_notices(&snapshot.withheld_tool_inputs, output)?;
    if snapshot.blocked_actions_waiting > 0 {
        writeln!(
            output,
            "\u{26a0} {} action(s) waiting on you (denied while no client was attached).",
            snapshot.blocked_actions_waiting
        )?;
    }
    if mode == AttachMode::ReadOnly && snapshot.retained_approvals > 0 {
        writeln!(
            output,
            "{} approval(s) waiting for the writer",
            snapshot.retained_approvals
        )?;
    }
    match mode {
        AttachMode::ReadWrite => writeln!(
            output,
            "— attached (writer). Type a message, Ctrl-D to detach. —"
        )?,
        AttachMode::ReadOnly => writeln!(
            output,
            "— attached (read-only; another client holds the writer). —"
        )?,
    }
    output.flush()
}

fn stdout_terminal_width() -> Option<usize> {
    std::io::stdout()
        .is_terminal()
        .then(|| crossterm::terminal::size().map_or(80, |(width, _)| usize::from(width)))
}

fn print_frame_json(frame: &DaemonFrame) -> Result<()> {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, frame)?;
    writeln!(output)?;
    output.flush()?;
    Ok(())
}

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

    // Server-first challenge handshake (Story 17.1a): load (or provision) this
    // machine's identity key from the rustain data directory, then answer the
    // daemon's one-use challenge with an Ed25519 proof of possession.
    let signer = IdentityKeyStore::new(crate::infrastructure::paths::data_dir()?)
        .load_or_generate()
        .context("loading the local peer identity key")?;
    answer_attach_challenge(
        &mut read_half,
        &mut write_half,
        false,
        ConnectionTier::TrustedLocal,
        &signer,
    )
    .await?;

    // Handshake.
    let (mut peer_row_revision, writer) = match read_frame::<_, DaemonFrame>(&mut read_half).await?
    {
        Some(DaemonFrame::AttachAck {
            granted_mode,
            snapshot,
        }) => {
            print_snapshot(&snapshot, granted_mode, &mut std::io::stdout().lock())?;
            (
                snapshot.peer_row_revision,
                granted_mode == AttachMode::ReadWrite,
            )
        }
        Some(DaemonFrame::Error(e)) => anyhow::bail!("daemon rejected attach: {e}"),
        other => anyhow::bail!("unexpected first frame from daemon: {other:?}"),
    };
    // This line client has no TeamProposalDecision input surface. Keep ordinary
    // writer turns usable, but never opt in with EnableTeamProposals.

    let mut team_views = PlainTeamViews::default();
    let approvals = Arc::new(Mutex::new(PlainApprovals::new(writer)));
    let reader_approvals = Arc::clone(&approvals);
    let mut reader = tokio::spawn(async move {
        loop {
            match read_frame::<_, DaemonFrame>(&mut read_half).await {
                Ok(Some(frame)) => {
                    let frame = reader_approvals
                        .lock()
                        .await
                        .handle_frame(frame, &mut std::io::stdout().lock())?;
                    let Some(frame) = frame else {
                        continue;
                    };
                    match frame {
                        DaemonFrame::Event(raw) => {
                            if let RawEventKind::Provider(StreamChunk::Text { content, .. }) =
                                &raw.kind
                            {
                                print!("{content}");
                                use std::io::Write;
                                let _ = std::io::stdout().flush();
                            }
                        }
                        DaemonFrame::PeerRowOperation(operation) => {
                            if operation.revision() > peer_row_revision {
                                // A line client cannot replace a prior terminal row:
                                // emit the canonical addressed delta, not a new reply.
                                use std::io::Write;
                                let stdout = std::io::stdout();
                                let mut output = stdout.lock();
                                serde_json::to_writer(&mut output, operation.as_ref())?;
                                writeln!(output)?;
                                output.flush()?;
                                peer_row_revision = operation.revision();
                            }
                        }
                        DaemonFrame::TeamViewChunk {
                            request,
                            sequence,
                            view,
                        } => team_views.chunk(request, sequence, &view),
                        DaemonFrame::TeamViewDone { request, chunks } => {
                            team_views.done(&request, chunks, &mut std::io::stdout().lock())?
                        }
                        result @ DaemonFrame::TeamProposal { .. }
                        | result @ DaemonFrame::TeamProposalClosed { .. } => {
                            // The line client exposes the exact private frame; only
                            // the TM-09 TUI key handler constructs send decisions.
                            print_frame_json(&result)?;
                        }
                        result @ DaemonFrame::TeamSendStarted { .. }
                        | result @ DaemonFrame::TeamSendSettled { .. }
                        | result @ DaemonFrame::TeamStatusResult { .. }
                        | result @ DaemonFrame::TeamRetractPreviewed { .. }
                        | result @ DaemonFrame::TeamRetractAnswered { .. }
                        | result @ DaemonFrame::SenderConsentRevoked { .. } => {
                            print_frame_json(&result)?;
                        }
                        result @ DaemonFrame::RecipientActionResult {
                            target: super::protocol::RecipientActionTarget::Request { .. },
                            ..
                        } => print_frame_json(&result)?,
                        result @ DaemonFrame::RecipientActionResult { .. } => {
                            print_frame_json(&result)?
                        }
                        DaemonFrame::Detached => return Ok::<(), anyhow::Error>(()),
                        DaemonFrame::Error(e) => eprintln!("\n[daemon error] {e}"),
                        _ => {}
                    }
                }
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
        let frame = approvals.lock().await.stdin_frame(
            line,
            stdout_terminal_width(),
            &mut std::io::stdout().lock(),
        )?;
        let Some(frame) = frame else {
            continue;
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

#[cfg(test)]
mod exact_approval_tests {
    use super::super::protocol::ToolInputWithheld;
    use super::*;
    use crate::domain::models::tool_call::ApprovalSource;
    use crate::domain::models::{PermissionMode, ToolRisk};
    use serde_json::json;

    fn request(id: &str, tool: &str, bytes: Option<u64>) -> DaemonFrame {
        DaemonFrame::ApprovalRequest {
            request_id: RequestId(id.into()),
            tool: tool.into(),
            source: ApprovalSource::ForegroundTurn {
                conversation_id: "conversation".into(),
            },
            risk: ToolRisk::Standard,
            summary: "summary…".into(),
            input_bytes: bytes,
        }
    }

    fn chunk(id: &str, sequence: u32, data: &str) -> DaemonFrame {
        DaemonFrame::ApprovalInputChunk {
            request_id: RequestId(id.into()),
            sequence,
            data: data.into(),
        }
    }

    fn complete(id: &str, chunks: u32) -> DaemonFrame {
        DaemonFrame::ApprovalInputComplete {
            request_id: RequestId(id.into()),
            chunks,
        }
    }

    fn receive(state: &mut PlainApprovals, frame: DaemonFrame) -> String {
        let mut output = Vec::new();
        assert!(state.handle_frame(frame, &mut output).unwrap().is_none());
        String::from_utf8(output).unwrap()
    }

    fn stdin(
        state: &PlainApprovals,
        line: &str,
        width: Option<usize>,
    ) -> (Option<ClientFrame>, String) {
        let mut output = Vec::new();
        let frame = state.stdin_frame(line.into(), width, &mut output).unwrap();
        (frame, String::from_utf8(output).unwrap())
    }

    fn deliver(state: &mut PlainApprovals, id: &str, tool: &str, input: &serde_json::Value) {
        let data = serde_json::to_string(input).unwrap();
        receive(state, request(id, tool, Some(data.len() as u64)));
        receive(state, chunk(id, 0, &data));
        receive(state, complete(id, 1));
    }

    #[test]
    fn exact_request_and_sender_consent_keep_their_distinct_copy() {
        let mut state = PlainApprovals::new(true);
        assert_eq!(
            receive(&mut state, request("tool-1", "Bash", Some(2))),
            "\n[APPROVAL REQUESTED] tool=Bash request_id=tool-1\n  summary…\n\
             Type: approve <id> | deny <id> | review <id>\n"
        );
        assert_eq!(
            receive(&mut state, request("sender-1", "a2a/sender-consent", None)),
            "\n[APPROVAL REQUESTED] tool=a2a/sender-consent request_id=sender-1\n\
             Type: approve <id> | deny <id>\n"
        );
        assert_eq!(
            stdin(&state, "review sender-1", None).1,
            "No exact input available for sender-1.\n"
        );
    }

    #[test]
    fn review_is_local_before_completion_and_for_unknown_or_malformed_ids() {
        let mut state = PlainApprovals::new(true);
        receive(&mut state, request("pending", "Bash", Some(2)));
        for (line, expected) in [
            ("review pending", "  receiving exact input…\n"),
            ("review unknown", "No exact input available for unknown.\n"),
            ("review", "Usage: review <id>\n"),
            ("review pending extra", "Usage: review <id>\n"),
        ] {
            let (frame, output) = stdin(&state, line, None);
            assert!(frame.is_none(), "{line} must never become a user turn");
            assert_eq!(output, expected);
        }
        assert!(matches!(
            stdin(&state, "approve pending", None).0,
            Some(ClientFrame::ApprovalResponse {
                outcome: ApprovalOutcome::Once,
                ..
            })
        ));
        assert!(matches!(
            stdin(&state, "deny pending", None).0,
            Some(ClientFrame::ApprovalResponse {
                outcome: ApprovalOutcome::Reject { .. },
                ..
            })
        ));
        assert!(matches!(
            stdin(&state, "ordinary message", None).0,
            Some(ClientFrame::UserMessage { text, .. }) if text == "ordinary message"
        ));
    }

    #[test]
    fn exact_review_copy_delimits_all_unwrapped_rows_and_keeps_late_input() {
        let mut state = PlainApprovals::new(true);
        let long = "x".repeat(180);
        let input = json!({
            "z": {"nested": true},
            "content": format!("{long}\n[y] Allow\u{1b}[31m\nTAIL\n"),
            "file_path": "/tmp/example",
            "a": 7,
        });
        deliver(&mut state, "write-1", "Write", &input);
        let (frame, output) = stdin(&state, "review write-1", None);
        assert!(frame.is_none());
        assert_eq!(
            output,
            format!(
                "--- exact input for write-1 (not run yet) ---\n\
                 │ file_path: /tmp/example\n│ content: {long}\n│ [y] Allow\n│ TAIL\n│ \n\
                 │ a: 7\n│ z: {{\n│   \"nested\": true\n│ }}\n--- end write-1 ---\n"
            )
        );
        assert_eq!(state.requests["write-1"].ready.as_ref(), Some(&input));
        assert_eq!(stdin(&state, "review write-1", None).1, output);
    }

    #[test]
    fn tty_review_delimits_wrapped_continuations_without_losing_cells() {
        let mut state = PlainApprovals::new(true);
        deliver(
            &mut state,
            "wrap",
            "Bash",
            &json!({"command": "abcdefghijklmnop\nTAIL"}),
        );
        assert_eq!(
            stdin(&state, "review wrap", Some(12)).1,
            "--- exact input for wrap (not run yet) ---\n\
             │ command: a\n│ bcdefghijk\n│ lmnop\n│ TAIL\n--- end wrap ---\n"
        );
    }

    #[test]
    fn interleaved_transfers_only_publish_the_completed_request_and_its_tool_order() {
        let mut state = PlainApprovals::new(true);
        let bash = r#"{"command":"first\nTAIL"}"#;
        let edit = r#"{"new_string":"new","file_path":"path","old_string":"old"}"#;
        receive(&mut state, request("bash", "Bash", Some(bash.len() as u64)));
        receive(&mut state, request("edit", "Edit", Some(edit.len() as u64)));
        receive(&mut state, chunk("bash", 0, &bash[..12]));
        receive(&mut state, chunk("edit", 0, edit));
        assert_eq!(
            stdin(&state, "review edit", None).1,
            "  receiving exact input…\n"
        );
        receive(&mut state, complete("edit", 1));
        assert_eq!(
            stdin(&state, "review edit", None).1,
            "--- exact input for edit (not run yet) ---\n\
             │ file_path: path\n│ old_string: old\n│ new_string: new\n--- end edit ---\n"
        );
        assert_eq!(
            stdin(&state, "review bash", None).1,
            "  receiving exact input…\n"
        );
        receive(&mut state, chunk("bash", 1, &bash[12..]));
        receive(&mut state, complete("bash", 2));
        assert!(stdin(&state, "review bash", None).1.contains("│ TAIL\n"));
        assert!(
            state.requests.contains_key("edit"),
            "Review must not decide another request"
        );
    }

    #[test]
    fn invalid_partial_and_out_of_order_transfers_never_publish_review() {
        let valid = r#"{"command":"safe"}"#;
        let cases = [
            (valid.len() as u64, vec![(0, &valid[..5])], 1),
            (valid.len() as u64, vec![(0, valid)], 2),
            (valid.len() as u64 + 1, vec![(0, valid)], 1),
            (valid.len() as u64 - 1, vec![(0, valid)], 1),
            (4, vec![(0, "oops")], 1),
            (valid.len() as u64, vec![(1, valid)], 1),
            (valid.len() as u64, vec![(1, valid), (0, valid)], 1),
            (valid.len() as u64, vec![(0, valid), (0, "")], 1),
        ];
        for (bytes, chunks, count) in cases {
            let mut state = PlainApprovals::new(true);
            receive(&mut state, request("invalid", "Bash", Some(bytes)));
            for (sequence, data) in chunks {
                receive(&mut state, chunk("invalid", sequence, data));
            }
            receive(&mut state, complete("invalid", count));
            assert_eq!(
                stdin(&state, "review invalid", None).1,
                "  receiving exact input…\n"
            );
            assert!(state.requests["invalid"].ready.is_none());
            // A new issuance resets the failed transfer and is a positive control.
            deliver(
                &mut state,
                "invalid",
                "Bash",
                &json!({"command": "RECOVERED"}),
            );
            assert!(
                stdin(&state, "review invalid", None)
                    .1
                    .contains("│ command: RECOVERED\n")
            );
        }
    }

    #[test]
    fn closed_and_not_pending_retire_but_other_refusals_keep_exact_input() {
        for reason in [
            ApprovalRefusal::WriterOnly,
            ApprovalRefusal::IssuedElsewhere,
        ] {
            let mut state = PlainApprovals::new(true);
            deliver(
                &mut state,
                "keep",
                "Bash",
                &json!({"command": "still pending"}),
            );
            assert_eq!(
                receive(
                    &mut state,
                    DaemonFrame::ApprovalRefused {
                        request_id: RequestId("keep".into()),
                        reason,
                    }
                ),
                format!("{reason}\n")
            );
            assert!(
                stdin(&state, "review keep", None)
                    .1
                    .contains("│ command: still pending\n")
            );
        }
        for ready in [false, true] {
            for closed in [false, true] {
                let mut state = PlainApprovals::new(true);
                if ready {
                    deliver(&mut state, "retire", "Bash", &json!({"command": "secret"}));
                } else {
                    receive(&mut state, request("retire", "Bash", Some(2)));
                    receive(&mut state, chunk("retire", 0, "{"));
                }
                let retirement = if closed {
                    DaemonFrame::ApprovalClosed {
                        request_id: RequestId("retire".into()),
                    }
                } else {
                    DaemonFrame::ApprovalRefused {
                        request_id: RequestId("retire".into()),
                        reason: ApprovalRefusal::NoLongerPending,
                    }
                };
                receive(&mut state, retirement);
                receive(&mut state, chunk("retire", 1, "}"));
                receive(&mut state, complete("retire", 2));
                assert_eq!(
                    stdin(&state, "review retire", None).1,
                    "No exact input available for retire.\n"
                );
            }
        }
    }

    fn snapshot(retained_approvals: usize) -> AttachSnapshot {
        AttachSnapshot {
            conversation_id: "conversation".into(),
            transcript: vec![],
            permission_mode: PermissionMode::Normal,
            channels: vec![],
            blocked_actions_waiting: 2,
            pending_consent_cards: 0,
            retained_approvals,
            peer_row_controls: Default::default(),
            peer_row_revision: 0,
            recipient_context_ids: vec![],
            withheld_tool_inputs: vec![WithheldToolInput {
                message_id: "message".into(),
                tool_call_id: "tool".into(),
                withheld: ToolInputWithheld::PeerTier,
            }],
        }
    }

    #[test]
    fn read_only_snapshot_has_passive_retained_count_and_never_accepts_card_or_chunks() {
        let mut output = Vec::new();
        print_snapshot(&snapshot(3), AttachMode::ReadOnly, &mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "input withheld — not shared with peer attachments\n\
             ⚠ 2 action(s) waiting on you (denied while no client was attached).\n\
             3 approval(s) waiting for the writer\n\
             — attached (read-only; another client holds the writer). —\n"
        );
        for (count, mode) in [(0, AttachMode::ReadOnly), (3, AttachMode::ReadWrite)] {
            let mut output = Vec::new();
            print_snapshot(&snapshot(count), mode, &mut output).unwrap();
            assert!(!String::from_utf8(output).unwrap().contains("approval(s)"));
        }
        let mut state = PlainApprovals::new(false);
        assert!(receive(&mut state, request("private", "Bash", Some(2))).is_empty());
        assert!(receive(&mut state, chunk("private", 0, "{}")).is_empty());
        assert!(receive(&mut state, complete("private", 1)).is_empty());
        assert!(state.requests.is_empty());
        assert_eq!(
            stdin(&state, "review private", None).1,
            "No exact input available for private.\n"
        );
    }

    #[test]
    fn withheld_event_history_and_oversized_frame_print_host_notices() {
        let mut state = PlainApprovals::new(true);
        let event = crate::infrastructure::runtime::event_bus::RawEvent {
            conversation_id: None,
            timestamp_ms: 1,
            kind: RawEventKind::Provider(StreamChunk::Text {
                content: "not an argument notice".into(),
                parent_tool_use_id: None,
            }),
        };
        assert_eq!(
            receive(
                &mut state,
                DaemonFrame::ToolEventWithheld {
                    event,
                    withheld: ToolInputWithheld::TooLarge { bytes: 2048 },
                }
            ),
            "input too large to show here (2.0 KiB)\n"
        );
        assert_eq!(
            receive(
                &mut state,
                DaemonFrame::History {
                    messages: vec![],
                    has_more: false,
                    withheld_tool_inputs: snapshot(0).withheld_tool_inputs,
                }
            ),
            "input withheld — not shared with peer attachments\n"
        );
        assert_eq!(
            receive(
                &mut state,
                DaemonFrame::FrameTooLarge {
                    frame: "History".into(),
                    bytes: 4096,
                }
            ),
            "History too large to show here (4.0 KiB)\n"
        );
    }
}

/// Story 19.21c: the line client prints only complete ordered rendered views.
/// A broken stream stays poisoned (`None`) until its done boundary, so a
/// repeated `sequence == 0` can never restart it.
#[derive(Default)]
struct PlainTeamViews(std::collections::HashMap<String, Option<(u32, String)>>);
impl PlainTeamViews {
    fn chunk(&mut self, request: String, sequence: u32, view: &str) {
        let slot = self
            .0
            .entry(request)
            .or_insert_with(|| (sequence == 0).then_some((0, String::new())));
        match slot {
            Some((next, assembled)) if *next == sequence && sequence < u32::MAX => {
                assembled.push_str(view);
                *next += 1;
            }
            _ => *slot = None,
        }
    }
    fn done(
        &mut self,
        request: &str,
        chunks: u32,
        output: &mut impl std::io::Write,
    ) -> std::io::Result<()> {
        if let Some(Some((received, view))) = self.0.remove(request)
            && received == chunks
        {
            writeln!(output, "{view}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod team_view_tests {
    use super::*;
    #[test]
    fn plain_prints_complete_views_but_not_incomplete_or_out_of_order_streams() {
        let mut views = PlainTeamViews::default();
        let mut output = Vec::new();
        views.chunk("complete".into(), 0, "thread · 界\n");
        views.chunk("complete".into(), 1, "reply");
        views.done("complete", 2, &mut output).unwrap();
        assert_eq!(
            String::from_utf8(output.clone()).unwrap(),
            "thread · 界\nreply\n"
        );
        views.chunk("short".into(), 0, "never print");
        views.done("short", 2, &mut output).unwrap();
        views.chunk("unordered".into(), 1, "never print");
        views.done("unordered", 1, &mut output).unwrap();
        views.chunk("gap".into(), 0, "never print");
        views.chunk("gap".into(), 2, "gap");
        views.done("gap", 3, &mut output).unwrap();
        views.chunk("restart".into(), 0, "never print");
        views.chunk("restart".into(), 0, "replacement");
        views.done("restart", 1, &mut output).unwrap();
        views.chunk("revived".into(), 0, "never print");
        views.chunk("revived".into(), 2, "gap");
        views.chunk("revived".into(), 0, "replacement");
        views.done("revived", 1, &mut output).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "thread · 界\nreply\n");
    }
}
