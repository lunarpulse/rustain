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

use crate::{adapters::auth::flows::LoginUi, domain::models::SecretString};
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
}
impl LoginUi for TerminalUi {
    fn on_auth_url(&self, url: &str, launch: &str, instructions: &str) {
        eprintln!("{instructions}\n{url}\nLocal browser launch: {launch}");
        if self.interactive {
            #[cfg(target_os = "macos")]
            let program = "open";
            #[cfg(not(target_os = "macos"))]
            let program = "xdg-open";
            let _ = std::process::Command::new(program)
                .arg(launch)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }
    fn on_progress(&self, message: &str) {
        eprintln!("{message}");
    }
    fn prompt_secret(&self, label: &str) -> BoxFuture<'_, Option<SecretString>> {
        let label = label.to_owned();
        Box::pin(async move {
            let (tx, rx) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let value = if label.starts_with("Overwrite ") {
                    eprint!("{label}");
                    let _ = std::io::stderr().flush();
                    let mut value = String::new();
                    std::io::stdin().read_line(&mut value).ok().map(|_| value)
                } else {
                    rpassword::prompt_password(label).ok()
                };
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
    fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }
}
