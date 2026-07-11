//! Story 17.1a — Task 5: real-process Unix-socket peer harness.
//!
//! The only test that exercises the signed peer boundary end-to-end through a
//! REAL `rustain` process. It spawns the built binary as a foreground daemon,
//! answers the server-first challenge-proof Attach handshake over a real Unix
//! socket with an `IdentityKeyStore`-provisioned Ed25519 key, sends a signed
//! `PeerEnvelope`, and asserts `PeerAccepted`. It then drives three mutation
//! classes against the SAME live daemon and asserts each is rejected as
//! `DaemonFrame::Error(ProtocolError::PeerVerification)` with zero side effect:
//!
//!   (a) a flipped signature byte  → `BadSignature`
//!   (b) `not_after` in the past   → `Expired` (a `MockClock` TTL advance is
//!       impossible against a real process, so a past wall-clock timestamp
//!       exercises the real `SystemClock` expiry path instead)
//!   (c) a replayed sequence       → `Replay`
//!
//! "Zero side effect" is proven positively: after each tamper rejection the
//! SAME sequence number is accepted when re-signed validly, so the rejection
//! consumed no replay-window slot and left the connection usable.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use assert_cmd::cargo::cargo_bin;
use rustain::adapters::daemon::protocol::{
    AttachMode, ClientFrame, ConnectionTier, DaemonFrame, PROTOCOL_VERSION, ProtocolError,
    read_frame, write_frame,
};
use rustain::adapters::rap::{AgentSigner, IdentityKeyStore, entry_hash};
use rustain::domain::clock::{Clock, SystemClock};
use rustain::domain::models::{AgentEnvelope, AgentId, CorrelationId, MessageKind};
use rustain::infrastructure::paths::workspace_hash;
use serde_json::{Value, json};
use tokio::net::UnixStream;
use tokio::process::Command;

/// The socket path the daemon binds: `{data_dir}/daemons/<workspace-hash>.sock`.
///
/// `workspace_hash` canonicalizes the path the same way the daemon does (the
/// daemon resolves `current_dir()` then hashes via this same function), so the
/// test and the real process always agree on the path.
fn daemon_socket_path(data_dir: &Path, workspace: &Path) -> PathBuf {
    let hash = workspace_hash(workspace);
    data_dir.join("daemons").join(format!("{hash}.sock"))
}

/// Spawn `rustain daemon start --foreground` pinned to isolated temp dirs.
///
/// `kill_on_drop` is the safety net for an early return or panic: the daemon is
/// reaped even if an assertion fails mid-handshake. `logging.rs` sends ALL
/// tracing to `{data_dir}/rustain.log` and ZERO bytes to stdout/stderr, so
/// stderr is left inherited purely to surface a startup panic in the test's
/// captured output.
fn spawn_foreground_daemon(
    workspace: &Path,
    data_dir: &Path,
    config_dir: &Path,
) -> std::io::Result<tokio::process::Child> {
    let mut cmd = Command::new(cargo_bin("rustain"));
    cmd.arg("daemon")
        .arg("start")
        .arg("--foreground")
        .current_dir(workspace)
        .env("RUSTAIN_DATA_DIR", data_dir)
        .env("RUSTAIN_CONFIG_DIR", config_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true);
    cmd.spawn()
}

/// Build a signed peer envelope rooted at `signer`'s PeerId (the peer-path
/// invariant the daemon enforces). `not_after` is wall-clock milliseconds — the
/// unit the daemon's verify seam compares against `SystemClock::wall_now_ms`.
fn peer_envelope(
    signer: &AgentSigner,
    sequence: u64,
    not_after: i64,
    nonce: &str,
    prev_hash: Vec<u8>,
    body: Value,
) -> Box<AgentEnvelope<Value>> {
    let sender = AgentId::from_peer_path(&format!("{}/agent", signer.identity().peer_id.as_str()))
        .expect("peer-rooted sender");
    Box::new(
        signer
            .sign(
                sender,
                AgentId::parse("daemon").expect("valid recipient"),
                CorrelationId::new("real-peer-corr"),
                MessageKind::PeerMessage,
                sequence,
                not_after,
                nonce.to_string(),
                prev_hash,
                body,
            )
            .expect("signing succeeds when sender is rooted at signer"),
    )
}

/// Capability and delivery receipts are asynchronous observability frames.
/// Protocol assertions consume them so each helper checks the response to the
/// frame it just sent rather than a queued event from the prior frame.
async fn read_non_event(stream: &mut UnixStream) -> Option<DaemonFrame> {
    loop {
        match read_frame::<_, DaemonFrame>(stream)
            .await
            .expect("read response")
        {
            Some(DaemonFrame::Event(_)) => continue,
            frame => return frame,
        }
    }
}

/// Send a signed `PeerEnvelope` and assert the daemon replies `PeerAccepted`
/// with the echoed `sequence`.
async fn expect_accepted(stream: &mut UnixStream, env: Box<AgentEnvelope<Value>>, want_seq: u64) {
    write_frame(stream, &ClientFrame::PeerEnvelope(env))
        .await
        .expect("write PeerEnvelope");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut accepted = false;
        let mut delivered = false;
        while !(accepted && delivered) {
            match read_frame::<_, DaemonFrame>(stream)
                .await
                .expect("read peer acceptance/receipt")
            {
                Some(DaemonFrame::PeerAccepted { sequence }) => {
                    assert_eq!(
                        sequence, want_seq,
                        "PeerAccepted must echo the envelope sequence"
                    );
                    accepted = true;
                }
                Some(DaemonFrame::Event(event))
                    if matches!(
                        event.kind,
                        rustain::infrastructure::runtime::event_bus::RawEventKind::Subagent(
                            rustain::domain::models::SubagentEnvelope {
                                event:
                                    rustain::domain::models::SubagentEvent::MessageDelivered {
                                        ..
                                    },
                                ..
                            }
                        )
                    ) =>
                {
                    delivered = true;
                }
                Some(DaemonFrame::Event(_)) => {}
                other => panic!(
                    "expected PeerAccepted and MessageDelivered in either order, got {other:?}"
                ),
            }
        }
    })
    .await
    .expect("peer acceptance/receipt sequence timed out");
}

/// Send a `PeerEnvelope` and assert the daemon rejects it with
/// `Error(ProtocolError::PeerVerification)` — the single rejection class for
/// every peer-envelope verification failure (bad signature, expiry, replay).
async fn expect_peer_verification(
    stream: &mut UnixStream,
    env: Box<AgentEnvelope<Value>>,
    label: &str,
) {
    write_frame(stream, &ClientFrame::PeerEnvelope(env))
        .await
        .expect("write PeerEnvelope");
    match read_non_event(stream).await {
        Some(DaemonFrame::Error(ProtocolError::PeerVerification(_))) => {}
        other => panic!("expected PeerVerification rejection for {label}, got {other:?}"),
    }
}

async fn expect_feed_fork(stream: &mut UnixStream, env: Box<AgentEnvelope<Value>>) {
    write_frame(stream, &ClientFrame::PeerEnvelope(env))
        .await
        .expect("write PeerEnvelope");
    assert!(matches!(
        read_non_event(stream).await,
        Some(DaemonFrame::Error(ProtocolError::FeedForkOrGap))
    ));
}

#[tokio::test]
async fn real_daemon_accepts_signed_envelope_and_rejects_mutations() {
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let data_dir = tempfile::tempdir().expect("data tempdir");
    let config_dir = tempfile::tempdir().expect("config tempdir");

    let socket = daemon_socket_path(data_dir.path(), workspace.path());

    // ── Spawn the real daemon (foreground = no detach; it stays alive for the
    //    whole test so a single process answers every assertion below).
    let mut child = spawn_foreground_daemon(workspace.path(), data_dir.path(), config_dir.path())
        .expect("spawn `rustain daemon start --foreground`");

    // ── Wait for the socket file (bound inside the lifecycle loop, just after
    //    the PID-file readiness marker). Poll + detect an early child exit so a
    //    startup failure is reported with the exit status rather than a hang.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if socket.exists() {
            break;
        }
        if let Ok(Some(status)) = child.try_wait() {
            panic!(
                "daemon exited before binding socket {} (status={status}); \
                 see captured stderr for the daemon's error",
                socket.display()
            );
        }
        if Instant::now() >= deadline {
            panic!(
                "daemon socket {} did not appear within 15s; see captured stderr",
                socket.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // ── Connect + perform the server-first challenge-proof Attach handshake.
    let mut stream = UnixStream::connect(&socket)
        .await
        .expect("connect to daemon socket");

    // 1. Read the daemon's AttachChallenge — a fresh 32-byte nonce.
    let nonce = match read_frame::<_, DaemonFrame>(&mut stream)
        .await
        .expect("read AttachChallenge")
    {
        Some(DaemonFrame::AttachChallenge { nonce }) => nonce,
        other => panic!("expected AttachChallenge from real daemon, got {other:?}"),
    };
    assert_eq!(nonce.len(), 32, "challenge nonce must be 32 random bytes");

    // 2. Provision a real identity key via the IdentityKeyStore (the same data
    //    dir the daemon runs under — a real on-disk Ed25519 key via OsRng, not
    //    a hardcoded test seed). The daemon (server) never touches this file;
    //    only client-side attach code loads it, so there is no race.
    let signer = IdentityKeyStore::new(data_dir.path())
        .load_or_generate()
        .expect("load or generate peer identity key");

    // 3. Build the proof-bearing Attach — an Ed25519 signature over the
    //    domain-separated transcript via AgentSigner::attach_proof — and send it.
    let read_only_ok = false;
    let tier = ConnectionTier::Peer;
    let proof = signer.attach_proof(&nonce, PROTOCOL_VERSION, tier.proof_tag(), read_only_ok);
    write_frame(
        &mut stream,
        &ClientFrame::Attach {
            protocol_version: PROTOCOL_VERSION,
            read_only_ok,
            tier,
            challenge_nonce: nonce,
            identity: signer.identity().clone(),
            proof,
        },
    )
    .await
    .expect("write proof-bearing Attach");

    // 4. Read AttachAck — the Peer tier is always granted read-only.
    match read_frame::<_, DaemonFrame>(&mut stream)
        .await
        .expect("read AttachAck")
    {
        Some(DaemonFrame::AttachAck { granted_mode, .. }) => {
            assert_eq!(granted_mode, AttachMode::ReadOnly, "peer tier is read-only");
        }
        other => panic!("expected AttachAck from real daemon, got {other:?}"),
    }

    // Wall-clock milliseconds — the unit the daemon's verify seam uses. The real
    // daemon reads SystemClock::wall_now_ms(); both processes share the host
    // clock, so a timestamp 60s in the past here is also past at the daemon
    // (the 60s margin dwarfs any sub-second verify latency).
    let now_ms = SystemClock::default().wall_now_ms();
    let past = now_ms - 60_000;

    // ── Positive control: a valid genesis envelope is accepted (seq=1). ─────
    let accepted1 = peer_envelope(
        &signer,
        1,
        i64::MAX,
        "n1",
        Vec::new(),
        json!({"msg": "hello peer"}),
    );
    let first_hash = entry_hash(&accepted1.header).unwrap();
    expect_accepted(&mut stream, accepted1, 1).await;

    // ── (a) Flipped signature bit → BadSignature → PeerVerification. ─────────
    let mut bad_sig = peer_envelope(
        &signer,
        2,
        i64::MAX,
        "n2",
        first_hash.clone(),
        json!({"msg": "tampered"}),
    );
    bad_sig.signature.0[0] ^= 0xFF;
    expect_peer_verification(&mut stream, bad_sig, "flipped signature byte").await;

    // Cryptographically valid but semantically invalid content must also roll
    // back its pending replay reservation. Otherwise an authenticated peer can
    // poison the feed with an undeliverable body and make seq=2 non-retriable.
    expect_peer_verification(
        &mut stream,
        peer_envelope(
            &signer,
            2,
            i64::MAX,
            "n2-invalid-body",
            first_hash.clone(),
            json!({"unexpected": true}),
        ),
        "verified frame rejected by semantic translation",
    )
    .await;

    // Zero side effect: the rejected seq=2 is still acceptable when validly
    // re-signed with the same predecessor.
    let accepted2 = peer_envelope(
        &signer,
        2,
        i64::MAX,
        "n2-fresh",
        first_hash.clone(),
        json!({"msg": "fresh 2"}),
    );
    let second_hash = entry_hash(&accepted2.header).unwrap();
    expect_accepted(&mut stream, accepted2, 2).await;

    // A valid signature and increasing sequence cannot skip the accepted head.
    // The dedicated protocol variant proves the chain rejection survives the
    // real daemon boundary and consumes no sequence/head state.
    expect_feed_fork(
        &mut stream,
        peer_envelope(
            &signer,
            3,
            i64::MAX,
            "n3-fork",
            first_hash,
            json!({"msg": "fork"}),
        ),
    )
    .await;

    // ── (b) not_after in the past → Expired → PeerVerification. ──────────────
    expect_peer_verification(
        &mut stream,
        peer_envelope(
            &signer,
            3,
            past,
            "n3",
            second_hash.clone(),
            json!({"msg": "stale"}),
        ),
        "expired not_after (past wall-clock)",
    )
    .await;

    // Zero side effect: the expired seq=3 is still acceptable when re-signed
    // with a future not_after and the same predecessor.
    let accepted3 = peer_envelope(
        &signer,
        3,
        i64::MAX,
        "n3-fresh",
        second_hash,
        json!({"msg": "fresh 3"}),
    );
    expect_accepted(&mut stream, accepted3.clone(), 3).await;

    // ── (c) Replayed sequence → Replay → PeerVerification. ───────────────────
    expect_peer_verification(&mut stream, accepted3, "replayed sequence").await;

    // ── Cleanly stop the daemon: kill + best-effort wait. `kill_on_drop` is the
    //    safety net if anything above panicked; the temp dirs reclaim any
    //    leftover socket/PID files.
    let _ = child.kill().await;
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
}
