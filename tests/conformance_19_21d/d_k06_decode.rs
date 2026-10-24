//! A well-framed unknown client frame must not kill an attached session.
use super::attached_harness::{AttachedClient, AttachedDaemon};
use rustain::adapters::daemon::protocol::{
    ClientFrame, ConnectionTier, DaemonFrame, ProtocolError, TeamBoardRequest, read_frame,
};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

async fn raw(stream: &mut UnixStream, body: &[u8]) {
    stream
        .write_all(&(body.len() as u32).to_be_bytes())
        .await
        .unwrap();
    stream.write_all(body).await.unwrap();
}

#[tokio::test]
async fn k06_undecodable_frame_is_answered_and_session_continues() {
    let daemon = AttachedDaemon::start(vec![]).await;
    let mut client =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    let mut observer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, true).await;
    raw(&mut client.stream, br#"{"futureFrame":{}}"#).await;
    let answer = tokio::time::timeout(
        Duration::from_secs(10),
        read_frame::<_, DaemonFrame>(&mut client.stream),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        matches!(&answer, Some(DaemonFrame::Error(ProtocolError::Malformed(text))) if text == "this daemon could not decode a frame from this client; the frame was ignored"),
        "expected value-free decode refusal, got {answer:?}"
    );
    client
        .send_frame(ClientFrame::TeamBoard {
            request: "after-decode".into(),
            selector: TeamBoardRequest::Inventory,
        })
        .await;
    client
        .wait_board_result("after-decode")
        .await
        .expect("following board request answered");
    observer
        .send_frame(ClientFrame::TeamBoard {
            request: "observer-fence".into(),
            selector: TeamBoardRequest::Inventory,
        })
        .await;
    observer
        .wait_board_result("observer-fence")
        .await
        .expect("observer remains usable");
    assert!(
        !observer
            .received
            .iter()
            .any(|frame| matches!(frame, DaemonFrame::Error(ProtocolError::Malformed(_)))),
        "decode refusal must be origin-only"
    );
    observer.detach().await;
    client.detach().await;
    daemon.stop().await;
}

#[tokio::test]
async fn k06_zero_length_frame_closes_session() {
    let daemon = AttachedDaemon::start(vec![]).await;
    let mut client =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    raw(&mut client.stream, b"").await;
    let answer = tokio::time::timeout(
        Duration::from_secs(10),
        read_frame::<_, DaemonFrame>(&mut client.stream),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(answer.is_none(), "unresynchronizable frame must close");
    daemon.stop().await;
}

#[tokio::test]
async fn k06_undecodable_first_frame_is_answered_then_closed() {
    let daemon = AttachedDaemon::start(vec![]).await;
    let mut stream = UnixStream::connect(&daemon.path).await.unwrap();
    assert!(matches!(
        read_frame::<_, DaemonFrame>(&mut stream).await.unwrap(),
        Some(DaemonFrame::AttachChallenge { .. })
    ));
    raw(&mut stream, br#"{"futureFrame":{}}"#).await;
    let answer = tokio::time::timeout(
        Duration::from_secs(10),
        read_frame::<_, DaemonFrame>(&mut stream),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        matches!(&answer, Some(DaemonFrame::Error(ProtocolError::Malformed(text))) if text == "the first frame could not be decoded; attach with a client of the same version"),
        "expected handshake refusal, got {answer:?}"
    );
    let closed = tokio::time::timeout(
        Duration::from_secs(10),
        read_frame::<_, DaemonFrame>(&mut stream),
    )
    .await
    .expect("the daemon closes after the handshake refusal")
    .unwrap();
    assert!(closed.is_none(), "expected close, got {closed:?}");
    daemon.stop().await;
}
