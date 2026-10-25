//! One-shot daemon client; no A2A composition or journal access lives here.

use crate::adapters::cli::team::send::{self as copy, SendRequest};
use crate::adapters::daemon::attach_client::{OneShotAttachError, attach_one_shot_writer};
use crate::adapters::daemon::protocol::{
    ClientFrame, DaemonFrame, ProtocolError, RecipientAction, RecipientActionOutcome,
    RecipientActionTarget, frame_too_large_notice, read_frame, write_frame,
};
use crate::adapters::tui::handlers::team_command::{TeamSendRow, render_team_send_rows};
use crate::domain::models::SendActionId;
use std::io::Write;
use std::path::Path;
use tokio::io::{AsyncRead, AsyncWrite};

const REQUEST: &str = "cli-send";

/// Production caller: the early Command::Team dispatch, before provider construction.
pub async fn run_cli_team_send(
    workspace: &Path,
    args: SendRequest,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let (mut reader, mut writer) = match attach_one_shot_writer(workspace).await {
        Ok(connection) => connection,
        Err(error) => {
            match &error {
                OneShotAttachError::NoDaemon => writeln!(out, "{}", copy::NO_DAEMON)?,
                OneShotAttachError::WriterHeld => writeln!(out, "{}", copy::WRITER_HELD)?,
                OneShotAttachError::VersionMismatch { daemon, client } => {
                    writeln!(out, "{}", copy::version_mismatch(*daemon, *client))?
                }
                OneShotAttachError::Rejected(_) | OneShotAttachError::Io(_) => {
                    writeln!(out, "{}", copy::attach_failed(&error))?
                }
            }
            return Err(error.into());
        }
    };
    run_team_send_over(&mut reader, &mut writer, args, out).await
}

fn print_rows(out: &mut impl Write, rows: &[TeamSendRow]) -> std::io::Result<()> {
    writeln!(out, "{}", render_team_send_rows(rows))
}

fn interrupted(
    out: &mut impl Write,
    rows: &[TeamSendRow],
    action: Option<&SendActionId>,
    error: impl std::fmt::Display,
) -> anyhow::Result<()> {
    print_rows(out, rows)?;
    writeln!(out, "{}", copy::incomplete(action))?;
    anyhow::bail!("{error}")
}

async fn run_team_send_over<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    args: SendRequest,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let frame = ClientFrame::TeamSend {
        request: REQUEST.to_owned(),
        recipients: args.recipients,
        text: args.text,
        message_type: args.message_type,
        subject: args.subject,
        stories: args.stories,
        holders_of: None,
    };
    let submitted = write_frame(writer, &frame).await;
    // Reuse the recipient strings after serialization instead of copying them.
    let ClientFrame::TeamSend { recipients, .. } = frame else {
        unreachable!()
    };
    let mut rows: Vec<_> = recipients
        .into_iter()
        .map(|alias| TeamSendRow {
            alias,
            outcome: None,
        })
        .collect();
    if let Err(error) = submitted {
        return interrupted(out, &rows, None, error);
    }
    let mut pending = rows.len();
    let mut action = None;
    loop {
        let frame = match read_frame::<_, DaemonFrame>(reader).await {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                return interrupted(out, &rows, action.as_ref(), "daemon closed the connection");
            }
            Err(error) => return interrupted(out, &rows, action.as_ref(), error),
        };
        match frame {
            DaemonFrame::TeamSendStarted {
                request,
                action: started_action,
                ..
            } if request == REQUEST => {
                if action.is_none() {
                    writeln!(out, "{}", copy::started(rows.len(), &started_action))?;
                    out.flush()?;
                    action = Some(started_action);
                }
            }
            DaemonFrame::TeamSendSettled {
                request,
                index,
                outcome,
            } if request == REQUEST => {
                let Some(row) = rows.get_mut(index) else {
                    print_rows(out, &rows)?;
                    anyhow::bail!("protocol fault: settlement index {index} is out of range");
                };
                if row.outcome.is_some() {
                    print_rows(out, &rows)?;
                    anyhow::bail!("protocol fault: duplicate settlement index {index}");
                }
                row.outcome = Some(outcome.into());
                pending -= 1;
                if pending == 0 {
                    print_rows(out, &rows)?;
                    let _ = write_frame(writer, &ClientFrame::Detach).await;
                    if rows
                        .iter()
                        .all(|row| row.outcome.as_ref().is_some_and(copy::reached))
                    {
                        return Ok(());
                    }
                    anyhow::bail!(copy::NOT_EVERY_RECIPIENT);
                }
            }
            DaemonFrame::RecipientActionResult {
                action: RecipientAction::TeamSend,
                target: RecipientActionTarget::Request { request },
                outcome: RecipientActionOutcome::Refused { error },
            } if request == REQUEST => {
                let reason = match error {
                    ProtocolError::Malformed(reason) | ProtocolError::Internal(reason) => reason,
                    other => other.to_string(),
                };
                // Once the send started or any row settled, a refusal cannot mean nothing was sent.
                if action.is_some() || pending < rows.len() {
                    return interrupted(
                        out,
                        &rows,
                        action.as_ref(),
                        format!("protocol fault: refusal after the send began: {reason}"),
                    );
                }
                writeln!(out, "{}", copy::request_refused(&reason))?;
                anyhow::bail!("{reason}");
            }
            // Send frames go to the origin connection only, so an oversized one is this request's.
            DaemonFrame::FrameTooLarge { frame, bytes }
                if frame == "TeamSendStarted" || frame == "TeamSendSettled" =>
            {
                return interrupted(
                    out,
                    &rows,
                    action.as_ref(),
                    frame_too_large_notice(&frame, bytes),
                );
            }
            DaemonFrame::Error(error) => return interrupted(out, &rows, action.as_ref(), error),
            // Writer handoff can replay notices/cards; none is this send's result.
            _ => {}
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::daemon::protocol::{TeamSendCause, TeamSendCauseKind, TeamSendOutcome};
    use crate::domain::models::{RecipientOutcome, SendActionId};

    fn request() -> SendRequest {
        SendRequest {
            recipients: vec!["alice".into(), "bob".into()],
            text: "hello".into(),
            message_type: None,
            subject: None,
            stories: None,
        }
    }
    fn settled(index: usize, outcome: TeamSendOutcome) -> DaemonFrame {
        DaemonFrame::TeamSendSettled {
            request: REQUEST.into(),
            index,
            outcome,
        }
    }
    fn start() -> DaemonFrame {
        DaemonFrame::TeamSendStarted {
            recipients: Vec::new(),
            request: REQUEST.into(),
            action: SendActionId::from_replay("sa_test"),
        }
    }
    fn refusal(action: RecipientAction, request: &str) -> DaemonFrame {
        DaemonFrame::RecipientActionResult {
            action,
            target: RecipientActionTarget::Request {
                request: request.into(),
            },
            outcome: RecipientActionOutcome::Refused {
                error: ProtocolError::Malformed("no A2A peer `ghost` in this workspace".into()),
            },
        }
    }
    async fn scripted(frames: Vec<DaemonFrame>, complete: bool) -> (anyhow::Result<()>, String) {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();
        let task = tokio::spawn(async move {
            let (mut reader, mut writer) = server.into_split();
            assert!(
                matches!(read_frame::<_, ClientFrame>(&mut reader).await.unwrap(), Some(ClientFrame::TeamSend { request, recipients, text, .. }) if request == REQUEST && recipients == ["alice", "bob"] && text == "hello")
            );
            for frame in frames {
                write_frame(&mut writer, &frame).await.unwrap();
            }
            if complete {
                assert!(matches!(
                    read_frame::<_, ClientFrame>(&mut reader).await.unwrap(),
                    Some(ClientFrame::Detach)
                ));
            }
        });
        let (mut reader, mut writer) = client.into_split();
        let mut output = Vec::new();
        let result = run_team_send_over(&mut reader, &mut writer, request(), &mut output).await;
        task.await.unwrap();
        (result, String::from_utf8(output).unwrap())
    }
    #[tokio::test]
    async fn send_loop_correlates_and_orders_rows_and_prints_started_once() {
        let mut foreign = settled(99, TeamSendOutcome::Delivered);
        if let DaemonFrame::TeamSendSettled { request, .. } = &mut foreign {
            *request = "other".into();
        }
        let (result, output) = scripted(
            vec![
                refusal(RecipientAction::TeamBoard, REQUEST),
                refusal(RecipientAction::TeamSend, "other"),
                foreign,
                DaemonFrame::FrameTooLarge {
                    frame: "History".into(),
                    bytes: 100,
                },
                start(),
                start(),
                settled(1, TeamSendOutcome::AcceptedWithoutItem),
                settled(0, TeamSendOutcome::Delivered),
            ],
            true,
        )
        .await;
        result.unwrap();
        assert_eq!(output.matches("Sending to").count(), 1);
        let expected = render_team_send_rows(&[
            TeamSendRow {
                alias: "alice".into(),
                outcome: Some(RecipientOutcome::Delivered),
            },
            TeamSendRow {
                alias: "bob".into(),
                outcome: Some(RecipientOutcome::AcceptedWithoutItem),
            },
        ]);
        assert_eq!(
            output,
            format!(
                "{}\n{expected}\n",
                copy::started(2, &SendActionId::from_replay("sa_test"))
            )
        );
    }
    #[tokio::test]
    async fn send_loop_keyed_refusal_uses_inner_reason() {
        let (result, output) =
            scripted(vec![refusal(RecipientAction::TeamSend, REQUEST)], false).await;
        assert!(result.is_err());
        assert_eq!(
            output,
            "Nothing was sent: no A2A peer `ghost` in this workspace\n"
        );
    }
    #[tokio::test]
    async fn send_loop_all_not_sent_without_started_is_valid() {
        let (result, output) = scripted(
            vec![
                settled(1, TeamSendOutcome::NotSent),
                settled(0, TeamSendOutcome::NotSent),
            ],
            true,
        )
        .await;
        assert_eq!(result.unwrap_err().to_string(), copy::NOT_EVERY_RECIPIENT);
        assert!(!output.contains("stopped answering"));
        assert!(!output.contains("Sending to"));
        assert_eq!(output.matches("not sent").count(), 2);
    }
    #[tokio::test]
    async fn send_loop_every_recipient_must_be_reached() {
        for outcome in [
            TeamSendOutcome::Declined,
            TeamSendOutcome::AskedQuestion {
                task_id: "t".into(),
            },
            TeamSendOutcome::NoUsableAnswer,
            TeamSendOutcome::Unreachable {
                cause: Some(TeamSendCause {
                    kind: TeamSendCauseKind::Ratified,
                    text: "offline".into(),
                }),
            },
        ] {
            let (result, _) = scripted(
                vec![
                    start(),
                    settled(0, TeamSendOutcome::AwaitingApproval),
                    settled(1, outcome),
                ],
                true,
            )
            .await;
            assert_eq!(result.unwrap_err().to_string(), copy::NOT_EVERY_RECIPIENT);
        }
    }
    #[tokio::test]
    async fn send_loop_duplicate_and_out_of_range_are_faults_with_rows() {
        for index in [0, 2] {
            let (result, output) = scripted(
                vec![
                    settled(0, TeamSendOutcome::Delivered),
                    settled(index, TeamSendOutcome::Delivered),
                ],
                false,
            )
            .await;
            assert!(result.unwrap_err().to_string().contains("protocol fault"));
            assert!(output.contains("delivered"));
            assert!(output.contains("sending…"));
        }
    }
    #[tokio::test]
    async fn send_loop_disconnects_never_claim_nothing_was_sent() {
        for started in [false, true] {
            for prior_row in [false, true] {
                for error in [false, true] {
                    let mut frames = Vec::new();
                    if started {
                        frames.push(start());
                    }
                    if prior_row {
                        frames.push(settled(0, TeamSendOutcome::NotSent));
                    }
                    if error {
                        frames.push(DaemonFrame::Error(ProtocolError::Internal("closed".into())));
                    }
                    let (result, output) = scripted(frames, false).await;
                    assert!(result.is_err());
                    assert!(!output.contains("Nothing was sent"));
                    assert!(!output.contains("so nothing was sent"));
                    if started {
                        assert!(output.contains("'/team board --action sa_test' shows the rest."));
                    } else {
                        assert!(output.ends_with(&format!("{}\n", copy::OUTCOME_UNKNOWN)));
                    }
                    if prior_row {
                        assert!(output.contains("not sent"));
                    }
                }
            }
        }
    }
    #[tokio::test]
    async fn send_loop_late_refusal_and_oversized_send_frame_are_uncertain() {
        for (frames, board) in [
            (
                vec![start(), refusal(RecipientAction::TeamSend, REQUEST)],
                true,
            ),
            (
                vec![
                    settled(0, TeamSendOutcome::NotSent),
                    refusal(RecipientAction::TeamSend, REQUEST),
                ],
                false,
            ),
            (
                vec![
                    start(),
                    DaemonFrame::FrameTooLarge {
                        frame: "TeamSendSettled".into(),
                        bytes: 9_000_000,
                    },
                ],
                true,
            ),
            (
                vec![DaemonFrame::FrameTooLarge {
                    frame: "TeamSendStarted".into(),
                    bytes: 9_000_000,
                }],
                false,
            ),
        ] {
            let (result, output) = scripted(frames, false).await;
            assert!(result.is_err());
            assert!(!output.contains("Nothing was sent"), "{output}");
            if board {
                assert!(
                    output.ends_with("'/team board --action sa_test' shows the rest.\n"),
                    "{output}"
                );
            } else {
                assert!(
                    output.ends_with(&format!("{}\n", copy::OUTCOME_UNKNOWN)),
                    "{output}"
                );
            }
        }
    }
    #[tokio::test]
    async fn send_loop_missing_settlement_keeps_waiting_and_decode_failure_is_uncertain() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        write_frame(&mut server, &settled(0, TeamSendOutcome::NotSent))
            .await
            .unwrap();
        let (mut reader, mut writer) = tokio::io::split(&mut client);
        let mut output = Vec::new();
        {
            let future = run_team_send_over(&mut reader, &mut writer, request(), &mut output);
            tokio::pin!(future);
            assert!(
                futures::poll!(&mut future).is_pending(),
                "no terminal frame means keep waiting"
            );
            server.write_all(&[0, 0, 0, 1, b'{']).await.unwrap();
            assert!(future.await.is_err());
        }
        assert!(
            String::from_utf8(output)
                .unwrap()
                .ends_with(&format!("{}\n", copy::OUTCOME_UNKNOWN))
        );
    }
    #[test]
    fn semantic_send_flags_match_the_attached_usage() {
        use clap::CommandFactory;
        use std::collections::BTreeSet;
        let cli = crate::adapters::cli::commands::Cli::command();
        let command = cli
            .find_subcommand("team")
            .unwrap()
            .find_subcommand("send")
            .unwrap();
        let flags: BTreeSet<_> = command
            .get_arguments()
            .filter_map(|arg| arg.get_long())
            .filter(|flag| *flag != "help")
            .collect();
        let clause = crate::adapters::tui::handlers::team_command::USAGE
            .split(" | ")
            .find(|clause| clause.starts_with("/team send "))
            .unwrap();
        let attached: BTreeSet<_> = clause
            .split_whitespace()
            .filter_map(|token| token.strip_prefix("[--"))
            .collect();
        assert_eq!(flags, attached);
    }
}
