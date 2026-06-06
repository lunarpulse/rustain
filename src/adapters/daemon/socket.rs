//! Daemon Unix socket (Story 12.1a Task 3).
//!
//! Scope discipline: in 12.1a the socket is **bound** and accepts connections
//! that it logs and immediately closes. The TUI-attach wire protocol is Story
//! 12.2 — this module deliberately does NOT design or read any framing. It owns
//! only bind / accept-stub / cleanup.

use std::path::Path;

use anyhow::{Context, Result};
use tokio::net::UnixListener;

/// Bind the daemon's `UnixListener`. Removes a stale socket file first (a leftover
/// from an unclean exit would otherwise make `bind` fail with `EADDRINUSE`).
///
/// Surfaces the AF_UNIX `sun_path` length limit (~108 bytes on Linux) as an
/// `io::Error` from `bind`; the hashed path from `paths::daemon_socket_path`
/// keeps us well under it (see that helper's docs).
pub fn bind(socket_path: &Path) -> Result<UnixListener> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating socket dir {}", parent.display()))?;
    }
    // A live daemon is guarded by the PID file (AC-12-1a-9); reaching here means
    // any existing socket file is a stale leftover safe to remove.
    let _ = std::fs::remove_file(socket_path);
    UnixListener::bind(socket_path)
        .with_context(|| format!("binding daemon socket {}", socket_path.display()))
}

/// Remove the socket file, ignoring "not found".
pub fn cleanup(socket_path: &Path) {
    let _ = std::fs::remove_file(socket_path);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bind_creates_socket_and_cleanup_removes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("d.sock");
        let listener = bind(&sock).unwrap();
        assert!(sock.exists());
        drop(listener);
        cleanup(&sock);
        assert!(!sock.exists());
    }

    #[tokio::test]
    async fn bind_reclaims_a_stale_socket_file() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("d.sock");
        std::fs::write(&sock, b"stale").unwrap(); // simulate leftover
        let _listener = bind(&sock).expect("bind must reclaim a stale socket file");
        assert!(sock.exists());
    }
}
