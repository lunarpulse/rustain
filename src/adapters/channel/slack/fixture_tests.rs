//! Typed local Slack transport for offline evidence (Story C-12): scripted
//! connections, recorded acknowledgements and posts. No network, no tokens.

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{Semaphore, mpsc};
use tokio::time::Instant;

use super::{SlackError, SlackSocket, SlackTransport, SocketEvent};

/// What one `connect()` call does.
pub(crate) enum ConnectStep {
    /// The attempt fails before a socket exists.
    Fail,
    /// The attempt opens a socket fed by the returned sender; dropping every
    /// sender closes it.
    Open(mpsc::UnboundedReceiver<SocketEvent>),
}

/// A scripted connection: the sender feeds frames, the step is queued on the fake.
pub(crate) fn socket() -> (mpsc::UnboundedSender<SocketEvent>, ConnectStep) {
    let (tx, rx) = mpsc::unbounded_channel();
    (tx, ConnectStep::Open(rx))
}

pub(crate) struct FakeSlack {
    steps: Mutex<VecDeque<ConnectStep>>,
    pub connects: AtomicUsize,
    pub connect_times: Mutex<Vec<Instant>>,
    pub acks: Arc<Mutex<Vec<String>>>,
    pub acked: Arc<Semaphore>,
    pub posts: Mutex<Vec<(String, String)>>,
    pub post_times: Mutex<Vec<Instant>>,
    pub posted: Semaphore,
    pub fail_posts: std::sync::atomic::AtomicBool,
}

impl FakeSlack {
    pub(crate) fn new(steps: Vec<ConnectStep>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into()),
            connects: AtomicUsize::new(0),
            connect_times: Mutex::new(Vec::new()),
            acks: Arc::new(Mutex::new(Vec::new())),
            acked: Arc::new(Semaphore::new(0)),
            posts: Mutex::new(Vec::new()),
            post_times: Mutex::new(Vec::new()),
            posted: Semaphore::new(0),
            fail_posts: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub(crate) fn push(&self, step: ConnectStep) {
        self.steps.lock().push_back(step);
    }

    pub(crate) fn connect_count(&self) -> usize {
        self.connects.load(Ordering::SeqCst)
    }

    pub(crate) fn posts(&self) -> Vec<(String, String)> {
        self.posts.lock().clone()
    }

    pub(crate) fn acks(&self) -> Vec<String> {
        self.acks.lock().clone()
    }
}

struct FakeSocket {
    rx: mpsc::UnboundedReceiver<SocketEvent>,
    acks: Arc<Mutex<Vec<String>>>,
    acked: Arc<Semaphore>,
}

#[async_trait::async_trait]
impl SlackSocket for FakeSocket {
    async fn next_event(&mut self) -> Result<SocketEvent, SlackError> {
        Ok(self.rx.recv().await.unwrap_or(SocketEvent::Closed))
    }

    async fn send_text(&mut self, text: String) -> Result<(), SlackError> {
        self.acks.lock().push(text);
        self.acked.add_permits(1);
        Ok(())
    }
}

#[async_trait::async_trait]
impl SlackTransport for FakeSlack {
    async fn connect(&self) -> Result<Box<dyn SlackSocket>, SlackError> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        self.connect_times.lock().push(Instant::now());
        let step = self.steps.lock().pop_front();
        match step {
            Some(ConnectStep::Open(rx)) => Ok(Box::new(FakeSocket {
                rx,
                acks: self.acks.clone(),
                acked: self.acked.clone(),
            })),
            Some(ConnectStep::Fail) | None => Err(SlackError("fixture connect failure".into())),
        }
    }

    async fn post_message(&self, channel: &str, text: &str) -> Result<(), SlackError> {
        if self.fail_posts.load(Ordering::SeqCst) {
            self.posted.add_permits(1);
            return Err(SlackError("fixture post failure".into()));
        }
        self.post_times.lock().push(Instant::now());
        self.posts
            .lock()
            .push((channel.to_string(), text.to_string()));
        self.posted.add_permits(1);
        Ok(())
    }
}
