//! Story C-3 (C3-R2-01, R3-01, R5-02) — an identity-safe, LEADER-ONLY handle on one
//! spawned MCP child.
//!
//! # Scope (Round 5 reduction)
//!
//! This stops the **direct child** only. Descendants of a wrapper (`sh -c`, `npx`,
//! a launcher that forks the real server) and processes that call `setsid` are NOT
//! stopped: process containment belongs to the sandbox work (DF
//! `DF-C-3-MCP-WRAPPER-DESCENDANTS-NOT-STOPPED`, owner 23-1). Children are
//! spawned exactly as before C-3 (no process group, inherited stderr).
//!
//! # Mechanism
//!
//! * The child is named by a **pidfd** (`pidfd_open`, Linux 5.3+; taken right after
//!   spawn while the child is unreaped). `pidfd_send_signal` can only reach that
//!   exact process, so a recycled pid is never signalled. No bare pid is ever
//!   passed to `kill(2)`, and nothing calls `waitpid`: reaping stays with the
//!   owning `Child`/rmcp transport/Tokio's reaper.
//! * Exit is observed by polling the pidfd (readable == exited).
//! * Every syscall outcome maps EXPLICITLY (never "gone by default"):
//!   - signal `Ok`, then pidfd readable within the bound → [`ForceStop::Killed`];
//!   - signal `ESRCH` (the process no longer exists) or pidfd already readable →
//!     [`ForceStop::AlreadyExited`];
//!   - any other signal error (`EPERM`, `EINVAL`, …) or a poll error → the child may
//!     still run: [`ForceStop::Unstopped`] and it stays tracked;
//!   - signalled but not observed to exit within the bound → [`ForceStop::Stuck`].
//! * No pidfd (acquisition failed, or not Linux) → [`ForceStop::Unstopped`]:
//!   NOTHING is signalled and the caller keeps the child tracked and logs it. What
//!   then happens to the process is only what its owner does: the owning `Child`
//!   carries `kill_on_drop(true)`, so when the transport/service is dropped Tokio
//!   initiates SIGKILL and attempts background reaping. Neither termination nor
//!   reaping is confirmed here; PID-file removal proceeds best-effort.
//!
//! Linux-only mechanisms are `cfg(target_os = "linux")`; every other target
//! compiles to the `Unstopped` stub.

use std::time::Duration;

/// Outcome of a [`ChildHandle::force_stop`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForceStop {
    /// The child had already exited (pidfd readable, or the signal found no process).
    AlreadyExited,
    /// SIGKILLed and observed to exit.
    Killed,
    /// SIGKILLed but no exit was observed within the bound.
    Stuck,
    /// The child could not be stopped here (no pidfd, or a syscall failed): it MAY
    /// STILL BE RUNNING and must stay tracked.
    Unstopped,
}

/// Whether the child has exited, as observed through the pidfd.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitState {
    Exited,
    Running,
    /// Not observable (no pidfd, or the poll failed with this errno; 0 = none).
    Unknown(i32),
}

/// How a `pidfd_send_signal(SIGKILL)` result is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalOutcome {
    /// Delivered: wait for the exit.
    Sent,
    /// `ESRCH`: the process no longer exists.
    NoSuchProcess,
    /// Anything else: the child may still run.
    Failed(i32),
}

/// Map a raw signal result (`Ok` or an errno) to its outcome. Pure, so the
/// mapping is testable with injected errors.
pub fn classify_signal(result: Result<(), i32>) -> SignalOutcome {
    match result {
        Ok(()) => SignalOutcome::Sent,
        Err(errno) if errno == esrch() => SignalOutcome::NoSuchProcess,
        Err(errno) => SignalOutcome::Failed(errno),
    }
}

/// Map a raw `poll(2)` result on the pidfd to an [`ExitState`]. Pure.
/// `rc` is the return value, `readable` whether `POLLIN` was set, `errno` the
/// error when `rc < 0`.
pub fn classify_poll(rc: i32, readable: bool, errno: i32) -> ExitState {
    if rc < 0 {
        ExitState::Unknown(errno)
    } else if rc > 0 && readable {
        ExitState::Exited
    } else if rc == 0 {
        ExitState::Running
    } else {
        // Readiness reported without POLLIN (POLLERR/POLLNVAL): not a clean exit.
        ExitState::Unknown(0)
    }
}

#[cfg(target_os = "linux")]
fn esrch() -> i32 {
    libc::ESRCH
}
#[cfg(not(target_os = "linux"))]
fn esrch() -> i32 {
    3
}

/// Whether this host can take a pidfd (`pidfd_open`, Linux 5.3+). The pidfd tests
/// call this and SKIP LOUDLY with a named reason when it is false.
pub fn pidfd_supported() -> bool {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: pidfd_open on our own pid; the descriptor is closed at once.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) };
        if raw >= 0 {
            // SAFETY: a descriptor we just obtained and own.
            unsafe { libc::close(raw as libc::c_int) };
            return true;
        }
    }
    false
}

pub struct ChildHandle {
    pid: u32,
    #[cfg(target_os = "linux")]
    fd: Option<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>>,
    /// Test seam: pretend `pidfd_send_signal` failed with this errno.
    #[cfg(target_os = "linux")]
    injected_signal_errno: Option<i32>,
}

impl ChildHandle {
    /// Take a handle on `pid`, which MUST still be an unreaped child of this
    /// process (call it right after spawn). Must run inside a Tokio runtime.
    pub fn open(pid: u32) -> Self {
        Self::open_with(pid, true, None)
    }

    /// Like [`open`](Self::open) but the pidfd acquisition can be disabled and a
    /// signal errno injected, so tests can drive the failure branches.
    #[doc(hidden)]
    pub fn open_with(pid: u32, pidfd: bool, injected_signal_errno: Option<i32>) -> Self {
        #[cfg(target_os = "linux")]
        {
            Self {
                pid,
                fd: if pidfd { open_pidfd(pid) } else { None },
                injected_signal_errno,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (pidfd, injected_signal_errno);
            Self { pid }
        }
    }

    /// The child's pid: diagnostic only, never a signalling authority.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether a termination mechanism (a pidfd) is held.
    pub fn has_mechanism(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.fd.is_some()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    /// Whether the child has exited, observed through the pidfd.
    pub fn exit_state(&self) -> ExitState {
        #[cfg(target_os = "linux")]
        if let Some(fd) = &self.fd {
            use std::os::fd::AsRawFd;
            let mut poll = libc::pollfd {
                fd: fd.get_ref().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd, zero timeout.
            let rc = unsafe { libc::poll(&mut poll, 1, 0) };
            let errno = if rc < 0 {
                std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
            } else {
                0
            };
            return classify_poll(rc, (poll.revents & libc::POLLIN) != 0, errno);
        }
        ExitState::Unknown(0)
    }

    /// Whether the child is known to have exited (`false` when unknown).
    pub fn has_exited(&self) -> bool {
        self.exit_state() == ExitState::Exited
    }

    /// SIGKILL the child through the pidfd and wait, bounded, for its exit.
    pub async fn force_stop(&self, bound: Duration) -> ForceStop {
        if !self.has_mechanism() {
            return ForceStop::Unstopped;
        }
        match self.exit_state() {
            ExitState::Exited => return ForceStop::AlreadyExited,
            ExitState::Unknown(errno) => {
                tracing::error!(pid = self.pid, errno, "cannot observe the MCP child's exit");
                return ForceStop::Unstopped;
            }
            ExitState::Running => {}
        }
        match classify_signal(self.send_kill()) {
            SignalOutcome::Sent => {}
            SignalOutcome::NoSuchProcess => return ForceStop::AlreadyExited,
            SignalOutcome::Failed(errno) => {
                tracing::error!(pid = self.pid, errno, "pidfd_send_signal(SIGKILL) failed");
                return ForceStop::Unstopped;
            }
        }
        #[cfg(target_os = "linux")]
        {
            let Some(fd) = &self.fd else {
                return ForceStop::Unstopped;
            };
            return match tokio::time::timeout(bound, fd.readable()).await {
                Ok(Ok(_)) => match self.exit_state() {
                    ExitState::Exited => ForceStop::Killed,
                    ExitState::Running | ExitState::Unknown(_) => ForceStop::Unstopped,
                },
                Ok(Err(error)) => {
                    tracing::error!(pid = self.pid, %error, "pidfd readiness failed");
                    ForceStop::Unstopped
                }
                Err(_) => ForceStop::Stuck,
            };
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = bound;
            ForceStop::Unstopped
        }
    }

    #[cfg(target_os = "linux")]
    fn send_kill(&self) -> Result<(), i32> {
        if let Some(errno) = self.injected_signal_errno {
            return Err(errno);
        }
        use std::os::fd::AsRawFd;
        let Some(fd) = &self.fd else {
            return Err(libc::EBADF);
        };
        // SAFETY: pidfd_send_signal(pidfd, SIGKILL, info = NULL, flags = 0); the
        // descriptor is owned by this handle and no pointer is dereferenced by us.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.get_ref().as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0u32,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn send_kill(&self) -> Result<(), i32> {
        Err(0)
    }
}

#[cfg(target_os = "linux")]
fn open_pidfd(pid: u32) -> Option<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>> {
    use std::os::fd::{FromRawFd, OwnedFd};
    // SAFETY: pidfd_open takes a pid and flags and returns a new descriptor or -1.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if raw < 0 {
        tracing::warn!(
            pid,
            error = %std::io::Error::last_os_error(),
            "pidfd_open failed: this MCP child cannot be force-stopped"
        );
        return None;
    }
    // SAFETY: a freshly returned descriptor that nothing else owns.
    let owned = unsafe { OwnedFd::from_raw_fd(raw as libc::c_int) };
    match tokio::io::unix::AsyncFd::new(owned) {
        Ok(fd) => Some(fd),
        Err(error) => {
            tracing::warn!(pid, %error, "AsyncFd registration failed: this MCP child cannot be force-stopped");
            None
        }
    }
}
