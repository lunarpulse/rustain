use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::DomainInputEvent;

/// AC4: Event processing sets needs_redraw flag.
// Covers: NFR2 (redraw efficiency)
#[test]
fn test_tick_needs_redraw_flag() {
    let mut state = TuiState::new(80, 24);

    // Initially needs_redraw is true (first render)
    assert!(state.needs_redraw);

    // Simulate clearing after render
    state.needs_redraw = false;
    assert!(!state.needs_redraw);

    // Input event should set needs_redraw
    rustain::adapters::tui::app::handle_input(&mut state, &DomainInputEvent::KeyPress('a'));
    assert!(state.needs_redraw);
}

/// AC4: Resize events update terminal dimensions and trigger redraw.
// Covers: NFR2 (redraw efficiency)
#[test]
fn test_resize_event_updates_state() {
    let mut state = TuiState::new(80, 24);
    state.needs_redraw = false;

    rustain::adapters::tui::app::handle_input(&mut state, &DomainInputEvent::Resize(120, 40));

    assert_eq!(state.terminal_width, 120);
    assert_eq!(state.terminal_height, 40);
    assert!(state.needs_redraw);
}

// ── Task 19.9: PermissionQueue batch sweep (AC4 "Session-allow sweeps queue") ──
//
// When the user chooses session-allow on a prompt, the event loop drains every
// queued request for the same tool and auto-responds Allow. This test exercises
// the two primitives that back that behavior: `drain_matching` removes exactly
// the matching entries (preserving unrelated ones), and sending on the oneshot
// actually reaches the waiting side.

#[tokio::test]
async fn test_permission_queue_batch_sweep_drains_matching_and_responds() {
    use rustain::adapters::tui::state::{PendingPermission, PermissionQueue};
    use rustain::domain::models::ApprovalDecision;

    let mut queue = PermissionQueue::default();

    let (bash_tx_1, bash_rx_1) = tokio::sync::oneshot::channel();
    let (bash_tx_2, bash_rx_2) = tokio::sync::oneshot::channel();
    let (write_tx, mut write_rx) = tokio::sync::oneshot::channel();
    let (bash_tx_3, bash_rx_3) = tokio::sync::oneshot::channel();

    queue.push(PendingPermission {
        tool_name: "Bash".to_string(),
        tool_input: serde_json::json!({"command": "ls"}),
        response_tx: bash_tx_1,
    });
    queue.push(PendingPermission {
        tool_name: "Bash".to_string(),
        tool_input: serde_json::json!({"command": "pwd"}),
        response_tx: bash_tx_2,
    });
    queue.push(PendingPermission {
        tool_name: "Write".to_string(),
        tool_input: serde_json::json!({"file_path": "a.rs"}),
        response_tx: write_tx,
    });
    queue.push(PendingPermission {
        tool_name: "Bash".to_string(),
        tool_input: serde_json::json!({"command": "echo"}),
        response_tx: bash_tx_3,
    });
    assert_eq!(queue.len(), 4);

    // Session-allow sweep for "Bash"
    let drained = queue.drain_matching("Bash");
    assert_eq!(drained.len(), 3, "All three Bash entries must be drained");
    assert_eq!(queue.len(), 1, "Write entry must remain in the queue");

    // Event loop sends Allow on each drained oneshot
    for queued in drained {
        let _ = queued.response_tx.send(ApprovalDecision::Allow);
    }

    assert_eq!(bash_rx_1.await.unwrap(), ApprovalDecision::Allow);
    assert_eq!(bash_rx_2.await.unwrap(), ApprovalDecision::Allow);
    assert_eq!(bash_rx_3.await.unwrap(), ApprovalDecision::Allow);

    // Write request is still pending — its receiver is still live, not resolved
    assert!(
        matches!(
            write_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "Write request must not have been swept"
    );
}
