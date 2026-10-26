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

/// omp `parseCallbackInput`: URL, then any `code=` query, then `code#state` or a bare code.
pub(super) fn parse(input: &str, expected: &str) -> Option<String> {
    let input = input.trim();
    fn query(pairs: url::form_urlencoded::Parse<'_>) -> Option<(String, Option<String>)> {
        let query: std::collections::HashMap<_, _> = pairs.into_owned().collect();
        Some((query.get("code")?.clone(), query.get("state").cloned()))
    }
    let (code, state) = if let Ok(url) = url::Url::parse(input) {
        query(url.query_pairs())?
    } else if input.contains("code=") {
        let input = input.strip_prefix(['?', '#']).unwrap_or(input);
        query(url::form_urlencoded::parse(input.as_bytes()))?
    } else if let Some((code, state)) = input.split_once('#') {
        (code.to_owned(), Some(state.to_owned()))
    } else {
        (input.to_owned(), None)
    };
    // A supplied, non-empty state must match; an absent or empty one is accepted.
    let wrong_state = state.is_some_and(|s| !s.is_empty() && s != expected);
    if code.trim().is_empty() || wrong_state {
        return None;
    }
    Some(code.trim().to_owned())
}
