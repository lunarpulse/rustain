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
            let v6 = match TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, actual)).await {
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
            };
            if actual != port {
                ui.on_progress(&format!(
                    "Preferred port {port} unavailable, using port {actual}"
                ));
            }
            return Ok(Self {
                v4,
                v6,
                redirect: format!("http://localhost:{actual}{path}"),
                launch: format!("http://localhost:{actual}/launch"),
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
            let accepted = tokio::select! {
                result = self.v4.accept() => result,
                result = async { match &self.v6 { Some(v6) => v6.accept().await, None => std::future::pending().await } } => result,
                Some(_) = connections.join_next(), if !connections.is_empty() => continue,
            };
            let Ok((stream, _)) = accepted else {
                return;
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
    if params.get("state").map(String::as_str) != Some(state) {
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
