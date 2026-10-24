//! Story 19.20 K02: a typed bug report, attached acknowledgement and human fix reply.
use super::harness::Sender;
use super::k07_k08_recipient::{Recipient, acknowledge, frame, local_view};
use futures::stream::BoxStream;
use rustain::adapters::daemon::protocol::{
    ClientFrame, DaemonFrame, RecipientAction, RecipientActionOutcome, RecipientActionTarget,
    write_frame,
};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::errors::ProviderError;
use rustain::domain::events::AppEvent;
use rustain::domain::models::{
    CompletionOptions, JournalRecord, Message, ResponseMode, RoomEvent, SemanticMessageType,
    StreamChunk,
};
use rustain::domain::ports::{ProbeOutcome, StreamingProvider};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const IO_TIMEOUT: Duration = Duration::from_secs(45);
const BUG: &str = "Session token not refreshed on OAuth→password switch";
const FIX: &str = "Fix for your bug report: PR #251 committed. Ready for re-test.";

struct UnexpectedProvider(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl StreamingProvider for UnexpectedProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("the Act 5 handoff must never invoke the sender provider")
    }

    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }

    fn provider_id(&self) -> String {
        "act5-no-provider-turns".to_owned()
    }

    fn list_models(&self) -> Vec<rustain::domain::models::provider::ModelDescriptor> {
        Vec::new()
    }

    async fn health_check(&self) -> Result<(), ProviderError> {
        Ok(())
    }

    async fn connectivity_probe(&self) -> Result<ProbeOutcome, ProviderError> {
        Ok(ProbeOutcome {
            latency: Duration::ZERO,
        })
    }
}

async fn board(sender: &mut Sender, state: &mut TuiState) -> String {
    sender.command(state, "/team board").await;
    tokio::time::timeout(IO_TIMEOUT, async {
        loop {
            match sender.domain_rx.recv().await {
                Some(AppEvent::TeamBoardReady { message, .. }) => return message,
                Some(_) => {}
                None => panic!("sender event channel closed before board render"),
            }
        }
    })
    .await
    .expect("production board command completes")
}

#[tokio::test]
async fn k02_bug_report_acknowledgement_and_fix_reply_cross_real_hosts_without_provider_turns() {
    let recipient = Recipient::start(ResponseMode::NotifyAndWait).await;
    let mut sender = Sender::start(vec![recipient.peer("tom")]).await;
    let sender_turns = Arc::new(AtomicUsize::new(0));
    sender.app_state.provider.store(Arc::new(
        Arc::new(UnexpectedProvider(sender_turns.clone())) as Arc<dyn StreamingProvider>
    ));
    let mut state = TuiState::new(130, 30);
    sender
        .command(
            &mut state,
            &format!("/team send --type bug_report tom {BUG}"),
        )
        .await;
    sender.settle(&mut state, 1).await;

    let item = tokio::time::timeout(IO_TIMEOUT, async {
        loop {
            if let Some(item) =
                recipient
                    .journal
                    .entries()
                    .await
                    .iter()
                    .find_map(|entry| match &entry.record {
                        JournalRecord::Room(RoomEvent::RecipientItemReceived {
                            address,
                            content,
                            message_type,
                            ..
                        }) if content == BUG => {
                            assert_eq!(*message_type, SemanticMessageType::BugReport);
                            Some(address.item().as_str().to_owned())
                        }
                        _ => None,
                    })
            {
                return item;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("real recipient ingress journals the exact bug report");

    let before = local_view(recipient.journal.workspace.path(), &item)
        .await
        .recipient_surface
        .expect("local View opens the received bug report");
    assert_eq!(before.message_type, SemanticMessageType::BugReport);
    assert_eq!(before.item_state, Some("received"));
    assert!(
        before.content.contains("bug_report ·"),
        "{}",
        before.content
    );
    assert!(before.content.contains(BUG), "{}", before.content);
    assert!(!before.content.contains("[agent-composed]"));
    assert!(!before.content.contains("[auto-sent]"));
    let delivered = board(&mut sender, &mut state).await;
    assert!(delivered.contains("● tom  delivered"), "{delivered}");

    let mut attached = recipient.attach(false).await;
    acknowledge(&mut attached, &item).await;
    write_frame(
        &mut attached,
        &ClientFrame::ReplyToRecipientItem {
            item_id: item.clone(),
            text: FIX.to_owned(),
        },
    )
    .await
    .expect("send the exact human fix through the attached socket");
    tokio::time::timeout(IO_TIMEOUT, async {
        loop {
            match frame(&mut attached).await {
                DaemonFrame::RecipientActionResult {
                    action,
                    target,
                    outcome,
                } => {
                    assert_eq!(action, RecipientAction::Reply);
                    assert_eq!(
                        target,
                        RecipientActionTarget::Item {
                            item_id: item.clone()
                        }
                    );
                    assert!(matches!(
                        outcome,
                        RecipientActionOutcome::Applied { reply: Some(_) }
                    ));
                    break;
                }
                DaemonFrame::Error(error) => panic!("attached fix reply refused: {error:?}"),
                _ => {}
            }
        }
    })
    .await
    .expect("attached reply receives its durable action result");

    // Use a fresh reader so the board cannot reuse the pre-ack pull cache.
    let mut reader = Sender::with_journal_and_clock(
        vec![recipient.peer("tom")],
        sender.journal.room.clone(),
        sender.journal.clone(),
        sender.clock.clone(),
    )
    .await;
    reader.app_state.provider.store(Arc::new(
        Arc::new(UnexpectedProvider(sender_turns.clone())) as Arc<dyn StreamingProvider>
    ));
    let acknowledged = board(&mut reader, &mut state).await;
    assert!(
        acknowledged.contains("✓ tom  acknowledged"),
        "{acknowledged}"
    );
    let item_row = acknowledged
        .lines()
        .find(|line| line.contains(&item))
        .expect("board exposes the correlated recipient item");
    assert!(item_row.contains("1 replies"), "{item_row}");

    reader
        .command(&mut state, &format!("/team thread tom {item}"))
        .await;
    let thread = tokio::time::timeout(IO_TIMEOUT, async {
        loop {
            match reader.domain_rx.recv().await {
                Some(AppEvent::TeamThreadReady { message, .. }) => return message,
                Some(_) => {}
                None => panic!("sender event channel closed before thread render"),
            }
        }
    })
    .await
    .expect("production thread command reads the fix from the recipient host");
    assert!(thread.contains(FIX), "{thread}");

    let after = local_view(recipient.journal.workspace.path(), &item)
        .await
        .recipient_surface
        .expect("recipient View opens the acknowledged thread");
    assert_eq!(after.item_state, Some("acknowledged"));
    assert!(
        after.content.contains("you · human-written · answer"),
        "{}",
        after.content
    );
    assert!(after.content.contains(FIX), "{}", after.content);
    assert_eq!(*recipient.requests.lock(), 0, "recipient provider turns");
    assert_eq!(
        sender_turns.load(Ordering::SeqCst),
        0,
        "sender provider turns"
    );
    drop(attached);
    recipient.stop().await;
}
