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

use super::{FlowError, LoginUi};
use crate::domain::models::auth_policy::CallbackHost;
use std::io;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task::JoinSet,
};

/// Own the sockets in the login future, not in detached accept tasks. Dropping
/// that future immediately releases both listeners, including during exchange.
pub(super) struct Loopback {
    v4: TcpListener,
    v6: Option<TcpListener>,
    pub redirect: String,
    pub launch: String,
    pub authorize: String,
}
impl Loopback {
    pub async fn bind(
        port: u16,
        fallback: bool,
        host: CallbackHost,
        path: &str,
        ui: &dyn LoginUi,
    ) -> Result<Self, FlowError> {
        let mut requested = port;
        for attempt in 0..5 {
            let v4 = match TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, requested)).await {
                Ok(listener) => listener,
                Err(e) if fallback && requested != 0 && e.kind() == io::ErrorKind::AddrInUse => {
                    TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                        .await
                        .map_err(|_| {
                            FlowError::configuration("Cannot bind OAuth callback listener")
                        })?
                }
                // A registered redirect cannot move: tell the user which port to free.
                Err(e) if !fallback && e.kind() == io::ErrorKind::AddrInUse => {
                    return Err(FlowError::configuration(format!(
                        "Cannot bind: OAuth callback port {requested} is in use and the provider accepts only this callback port. Free port {requested} and retry."
                    )));
                }
                Err(_) => {
                    return Err(FlowError::configuration(
                        "Cannot bind OAuth callback listener",
                    ));
                }
            };
            let actual = v4
                .local_addr()
                .map_err(|_| FlowError::configuration("Cannot read OAuth callback address"))?
                .port();
            let v6 = match host {
                // A non-`localhost` callback host binds that single address only.
                CallbackHost::Ipv4Loopback => None,
                CallbackHost::Localhost => {
                    match TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, actual)).await {
                        Ok(listener) => Some(listener),
                        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                            drop(v4);
                            if !fallback || attempt == 4 {
                                return Err(FlowError::configuration(
                                    "IPv6 OAuth callback port is already in use",
                                ));
                            }
                            requested = 0;
                            continue;
                        }
                        Err(e) if ipv6_unavailable(&e) => None,
                        Err(_) => {
                            return Err(FlowError::configuration(
                                "Cannot bind IPv6 OAuth callback listener",
                            ));
                        }
                    }
                }
            };
            if actual != port {
                ui.on_progress(&format!(
                    "Preferred port {port} unavailable, using port {actual}"
                ));
            }
            let name = match host {
                CallbackHost::Localhost => "localhost",
                CallbackHost::Ipv4Loopback => "127.0.0.1",
            };
            return Ok(Self {
                v4,
                v6,
                redirect: format!("http://{name}:{actual}{path}"),
                launch: format!("http://{name}:{actual}/launch"),
                authorize: String::new(),
            });
        }
        Err(FlowError::configuration(
            "Cannot bind OAuth callback listener",
        ))
    }
    pub async fn serve(
        &self,
        path: &str,
        state: &str,
        tx: mpsc::Sender<Result<String, FlowError>>,
    ) {
        let mut connections = JoinSet::new();
        loop {
            let accepted = if take_injected_accept_failure() {
                Err(io::Error::other("injected accept failure"))
            } else {
                tokio::select! {
                    result = self.v4.accept() => result,
                    result = async { match &self.v6 { Some(v6) => v6.accept().await, None => std::future::pending().await } } => result,
                    Some(_) = connections.join_next(), if !connections.is_empty() => continue,
                }
            };
            // A failed accept (aborted handshake, descriptor pressure) never ends the
            // listener; it lives until the login future that owns it finishes (the
            // 300 s wait, then the exchange's own bounds). A short pause keeps a
            // persistent accept error from spinning a core.
            let Ok((stream, _)) = accepted else {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                continue;
            };
            let (path, state, authorize, tx) = (
                path.to_owned(),
                state.to_owned(),
                self.authorize.clone(),
                tx.clone(),
            );
            connections.spawn(async move {
                handle(stream, &path, &state, &authorize, tx).await;
            });
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Accept failures `serve` reports before its next real accept (tests only).
    static INJECTED_ACCEPT_FAILURES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn take_injected_accept_failure() -> bool {
    INJECTED_ACCEPT_FAILURES.with(|left| {
        let n = left.get();
        left.set(n.saturating_sub(1));
        n > 0
    })
}

#[cfg(not(test))]
fn take_injected_accept_failure() -> bool {
    false
}
fn ipv6_unavailable(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::AddrNotAvailable | io::ErrorKind::Unsupported
    ) {
        return true;
    }
    #[cfg(unix)]
    if matches!(
        error.raw_os_error(),
        Some(libc::EAFNOSUPPORT | libc::EPROTONOSUPPORT)
    ) {
        return true;
    }
    false
}
async fn handle(
    mut stream: TcpStream,
    callback_path: &str,
    state: &str,
    authorize: &str,
    tx: mpsc::Sender<Result<String, FlowError>>,
) {
    let head = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut head = Vec::with_capacity(1024);
        loop {
            let mut bytes = [0_u8; 1024];
            let n = stream.read(&mut bytes).await.ok()?;
            if n == 0 {
                return None;
            }
            head.extend_from_slice(&bytes[..n]);
            if let Some(end) = head.windows(4).position(|w| w == b"\r\n\r\n") {
                return (end + 4 <= 8192).then_some(head);
            }
            if head.len() >= 8192 {
                return None;
            }
        }
    })
    .await;
    let Ok(Some(head)) = head else {
        return;
    };
    let Ok(head) = std::str::from_utf8(&head) else {
        return;
    };
    let mut line = head.lines().next().unwrap_or_default().split_whitespace();
    if line.next() != Some("GET") {
        respond(&mut stream, "405 Method Not Allowed", "", None).await;
        return;
    }
    let target = line.next().unwrap_or_default();
    if !target.starts_with('/') || target.starts_with("//") {
        respond(&mut stream, "400 Bad Request", "", None).await;
        return;
    }
    let Ok(url) = url::Url::parse(&format!("http://localhost{target}")) else {
        return;
    };
    if url.path() == "/launch" {
        if authorize.is_empty() {
            respond(&mut stream, "503 Service Unavailable", "", None).await;
        } else {
            respond(&mut stream, "302 Found", "", Some(authorize)).await;
        }
        return;
    }
    if url.path() != callback_path {
        respond(&mut stream, "404 Not Found", "Not found", None).await;
        return;
    }
    let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    // omp compares state only when one is expected (a stateless key mint sends none);
    // an `error` is then a denial whether or not a state came back.
    if !state.is_empty() && params.get("state").map(String::as_str) != Some(state) {
        respond(
            &mut stream,
            "400 Bad Request",
            "State mismatch - possible CSRF attack",
            None,
        )
        .await;
        return;
    }
    if params.contains_key("error") {
        let _ = tx.try_send(Err(FlowError::configuration(
            "OAuth authorization was denied",
        )));
        respond(
            &mut stream,
            "400 Bad Request",
            "Authorization failed. Return to the application.",
            None,
        )
        .await;
    } else if let Some(code) = params.get("code").filter(|code| !code.trim().is_empty()) {
        respond(
            &mut stream,
            "200 OK",
            "Login complete. Return to the application.",
            None,
        )
        .await;
        let _ = tx.try_send(Ok(code.clone()));
    } else {
        respond(
            &mut stream,
            "400 Bad Request",
            "Missing authorization code",
            None,
        )
        .await;
    }
}
async fn respond(stream: &mut TcpStream, status: &str, body: &str, location: Option<&str>) {
    let location = location
        .map(|v| format!("Location: {v}\r\n"))
        .unwrap_or_default();
    let response = format!(
        "HTTP/1.1 {status}\r\n{location}Content-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::SecretString;
    use futures::future::BoxFuture;

    struct Silent;
    impl LoginUi for Silent {
        fn on_auth_url(&self, _: &str, _: &str, _: &str) {}
        fn on_device_code(&self, _: &str, _: &str, _: &str) {}
        fn on_progress(&self, _: &str) {}
        fn prompt_secret(&self, _: &str) -> BoxFuture<'_, Option<SecretString>> {
            Box::pin(async { None })
        }
        fn manual_input(&self, _: &str) -> Option<BoxFuture<'static, Option<String>>> {
            None
        }
        fn prompt_line(&self, _: &str) -> BoxFuture<'_, Option<String>> {
            Box::pin(async { None })
        }
        fn cancel_token(&self) -> tokio_util::sync::CancellationToken {
            tokio_util::sync::CancellationToken::new()
        }
    }

    /// Failed accepts never end the listener: after three injected accept
    /// errors the callback is still served.
    #[tokio::test]
    async fn serve_continues_after_accept_errors() {
        let listener = Loopback::bind(0, false, CallbackHost::Ipv4Loopback, "/callback", &Silent)
            .await
            .unwrap();
        INJECTED_ACCEPT_FAILURES.with(|left| left.set(3));
        let (tx, mut rx) = mpsc::channel(1);
        let callback = format!("{}?code=the-code&state=the-state", listener.redirect);
        let client = async {
            let _ = reqwest::get(&callback).await;
            rx.recv().await
        };
        let received = tokio::select! {
            () = listener.serve("/callback", "the-state", tx) => unreachable!("serve never returns"),
            received = client => received,
        };
        assert!(matches!(received, Some(Ok(_))), "{received:?}");
        assert_eq!(INJECTED_ACCEPT_FAILURES.with(std::cell::Cell::get), 0);
    }
}
