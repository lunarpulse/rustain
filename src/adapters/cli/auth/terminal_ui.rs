//! Terminal interaction adapted from oh-my-pi's OAuth terminal host.
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

use crate::{
    adapters::{
        auth::flows::LoginUi,
        cli::{auth::stdin_lines::Pump, session::list::sanitize_title},
    },
    domain::models::SecretString,
};
use futures::future::BoxFuture;
use std::{
    io::{IsTerminal, Write},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

/// Every read goes through the process's one terminal-input owner.
pub struct TerminalUi {
    cancel: CancellationToken,
    interactive: bool,
    pump: Pump,
}
impl Default for TerminalUi {
    fn default() -> Self {
        Self::new()
    }
}
impl TerminalUi {
    pub fn new() -> Self {
        Self::with_pump(
            super::stdin_lines::pump().clone(),
            std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
        )
    }
    /// A terminal host reading through `pump` (tests script its input).
    pub fn with_pump(pump: Pump, interactive: bool) -> Self {
        Self {
            cancel: CancellationToken::new(),
            interactive,
            pump,
        }
    }
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }
    fn open(&self, url: &str) {
        if self.interactive {
            crate::adapters::auth::opener::open_url(url);
        }
    }
}
impl LoginUi for TerminalUi {
    fn on_auth_url(&self, url: &str, launch: &str, instructions: &str) {
        eprintln!("{instructions}\n{url}\nLocal browser launch: {launch}");
        self.open(launch);
    }
    /// The instructions already carry the code; no loopback launch URL exists.
    /// Server-supplied text is stripped of terminal controls, and only a web page
    /// is handed to the opener.
    fn on_device_code(&self, verification_url: &str, _user_code: &str, instructions: &str) {
        eprintln!(
            "{}\n{}",
            sanitize_title(instructions),
            sanitize_title(verification_url)
        );
        if verification_url.starts_with("https://") || verification_url.starts_with("http://") {
            self.open(verification_url);
        }
    }
    /// Progress lines may carry a server-issued device code.
    fn on_progress(&self, message: &str) {
        eprintln!("{}", sanitize_title(message));
    }
    fn prompt_secret(&self, label: &str) -> BoxFuture<'_, Option<SecretString>> {
        let label = label.to_owned();
        Box::pin(async move { self.pump.secret(&label).await })
    }
    fn manual_input(&self, prompt: &str) -> Option<BoxFuture<'static, Option<String>>> {
        if !self.interactive {
            return None;
        }
        eprintln!("{prompt}");
        // A dropped future leaves its line to the next line request.
        let pump = self.pump.clone();
        Some(Box::pin(async move { pump.line().await }))
    }
    fn prompt_line(&self, prompt: &str) -> BoxFuture<'_, Option<String>> {
        let prompt = prompt.to_owned();
        Box::pin(async move {
            eprint!("{prompt}");
            let _ = std::io::stderr().flush();
            self.pump.line().await
        })
    }
    fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }
}
