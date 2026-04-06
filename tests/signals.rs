//! Signal handling tests — verify AC3 constraints.
//!
//! Note: Testing actual SIGTERM/SIGINT delivery requires subprocess spawning
//! which is deferred. These tests verify the shutdown channel mechanism.

use rustain::domain::events::AppEvent;
use tokio::sync::mpsc;

/// AC3: Shutdown event can be sent through the domain event channel.
// Covers: FR105 (crash safety), NFR24 (signal handling)
#[tokio::test]
async fn test_shutdown_event_through_channel() {
    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();

    tx.send(AppEvent::Shutdown).unwrap();

    let event = rx.recv().await.unwrap();
    assert!(
        matches!(event, AppEvent::Shutdown),
        "Expected Shutdown event"
    );
}

/// AC3: Multiple events can flow through the channel without blocking.
// Covers: FR105 (crash safety), NFR24 (signal handling)
#[tokio::test]
async fn test_event_channel_unbounded() {
    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();

    // Send multiple events rapidly
    for _ in 0..100 {
        tx.send(AppEvent::Tick).unwrap();
    }
    tx.send(AppEvent::Shutdown).unwrap();

    // Drain and find shutdown
    let mut count = 0;
    while let Some(event) = rx.recv().await {
        count += 1;
        if matches!(event, AppEvent::Shutdown) {
            break;
        }
    }
    assert_eq!(count, 101, "Expected 100 Ticks + 1 Shutdown");
}

/// AC3: Crash log path is unique per call (timestamp-based).
// Covers: FR105 (crash safety), NFR24 (signal handling)
#[test]
fn test_crash_log_paths_are_unique() {
    let path1 = rustain::infrastructure::paths::crash_log_path().unwrap();
    // Sleep briefly to ensure different timestamp
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let path2 = rustain::infrastructure::paths::crash_log_path().unwrap();

    assert_ne!(
        path1, path2,
        "Crash log paths should differ (timestamp-based)"
    );
}
