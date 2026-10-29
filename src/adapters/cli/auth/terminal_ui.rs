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
    adapters::{auth::flows::LoginUi, cli::session::list::sanitize_title},
    domain::models::SecretString,
};
use futures::future::BoxFuture;
use std::{
    io::{IsTerminal, Write},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

pub struct TerminalUi {
    cancel: CancellationToken,
    interactive: bool,
}
impl Default for TerminalUi {
    fn default() -> Self {
        Self::new()
    }
}
impl TerminalUi {
    pub fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
            interactive: std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
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
        Box::pin(async move {
            let (tx, rx) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let value = rpassword::prompt_password(label).ok();
                let _ = tx.send(value.map(SecretString::new));
            });
            rx.await.ok().flatten()
        })
    }
    fn manual_input(&self, prompt: &str) -> Option<BoxFuture<'static, Option<String>>> {
        if !self.interactive {
            return None;
        }
        eprintln!("{prompt}");
        let (tx, rx) = tokio::sync::oneshot::channel();
        // A detached OS thread, unlike spawn_blocking, cannot hold runtime shutdown
        // hostage when the callback wins while stdin is still waiting for a line.
        std::thread::spawn(move || {
            let mut value = String::new();
            let value = match std::io::stdin().read_line(&mut value) {
                Ok(0) | Err(_) => None,
                Ok(_) => Some(value),
            };
            let _ = tx.send(value);
        });
        Some(Box::pin(async move { rx.await.ok().flatten() }))
    }
    fn prompt_line(&self, prompt: &str) -> BoxFuture<'_, Option<String>> {
        let prompt = prompt.to_owned();
        Box::pin(async move {
            let (tx, rx) = tokio::sync::oneshot::channel();
            // Detached like `manual_input`: a cancelled login never waits on stdin.
            std::thread::spawn(move || {
                eprint!("{prompt}");
                let _ = std::io::stderr().flush();
                let mut value = String::new();
                let value = match std::io::stdin().read_line(&mut value) {
                    Ok(0) | Err(_) => None,
                    Ok(_) => Some(value),
                };
                let _ = tx.send(value);
            });
            rx.await.ok().flatten()
        })
    }
    fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }
}
