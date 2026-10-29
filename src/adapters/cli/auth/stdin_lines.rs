//! The process's one terminal-input owner for CLI sign-in: every login read
//! (`TerminalUi` lines and masked keys, the method menu, the `rustain init`
//! provider prompt) is served by one thread, one request at a time, and only
//! while a requester still waits. Two readers on one terminal would split a
//! key between them: rpassword reads `/dev/tty` with canonical mode off, so a
//! concurrent `stdin` read returns after one byte of it, and a reader that
//! reads ahead hands a later prompt's line to an earlier one.
//!
//! A line read for a requester that left (an abandoned `manual_input`) is held
//! for the next line request; a masked key is never held and never served
//! from held lines.

use crate::domain::models::SecretString;
use std::collections::VecDeque;
use std::io;
use std::sync::LazyLock;
use std::sync::mpsc;
use tokio::sync::oneshot;

/// The terminal reads the owner thread performs.
pub trait TerminalInput: Send {
    /// One line including its terminator; `None` at end of input.
    fn read_line(&mut self) -> io::Result<Option<String>>;
    /// A masked secret after printing `prompt`; `None` when none was entered.
    fn read_secret(&mut self, prompt: &str) -> io::Result<Option<String>>;
}

/// Production input: the shared std `stdin` buffer (so a line typed during the
/// wizard is not lost) and rpassword's masked `/dev/tty` prompt.
struct StdTerminal;

impl TerminalInput for StdTerminal {
    fn read_line(&mut self) -> io::Result<Option<String>> {
        let mut line = String::new();
        Ok(match std::io::stdin().read_line(&mut line)? {
            0 => None,
            _ => Some(line),
        })
    }
    fn read_secret(&mut self, prompt: &str) -> io::Result<Option<String>> {
        rpassword::prompt_password(prompt).map(Some)
    }
}

enum Request {
    Line(oneshot::Sender<Option<String>>),
    Secret(String, oneshot::Sender<Option<SecretString>>),
}

/// A handle to one owner thread; clones share it, and the thread ends when
/// the last handle is dropped.
#[derive(Clone)]
pub struct Pump {
    requests: mpsc::Sender<Request>,
}

impl Pump {
    /// Starts the owner thread over `input`.
    pub fn spawn(input: Box<dyn TerminalInput>) -> Pump {
        let (requests, rx) = mpsc::channel();
        // Detached: a blocked read never holds runtime shutdown hostage.
        std::thread::Builder::new()
            .name("rustain-terminal-input".into())
            .spawn(move || serve(input, rx))
            .expect("spawn the terminal-input thread");
        Pump { requests }
    }

    /// The next line (with its terminator); `None` at end of input.
    pub async fn line(&self) -> Option<String> {
        let (reply, rx) = oneshot::channel();
        self.requests.send(Request::Line(reply)).ok()?;
        rx.await.ok().flatten()
    }

    /// A masked secret read after every earlier request was served.
    pub async fn secret(&self, prompt: &str) -> Option<SecretString> {
        let (reply, rx) = oneshot::channel();
        self.requests
            .send(Request::Secret(prompt.to_owned(), reply))
            .ok()?;
        rx.await.ok().flatten()
    }
}

fn serve(mut input: Box<dyn TerminalInput>, requests: mpsc::Receiver<Request>) {
    let mut held = VecDeque::new();
    let mut ended = false;
    while let Ok(request) = requests.recv() {
        match request {
            Request::Line(reply) => {
                // No read for a requester that already left.
                if reply.is_closed() {
                    continue;
                }
                let line = match held.pop_front() {
                    Some(line) => Some(line),
                    None if ended => None,
                    None => match input.read_line() {
                        Ok(Some(line)) => Some(line),
                        Ok(None) => {
                            ended = true;
                            None
                        }
                        Err(_) => None,
                    },
                };
                // A requester that left during the read: keep the line for the
                // next one (decided by the send itself, never a prior check).
                if let Err(Some(line)) = reply.send(line) {
                    held.push_back(line);
                }
            }
            Request::Secret(prompt, reply) => {
                if reply.is_closed() {
                    continue;
                }
                let secret = input
                    .read_secret(&prompt)
                    .ok()
                    .flatten()
                    .map(SecretString::from);
                // A failed send drops (and zeroizes) the secret.
                let _ = reply.send(secret);
            }
        }
    }
}

static PUMP: LazyLock<Pump> = LazyLock::new(|| Pump::spawn(Box::new(StdTerminal)));

/// The process's terminal-input owner.
pub fn pump() -> &'static Pump {
    &PUMP
}

/// The next terminal line through the process's owner.
pub async fn line() -> Option<String> {
    PUMP.line().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::auth::flows::LoginUi;
    use crate::adapters::cli::auth::terminal_ui::TerminalUi;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

    /// Hang guard only; every wait below is released by a scripted read.
    const GUARD: Duration = Duration::from_secs(30);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Read {
        Line,
        Secret,
    }

    /// Each read reports that it started, then blocks until the test feeds it.
    struct Scripted {
        started: UnboundedSender<Read>,
        lines: mpsc::Receiver<Option<String>>,
        secrets: mpsc::Receiver<Option<String>>,
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
    }

    impl Scripted {
        fn read<T>(&self, kind: Read, from: &mpsc::Receiver<Option<T>>) -> Option<T> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
            let _ = self.started.send(kind);
            let value = from.recv().ok().flatten();
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            value
        }
    }

    impl TerminalInput for Scripted {
        fn read_line(&mut self) -> io::Result<Option<String>> {
            Ok(self.read(Read::Line, &self.lines))
        }
        fn read_secret(&mut self, _prompt: &str) -> io::Result<Option<String>> {
            Ok(self.read(Read::Secret, &self.secrets))
        }
    }

    struct Script {
        ui: TerminalUi,
        pump: Pump,
        lines: mpsc::Sender<Option<String>>,
        secrets: mpsc::Sender<Option<String>>,
        max_in_flight: Arc<AtomicUsize>,
    }

    /// The ordered log of reads the owner thread started.
    struct Reads(UnboundedReceiver<Read>);

    impl Reads {
        async fn next(&mut self) -> Read {
            tokio::time::timeout(GUARD, self.0.recv())
                .await
                .expect("hang guard: a read starts")
                .expect("script open")
        }
        fn none_started(&mut self) -> bool {
            self.0.try_recv().is_err()
        }
    }

    impl Script {
        fn new() -> (Self, Reads) {
            let (started_tx, started) = unbounded_channel();
            let (lines, lines_rx) = mpsc::channel();
            let (secrets, secrets_rx) = mpsc::channel();
            let max_in_flight = Arc::new(AtomicUsize::new(0));
            let pump = Pump::spawn(Box::new(Scripted {
                started: started_tx,
                lines: lines_rx,
                secrets: secrets_rx,
                in_flight: Arc::new(AtomicUsize::new(0)),
                max_in_flight: max_in_flight.clone(),
            }));
            let script = Self {
                ui: TerminalUi::with_pump(pump.clone(), true),
                pump,
                lines,
                secrets,
                max_in_flight,
            };
            (script, Reads(started))
        }
        fn feed(&self, line: &str) {
            self.lines.send(Some(line.to_owned())).unwrap();
        }
    }

    async fn guarded<T>(what: &str, future: impl Future<Output = T>) -> T {
        tokio::time::timeout(GUARD, future)
            .await
            .unwrap_or_else(|_| panic!("hang guard: {what}"))
    }

    /// (a) A line typed after an abandoned `manual_input` reaches the next
    /// `prompt_line`, not the line after it.
    #[tokio::test]
    async fn an_abandoned_line_read_is_held_for_the_next_line_request() {
        let (script, mut reads) = Script::new();
        let mut abandoned = script
            .ui
            .manual_input("Paste the code")
            .expect("interactive");
        assert!(futures::poll!(&mut abandoned).is_pending());
        assert_eq!(reads.next().await, Read::Line);
        drop(abandoned);
        script.feed("L1\n");
        script.feed("L2\n");
        let next = guarded("prompt_line", script.ui.prompt_line("Next: ")).await;
        assert_eq!(next.as_deref(), Some("L1\n"));
        assert_eq!(
            guarded("second line", script.pump.line()).await.as_deref(),
            Some("L2\n")
        );
    }

    /// (b) A masked prompt requested while an abandoned read is outstanding
    /// starts only after that read completes, and its value never reaches a
    /// later line request.
    #[tokio::test]
    async fn a_secret_waits_for_the_outstanding_read_and_is_never_held() {
        let (script, mut reads) = Script::new();
        let mut abandoned = script
            .ui
            .manual_input("Paste the code")
            .expect("interactive");
        assert!(futures::poll!(&mut abandoned).is_pending());
        assert_eq!(reads.next().await, Read::Line);
        drop(abandoned);
        let secret = script.ui.prompt_secret("Enter key: ");
        tokio::pin!(secret);
        assert!(futures::poll!(&mut secret).is_pending());
        assert!(reads.none_started(), "no second read in flight");
        script.feed("typed-ahead\n");
        assert_eq!(reads.next().await, Read::Secret);
        script
            .secrets
            .send(Some("sk-K8-SECRET".to_owned()))
            .unwrap();
        let secret = guarded("secret", secret).await.expect("a secret");
        assert_eq!(secret.expose_secret(), "sk-K8-SECRET");
        assert_eq!(script.max_in_flight.load(Ordering::SeqCst), 1);
        let later = guarded("later line", script.pump.line()).await;
        assert_eq!(later.as_deref(), Some("typed-ahead\n"));
    }

    /// (c) After a served line the thread reads nothing until the next
    /// request: a secret requested next is the very next read.
    #[tokio::test]
    async fn the_owner_never_reads_ahead() {
        let (script, mut reads) = Script::new();
        let line = script.pump.line();
        tokio::pin!(line);
        assert!(futures::poll!(&mut line).is_pending());
        assert_eq!(reads.next().await, Read::Line);
        script.feed("L0\n");
        assert_eq!(guarded("L0", line).await.as_deref(), Some("L0\n"));
        let secret = script.pump.secret("k");
        tokio::pin!(secret);
        assert!(futures::poll!(&mut secret).is_pending());
        assert_eq!(reads.next().await, Read::Secret);
        script.secrets.send(Some("S0".to_owned())).unwrap();
        let secret = guarded("secret", secret).await.expect("a secret");
        assert_eq!(secret.expose_secret(), "S0");
        assert!(reads.none_started(), "log is exactly [Line, Secret]");
    }

    /// (d) End of input answers every later line request without a read.
    #[tokio::test]
    async fn end_of_input_is_final_for_lines() {
        let (script, mut reads) = Script::new();
        let line = script.pump.line();
        tokio::pin!(line);
        assert!(futures::poll!(&mut line).is_pending());
        assert_eq!(reads.next().await, Read::Line);
        script.lines.send(None).unwrap();
        assert_eq!(guarded("eof", line).await, None);
        for _ in 0..3 {
            assert_eq!(guarded("after eof", script.pump.line()).await, None);
        }
        assert_eq!(
            guarded("prompt after eof", script.ui.prompt_line("x")).await,
            None
        );
        assert!(reads.none_started(), "no read after end of input");
    }
}
