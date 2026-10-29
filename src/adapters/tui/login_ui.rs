//! The TUI host of the login engines: every callback becomes a
//! `LoginEvent` for the event loop; prompts wait on a one-shot reply the
//! overlay sends when the user submits (a dropped reply reads as `None`).
//!
//! MIT License
//!
//! Copyright (c) 2025 Mario Zechner
//! Copyright (c) 2025-2026 Can Bölük
//! Copyright (c) 2026 Stencil Labs, Inc.
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.

use crate::adapters::auth::flows::LoginUi;
use crate::domain::events::{AppEvent, LoginEvent, LoginReply};
use crate::domain::models::SecretString;
use crate::infrastructure::runtime::event_bus::EventBus;
use futures::future::BoxFuture;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Opens a URL in the user's browser (production: `auth::opener::open_url`).
pub type Opener = Arc<dyn Fn(&str) + Send + Sync>;

pub struct TuiLoginUi {
    events: Arc<EventBus>,
    login_id: u64,
    cancel: CancellationToken,
    opener: Opener,
}

impl TuiLoginUi {
    pub fn new(
        events: Arc<EventBus>,
        login_id: u64,
        cancel: CancellationToken,
        opener: Opener,
    ) -> Self {
        Self {
            events,
            login_id,
            cancel,
            opener,
        }
    }

    fn emit(&self, event: LoginEvent) {
        let _ = self.events.emit_domain(AppEvent::Login(event));
    }
}

/// A provider-supplied string usable as a link, opener or copy target: an
/// `http`/`https` URL with no control character or whitespace. The returned
/// form is the re-serialized URL, so no `ESC`/`BEL` can reach a terminal.
pub fn safe_url(text: &str) -> Option<String> {
    if text.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let url = url::Url::parse(text).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.to_string())
}

impl LoginUi for TuiLoginUi {
    fn on_auth_url(&self, url: &str, launch_url: &str, instructions: &str) {
        self.emit(LoginEvent::AuthUrl {
            login_id: self.login_id,
            url: url.into(),
            launch_url: launch_url.into(),
            instructions: instructions.into(),
        });
        if let Some(launch) = safe_url(launch_url) {
            (self.opener)(&launch);
        }
    }

    fn on_device_code(&self, verification_url: &str, user_code: &str, instructions: &str) {
        self.emit(LoginEvent::DeviceCode {
            login_id: self.login_id,
            verification_url: verification_url.into(),
            user_code: user_code.into(),
            instructions: instructions.into(),
        });
        if let Some(url) = safe_url(verification_url) {
            (self.opener)(&url);
        }
    }

    fn on_progress(&self, message: &str) {
        self.emit(LoginEvent::Progress {
            login_id: self.login_id,
            message: message.into(),
        });
    }

    fn prompt_secret(&self, label: &str) -> BoxFuture<'_, Option<SecretString>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.emit(LoginEvent::PromptSecret {
            login_id: self.login_id,
            label: label.into(),
            reply: LoginReply(tx),
        });
        Box::pin(async move { rx.await.ok().flatten() })
    }

    /// Always offered: the overlay is interactive.
    fn manual_input(&self, prompt: &str) -> Option<BoxFuture<'static, Option<String>>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.emit(LoginEvent::ManualInput {
            login_id: self.login_id,
            prompt: prompt.into(),
            reply: LoginReply(tx),
        });
        Some(Box::pin(async move { rx.await.ok().flatten() }))
    }

    fn prompt_line(&self, prompt: &str) -> BoxFuture<'_, Option<String>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.emit(LoginEvent::PromptLine {
            login_id: self.login_id,
            prompt: prompt.into(),
            reply: LoginReply(tx),
        });
        Box::pin(async move { rx.await.ok().flatten() })
    }

    fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::safe_url;

    #[test]
    fn only_clean_web_urls_become_links() {
        assert_eq!(
            safe_url("https://example.test/device?x=1").as_deref(),
            Some("https://example.test/device?x=1")
        );
        assert_eq!(
            safe_url("http://localhost:1455/launch").as_deref(),
            Some("http://localhost:1455/launch")
        );
        for bad in [
            "https://x.ai/\u{1b}]52;c;ZXZpbA==\u{7}",
            "https://x.ai/\u{9b}31m",
            "https://x.ai/ a",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert_eq!(safe_url(bad), None, "{bad:?}");
        }
    }
}
