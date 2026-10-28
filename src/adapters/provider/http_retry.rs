//! HTTP attempts of one request: the OpenAI-wire policy (the HTTP half of the
//! Responses `Generic` retry policy, adapted from oh-my-pi utils/openai-http.ts)
//! plus the per-attempt identity alternate of a `RequestState`. The Messages
//! route has no HTTP-attempt policy: like omp, its retries are the provider
//! loop of `stream_retry`.
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

use reqwest::header::HeaderMap;

use super::request_profile::RequestState;
use crate::adapters::responses::retry;
use crate::domain::errors::ProviderError;

/// Total HTTP attempts (omp: `maxRetries: 5`).
const ATTEMPTS: u32 = 6;

/// Which responses are resent, and after how long.
#[derive(Debug)]
pub enum HttpRetryPolicy {
    /// 408/429/≥500 and transport errors; `min(500·2^n, 60 000)` ms or the retry
    /// hint; a hint above 60 000 ms, an admission 429 or a body-read timeout is
    /// surfaced at once.
    OpenAiWire,
}

pub static OPENAI_WIRE: HttpRetryPolicy = HttpRetryPolicy::OpenAiWire;

impl HttpRetryPolicy {
    /// Delay before retry number `n` (1-based) when no hint applies.
    fn default_delay(&self, n: u32) -> u64 {
        match self {
            Self::OpenAiWire => retry::generic_delay(n),
        }
    }

    /// The wait before retry number `n` (1-based) after a non-2xx response, or
    /// `None` when the response is final.
    pub fn status_delay(
        &self,
        n: u32,
        status: u16,
        headers: &HeaderMap,
        body: &str,
    ) -> Option<u64> {
        if n >= ATTEMPTS {
            return None;
        }
        match self {
            Self::OpenAiWire => {
                if !retry::retry_status(status)
                    || retry::admission_rejection(headers, body)
                    || retry::request_body_read_timeout(status, body)
                {
                    return None;
                }
                match retry::hint(headers, body, crate::infrastructure::utils::auth_now_ms()) {
                    Some(hint) if hint > retry::GENERIC_MAX_DELAY_MS => None,
                    Some(hint) => Some(hint),
                    None => Some(self.default_delay(n)),
                }
            }
        }
    }

    /// The wait before retry number `n` (1-based) after a transport error.
    pub fn transport_delay(&self, n: u32) -> Option<u64> {
        (n < ATTEMPTS).then(|| self.default_delay(n))
    }
}

/// The final answer of one logical HTTP exchange.
pub enum Outcome {
    Success(reqwest::Response),
    Failure {
        status: u16,
        headers: HeaderMap,
        body: String,
    },
}

/// Sends `request` (headers already set) under `policy`, consulting `state`
/// for one identity alternate on a 403/400 of each attempt. A transport error
/// that survives the policy is returned as `Err`.
pub async fn send(
    client: &reqwest::Client,
    request: &reqwest::Request,
    state: Option<&mut (dyn RequestState + '_)>,
    policy: Option<&HttpRetryPolicy>,
) -> Result<Outcome, ProviderError> {
    exchange(client, request, state, policy)
        .await
        .map(|(outcome, _)| outcome)
}

/// [`send`] for a stored request that provider re-issues send again: an
/// identity alternate that wins is written into `request`, so every later
/// re-issue of the same logical request sends the won identity directly
/// (omp rebuilds its headers from the learned identity on each re-issue).
pub async fn send_adopting(
    client: &reqwest::Client,
    request: &mut reqwest::Request,
    state: Option<&mut (dyn RequestState + '_)>,
    policy: Option<&HttpRetryPolicy>,
) -> Result<Outcome, ProviderError> {
    let (outcome, won) = exchange(client, request, state, policy).await?;
    if let Some(won) = won {
        for (name, value) in &won {
            request.headers_mut().insert(name, value.clone());
        }
    }
    Ok(outcome)
}

/// One logical exchange, and the headers of an identity alternate that won.
async fn exchange(
    client: &reqwest::Client,
    request: &reqwest::Request,
    mut state: Option<&mut (dyn RequestState + '_)>,
    policy: Option<&HttpRetryPolicy>,
) -> Result<(Outcome, Option<HeaderMap>), ProviderError> {
    let reuse = || {
        request
            .try_clone()
            .ok_or_else(|| ProviderError::Other("Could not reuse request body".into()))
    };
    let mut retries = 0u32;
    loop {
        let response = match client.execute(reuse()?).await {
            Ok(response) => response,
            Err(error) => {
                if let Some(delay) = policy.and_then(|p| p.transport_delay(retries + 1)) {
                    retries += 1;
                    tokio::time::sleep(retry::delay(delay)).await;
                    continue;
                }
                return Err(super::classify_reqwest_error(&error));
            }
        };
        if response.status().is_success() {
            return Ok((Outcome::Success(response), None));
        }
        let mut status = response.status().as_u16();
        let mut headers = response.headers().clone();
        let mut body = read_body(response).await;
        if matches!(status, 400 | 403)
            && let Some(alternate) = state
                .as_deref_mut()
                .and_then(|state| state.alternate(status, body.as_bytes(), request.headers()))
        {
            let mut resend = reuse()?;
            for (name, value) in &alternate.headers {
                resend.headers_mut().insert(name, value.clone());
            }
            // omp: a resend that never answered settles nothing (the learned
            // identity stays); its transport error is retried like any other.
            match client.execute(resend).await {
                Ok(response) if response.status().is_success() => {
                    (alternate.settle)(true);
                    return Ok((Outcome::Success(response), Some(alternate.headers)));
                }
                Ok(response) => {
                    (alternate.settle)(false);
                    status = response.status().as_u16();
                    headers = response.headers().clone();
                    body = read_body(response).await;
                }
                Err(error) => {
                    if let Some(delay) = policy.and_then(|p| p.transport_delay(retries + 1)) {
                        retries += 1;
                        tokio::time::sleep(retry::delay(delay)).await;
                        continue;
                    }
                    return Err(super::classify_reqwest_error(&error));
                }
            }
        }
        if let Some(delay) =
            policy.and_then(|p| p.status_delay(retries + 1, status, &headers, &body))
        {
            retries += 1;
            tokio::time::sleep(retry::delay(delay)).await;
            continue;
        }
        return Ok((
            Outcome::Failure {
                status,
                headers,
                body,
            },
            None,
        ));
    }
}

/// An error body; an unreadable one reads `unknown`, as the single-attempt
/// transports always reported it.
async fn read_body(response: reqwest::Response) -> String {
    response
        .text()
        .await
        .unwrap_or_else(|_| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().unwrap());
        }
        map
    }

    #[test]
    fn openai_wire_retries_transient_statuses_and_surfaces_long_hints() {
        let none = HeaderMap::new();
        let policy = &OPENAI_WIRE;
        assert_eq!(policy.status_delay(1, 503, &none, ""), Some(500));
        assert_eq!(policy.status_delay(2, 503, &none, ""), Some(1000));
        assert_eq!(policy.status_delay(1, 408, &none, ""), Some(500));
        assert_eq!(policy.status_delay(6, 503, &none, ""), None);
        assert_eq!(policy.status_delay(1, 409, &none, ""), None);
        assert_eq!(policy.status_delay(1, 400, &none, ""), None);
        assert_eq!(
            policy.status_delay(1, 429, &headers(&[("retry-after", "2")]), ""),
            Some(2000)
        );
        assert_eq!(
            policy.status_delay(1, 429, &headers(&[("retry-after", "120")]), ""),
            None
        );
        assert_eq!(
            policy.status_delay(
                1,
                429,
                &none,
                r#"{"error":{"rate_limit_type":"max_parallel_requests"}}"#
            ),
            None
        );
        assert_eq!(policy.transport_delay(1), Some(500));
        assert_eq!(policy.transport_delay(6), None);
    }
}
