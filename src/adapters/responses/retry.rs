//! Retry timing translated from oh-my-pi utils/fetch-retry.ts, with the
//! transport admission rules of ai/src/utils/openai-http.ts.
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

use regex::Regex;
use reqwest::header::HeaderMap;
use std::{sync::LazyLock, time::Duration};

/// Codex: a retry hint above this ends the attempt loop.
pub(super) const CODEX_MAX_DELAY_MS: u64 = 300_000;
/// `fetchWithRetry` default `maxDelayMs`: hint ceiling and default-delay cap.
pub(crate) const GENERIC_MAX_DELAY_MS: u64 = 60_000;
pub(crate) fn retry_status(status: u16) -> bool {
    status == 408 || status == 429 || status >= 500
}
/// `resolveDefaultDelay` for the `n`-th retry (1-based): `min(500·2^(n-1), 60 000)`.
pub(crate) fn generic_delay(n: u32) -> u64 {
    500u64
        .saturating_mul(1u64 << (n.saturating_sub(1)).min(32))
        .min(GENERIC_MAX_DELAY_MS)
}
/// A proxy concurrency-admission 429 (`rate_limit_type: max_parallel_requests`)
/// reached no model, so the transport surfaces it instead of retrying.
pub(crate) fn admission_rejection(headers: &HeaderMap, body: &str) -> bool {
    static BODY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#""rate_limit_type"\s*:\s*"max_parallel_requests""#)
            .expect("fixed admission pattern")
    });
    headers
        .get("rate_limit_type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim() == "max_parallel_requests")
        || BODY.is_match(body)
}
/// omp `isRequestBodyReadTimeout`: a full-replay request is not resent after it.
pub(crate) fn request_body_read_timeout(status: u16, body: &str) -> bool {
    static BODY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\btimed out reading request body\b").expect("fixed timeout pattern")
    });
    status == 408 && BODY.is_match(body)
}
fn finite(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}
fn millis(v: f64) -> u64 {
    v.clamp(0.0, u64::MAX as f64) as u64
}
/// JavaScript `Date.parse` over the forms providers send: RFC 2822, RFC 3339,
/// or a naive UTC `YYYY-MM-DD HH:MM:SS`.
pub(crate) fn date(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc2822(s)
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(s))
        .ok()
        .map(|v| v.timestamp_millis())
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(&s.replace('T', " "), "%Y-%m-%d %H:%M:%S%.f")
                .ok()
                .map(|v| v.and_utc().timestamp_millis())
        })
}
fn header<'a>(headers: &'a HeaderMap, key: &str) -> Option<&'a str> {
    headers.get(key)?.to_str().ok().filter(|v| !v.is_empty())
}
/// JavaScript `Number.parseInt(s, 10)`: optional sign, then the leading digits.
fn leading_integer(s: &str) -> Option<i64> {
    let s = s.trim_start();
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    let end = digits
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(digits.len());
    s[..s.len() - digits.len() + end].parse().ok()
}
pub fn hint(headers: &HeaderMap, body: &str, now: i64) -> Option<u64> {
    if let Some(v) = header(headers, "retry-after-ms")
        .and_then(finite)
        .filter(|v| *v >= 0.0)
    {
        return Some(millis(v));
    }
    if let Some(v) = header(headers, "retry-after") {
        if let Some(v) = finite(v) {
            return Some(millis(v * 1000.0));
        }
        if let Some(v) = date(v) {
            return Some(v.saturating_sub(now).max(0) as u64);
        }
    }
    if let Some(v) = header(headers, "x-ratelimit-reset-ms")
        .and_then(finite)
        .filter(|v| *v > 0.0)
    {
        let target = if v > 1e12 {
            Some(v)
        } else if v > 1e9 {
            Some(v * 1000.0)
        } else {
            None
        };
        match target {
            None => return Some(millis(v)),
            Some(v) if v > now as f64 => return Some(millis(v - now as f64)),
            _ => {}
        }
    }
    if let Some(v) = header(headers, "x-ratelimit-reset")
        .and_then(leading_integer)
        .and_then(|v| v.checked_mul(1000))
        .filter(|v| *v > now)
    {
        return Some((v - now) as u64);
    }
    if let Some(v) = header(headers, "x-ratelimit-reset-after")
        .and_then(finite)
        .filter(|v| *v > 0.0)
    {
        return Some(millis(v * 1000.0));
    }
    body_hint(body, now)
}
fn unit(s: &str) -> f64 {
    match s.to_ascii_lowercase().as_str() {
        "ms" => 1.0,
        "s" | "sec" => 1000.0,
        "m" | "min" | "mins" | "minute" | "minutes" => 60_000.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600_000.0,
        "d" | "day" | "days" => 86_400_000.0,
        _ => 0.0,
    }
}
fn body_hint(body: &str, now: i64) -> Option<u64> {
    static RELATIVE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        [
        r"(?i)Please retry in ([0-9.]+)(ms|s)",r#"(?i)"retryDelay":\s*"([0-9.]+)(ms|s)""#,
        r"(?i)try again in\s+~?\s*([0-9.]+)\s*(ms|sec|s|minutes?|mins?|m|hours?|hrs?|h)\b",
        r"(?i)(?:will\s+)?resets?\s+in\s+~?\s*([0-9.]+)\s*(ms|sec|s|minutes?|mins?|m|hours?|hrs?|h|days?|d)\b"
    ].iter().map(|p|Regex::new(p).expect("static retry pattern")).collect()
    });
    static QUOTA: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)reset after (?:(\d+)h)?(?:(\d+)m)?(\d+(?:\.\d+)?)s").unwrap()
    });
    static COMPOUND: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)resets?\s+in\s+~?\s*(\d+(?:\.\d+)?)\s*hr\s*(\d+(?:\.\d+)?)\s*min\b")
            .unwrap()
    });
    static ABSOLUTE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        [r"(?i)(?:will\s+)?reset at\s+([0-9]{4}-[0-9]{2}-[0-9]{2}[ T][0-9]{2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]+)?(?:Z|[+-][0-9]{2}:?[0-9]{2})?)",r"将在\s*([0-9]{4}-[0-9]{2}-[0-9]{2}\s+[0-9]{2}:[0-9]{2}:[0-9]{2})\s*重置"].iter().map(|p|Regex::new(p).unwrap()).collect()
    });
    static LEGACY: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        [
            r"(?i)\bretry-after-ms\s*[:=]\s*([0-9]+)\b",
            r"(?i)retry-after\s*[:=]\s*([^\s,;]+)",
            r"(?i)x-ratelimit-reset-ms\s*[:=]\s*(\d+)",
            r"(?i)x-ratelimit-reset\s*[:=]\s*(\d+)",
        ]
        .iter()
        .map(|p| Regex::new(p).unwrap())
        .collect()
    });
    let mut longest: Option<u64> = None;
    let mut naive: Option<u64> = None;
    let mut immediate = false;
    let mut consider = |v: f64| {
        if v > 0.0 {
            longest = Some(longest.unwrap_or(0).max(millis(v)));
        } else {
            immediate = true;
        }
    };
    if let Some(c) = QUOTA.captures(body) {
        let h = c.get(1).and_then(|v| finite(v.as_str())).unwrap_or(0.0);
        let m = c.get(2).and_then(|v| finite(v.as_str())).unwrap_or(0.0);
        if let Some(s) = finite(&c[3]) {
            if s + h + m > 0.0 {
                consider(((h * 60.0 + m) * 60.0 + s) * 1000.0);
            }
        }
    }
    for re in RELATIVE.iter() {
        if let Some(c) = re.captures(body) {
            if let Some(v) = finite(&c[1]).filter(|v| *v > 0.0) {
                consider(v * unit(&c[2]));
            }
        }
    }
    if let Some(c) = COMPOUND.captures(body) {
        if let (Some(h), Some(m)) = (finite(&c[1]), finite(&c[2])) {
            if h >= 0.0 && m > 0.0 {
                consider(h * 3_600_000.0 + m * 60_000.0);
            }
        }
    }
    for (i, re) in LEGACY.iter().enumerate() {
        if let Some(c) = re.captures(body) {
            let n = finite(&c[1]);
            let v = match i {
                0 => n,
                1 => n
                    .map(|v| v * 1000.0)
                    .or_else(|| date(&c[1]).map(|v| (v - now) as f64)),
                2 => n.map(|v| if v > 1e12 { v - now as f64 } else { v }),
                _ => n.map(|v| {
                    if v > 1e9 {
                        v * 1000.0 - now as f64
                    } else {
                        v * 1000.0
                    }
                }),
            };
            if let Some(v) = v {
                consider(v);
            }
        }
    }
    for re in ABSOLUTE.iter() {
        if let Some(c) = re.captures(body) {
            let normalized = c[1].replace(' ', "T");
            if let Some(v) = date(&normalized).filter(|v| *v > now) {
                let has_offset = normalized.ends_with('Z')
                    || normalized
                        .get(19..)
                        .is_some_and(|s| s.contains('+') || s.contains('-'));
                if has_offset {
                    consider((v - now) as f64);
                } else {
                    naive = Some(naive.unwrap_or(0).max((v - now) as u64));
                }
            }
        }
    }
    longest.or(naive).or(if immediate { Some(0) } else { None })
}
#[cfg(feature = "test-instrumentation")]
fn scale() -> f64 {
    crate::infrastructure::utils::env_var_trimmed("RUSTAIN_TEST_RESPONSES_RETRY_SCALE")
        .and_then(|v| finite(&v))
        .filter(|v| *v >= 0.0)
        .unwrap_or(1.0)
}
#[cfg(not(feature = "test-instrumentation"))]
fn scale() -> f64 {
    1.0
}
pub(crate) fn delay(ms: u64) -> Duration {
    Duration::from_secs_f64((ms as f64 * scale() / 1000.0).min(86_400.0))
}
pub(crate) fn stream_timeout() -> Duration {
    let scale = scale();
    Duration::from_secs_f64(300.0 * if scale > 0.0 { scale } else { 1.0 })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn header_precedence_dates_and_prose_longest_wins() {
        let now = 1_800_000_000_000;
        let mut h = HeaderMap::new();
        h.insert("retry-after", "400".parse().unwrap());
        assert_eq!(hint(&h, "", now), Some(400000));
        h.insert("retry-after-ms", "12".parse().unwrap());
        assert_eq!(hint(&h, "", now), Some(12));
        h.clear();
        h.insert("x-ratelimit-reset-after", "2.5".parse().unwrap());
        assert_eq!(hint(&h, "", now), Some(2500));
        assert_eq!(
            hint(
                &HeaderMap::new(),
                "Please retry in 2s; try again in 3 min",
                now
            ),
            Some(180000)
        );
        assert_eq!(hint(&HeaderMap::new(), "retry-after-ms=0", now), Some(0));
        h.clear();
        h.insert(
            "retry-after",
            chrono::DateTime::from_timestamp_millis(now + 60000)
                .unwrap()
                .to_rfc2822()
                .parse()
                .unwrap(),
        );
        assert_eq!(hint(&h, "", now), Some(60000));
        h.clear();
        // Number.parseInt keeps the integer prefix of a decimal epoch.
        h.insert(
            "x-ratelimit-reset",
            format!("{}.5", now / 1000 + 30).parse().unwrap(),
        );
        assert_eq!(hint(&h, "", now), Some(30000));
    }
    #[cfg(not(feature = "test-instrumentation"))]
    #[test]
    #[serial_test::serial]
    fn retry_test_knob_is_absent_in_normal_build() {
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                // SAFETY: unkeyed serial test lock outlives restore.
                unsafe {
                    match self.0.take() {
                        Some(v) => std::env::set_var("RUSTAIN_TEST_RESPONSES_RETRY_SCALE", v),
                        None => std::env::remove_var("RUSTAIN_TEST_RESPONSES_RETRY_SCALE"),
                    }
                }
            }
        }
        let _restore = Restore(std::env::var_os("RUSTAIN_TEST_RESPONSES_RETRY_SCALE"));
        // SAFETY: all environment-mutating fixtures take the unkeyed serial lock.
        unsafe {
            std::env::set_var("RUSTAIN_TEST_RESPONSES_RETRY_SCALE", "0");
        }
        assert_eq!(delay(500), Duration::from_millis(500));
        assert_eq!(stream_timeout(), Duration::from_secs(300));
    }
    #[test]
    fn generic_backoff_admission_and_body_read_timeout() {
        let delays: Vec<_> = (1..=9).map(generic_delay).collect();
        assert_eq!(
            delays,
            [500, 1000, 2000, 4000, 8000, 16000, 32000, 60000, 60000]
        );
        assert_eq!(generic_delay(u32::MAX), GENERIC_MAX_DELAY_MS);
        let mut headers = HeaderMap::new();
        assert!(!admission_rejection(
            &headers,
            r#"{"error":{"type":"rate_limit"}}"#
        ));
        assert!(admission_rejection(
            &headers,
            r#"{"error":{"rate_limit_type" : "max_parallel_requests"}}"#
        ));
        headers.insert(
            "rate_limit_type",
            " max_parallel_requests ".parse().unwrap(),
        );
        assert!(admission_rejection(&headers, ""));
        assert!(request_body_read_timeout(
            408,
            "Timed out reading request body"
        ));
        assert!(!request_body_read_timeout(
            504,
            "timed out reading request body"
        ));
        assert!(!request_body_read_timeout(408, "timed out"));
    }
}
