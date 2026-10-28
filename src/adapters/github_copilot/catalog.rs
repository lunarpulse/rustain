//! GitHub Copilot route table, bundled model limits and limit pins, adapted
//! from oh-my-pi's catalog (models.json, compat/rules/runtime/behavior.kdl,
//! compat/rules/providers/github-copilot.kdl).
//!
//! MIT License
//!
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

/// The wire API one model id is served on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// Anthropic Messages (`/v1/messages`).
    Messages,
    /// OpenAI Responses (`/responses`).
    Responses,
    /// OpenAI Chat Completions (`/chat/completions`).
    Chat,
}

/// omp `api-routes provider="github-copilot"`, evaluated in order on the
/// lowercased id: Claude family globs → Messages; Grok 4.x, GPT-5/6, `oswe`,
/// MAI prefixes → Responses; everything else → Chat Completions.
pub fn route(model: &str) -> Route {
    let id = model.to_ascii_lowercase();
    const MESSAGES: [&str; 5] = [
        "claude-haiku-",
        "claude-sonnet-",
        "claude-opus-",
        "claude-fable-",
        "claude-mythos-",
    ];
    const RESPONSES: [&str; 5] = ["grok-4.", "gpt-5", "gpt-6", "oswe", "mai-"];
    if MESSAGES.iter().any(|prefix| id.starts_with(prefix)) {
        Route::Messages
    } else if RESPONSES.iter().any(|prefix| id.starts_with(prefix)) {
        Route::Responses
    } else {
        Route::Chat
    }
}

/// One bundled row: output limit, whether sampling parameters are accepted and
/// whether its Chat Completions request sends `"store": false`.
#[derive(Debug, PartialEq, Eq)]
pub struct Row {
    pub id: &'static str,
    pub max_output: u32,
    pub sampling: bool,
    pub store: bool,
}

const fn row(id: &'static str, max_output: u32, sampling: bool, store: Option<bool>) -> Row {
    Row {
        id,
        max_output,
        sampling,
        store: matches!(store, Some(true)),
    }
}

/// `models.json` `.["github-copilot"]` in file order (`maxTokens`,
/// `compat.supportsSamplingParams`, `compat.supportsStore`).
pub static SEED: [Row; 55] = [
    row("claude-fable-5", 128000, false, None),
    row("claude-fable-5.1", 128000, false, None),
    row("claude-haiku-4.5", 64000, true, None),
    row("claude-haiku-5.5", 128000, false, None),
    row("claude-opus-4.5", 32000, true, None),
    row("claude-opus-4.6", 32000, true, None),
    row("claude-opus-4.7", 32000, false, None),
    row("claude-opus-4.8", 64000, false, None),
    row("claude-opus-5", 64000, false, None),
    row("claude-opus-5.5", 128000, false, None),
    row("claude-sonnet-4", 16000, true, None),
    row("claude-sonnet-4.5", 32000, true, None),
    row("claude-sonnet-4.6", 32000, true, None),
    row("claude-sonnet-5", 128000, false, None),
    row("claude-sonnet-5.5", 128000, false, None),
    row("gemini-2.5-pro", 64000, true, Some(true)),
    row("gemini-3-flash-preview", 64000, true, Some(true)),
    row("gemini-3-pro-preview", 64000, true, Some(true)),
    row("gemini-3.1-pro-preview", 64000, true, Some(false)),
    row("gemini-3.5-flash", 64000, true, Some(false)),
    row("gemini-3.6-flash", 64000, true, Some(false)),
    row("gemini-3.7-flash", 64000, true, Some(false)),
    row("gemini-3.8-flash", 64000, true, Some(false)),
    row("gemini-4-argon", 65536, true, Some(false)),
    row("gpt-4.1", 16384, true, Some(false)),
    row("gpt-4o", 4096, true, Some(true)),
    row("gpt-5", 128000, false, None),
    row("gpt-5-mini", 64000, false, None),
    row("gpt-5.1", 64000, false, None),
    row("gpt-5.1-codex", 128000, false, None),
    row("gpt-5.1-codex-max", 128000, false, None),
    row("gpt-5.1-codex-mini", 128000, false, None),
    row("gpt-5.2", 128000, false, None),
    row("gpt-5.2-codex", 128000, false, None),
    row("gpt-5.3-codex", 128000, false, None),
    row("gpt-5.4", 128000, false, None),
    row("gpt-5.4-mini", 128000, false, None),
    row("gpt-5.4-nano", 128000, false, None),
    row("gpt-5.5", 128000, false, None),
    row("gpt-5.6-luna", 128000, false, None),
    row("gpt-5.6-sol", 128000, false, None),
    row("gpt-5.6-terra", 128000, false, None),
    row("gpt-6-astra", 128000, false, None),
    row("gpt-6-luna", 128000, false, None),
    row("gpt-6-sol", 128000, false, None),
    row("gpt-6.1-sol", 128000, false, None),
    row("grok-4.5", 128000, true, None),
    row("grok-4.6", 128000, true, None),
    row("grok-4.7", 128000, true, None),
    row("grok-code-fast-1", 64000, true, Some(true)),
    row("kimi-k2.7-code", 32000, true, Some(false)),
    row("kimi-k3", 131072, true, Some(false)),
    row("mai-code-1-flash-picker", 128000, true, None),
    row("mai-code-1.1-flash", 128000, true, None),
    row("raptor-mini", 128000, true, Some(true)),
];

pub fn seed_row(id: &str) -> Option<&'static Row> {
    SEED.iter().find(|row| row.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_table_covers_every_seed_id_and_the_edges() {
        let expected = |id: &str| {
            if id.starts_with("claude-") {
                Route::Messages
            } else if id.starts_with("gpt-5")
                || id.starts_with("gpt-6")
                || id.starts_with("grok-4.")
                || id.starts_with("mai-")
            {
                Route::Responses
            } else {
                Route::Chat
            }
        };
        for row in &SEED {
            assert_eq!(route(row.id), expected(row.id), "{}", row.id);
        }
        for (id, want) in [
            ("claude-3-5-sonnet", Route::Chat),
            ("claude-mythos-1", Route::Messages),
            ("grok-4.6-1m", Route::Responses),
            ("grok-code-fast-1", Route::Chat),
            ("oswe-x", Route::Responses),
            ("GPT-5.5", Route::Responses),
            ("gpt-4.1", Route::Chat),
            ("gpt-4o", Route::Chat),
            ("gemini-3.1-pro-preview", Route::Chat),
            ("kimi-k3", Route::Chat),
            ("raptor-mini", Route::Chat),
            ("claude-opus-5-1m", Route::Messages),
        ] {
            assert_eq!(route(id), want, "{id}");
        }
    }

    #[test]
    fn seed_table_equals_the_bundled_seed_json() {
        let seed = crate::adapters::model_catalog_cache::load_embedded_seed()
            .and_then(|seed| seed.providers.get("github-copilot"))
            .expect("bundled Copilot seed");
        let ids: Vec<_> = seed
            .models
            .iter()
            .map(|m| m.descriptor.model_id.as_str())
            .collect();
        let table: Vec<_> = SEED.iter().map(|row| row.id).collect();
        assert_eq!(ids, table);
        let contexts: Vec<_> = seed
            .models
            .iter()
            .map(|m| (m.descriptor.model_id.as_str(), m.descriptor.context_window))
            .collect();
        for (id, context) in [
            ("claude-opus-4.6", 168_000),
            ("claude-sonnet-4", 216_000),
            ("gpt-5.5", 1_050_000),
            ("gemini-4-argon", 1_048_576),
            ("grok-code-fast-1", 192_000),
            ("raptor-mini", 400_000),
        ] {
            assert!(contexts.contains(&(id, context)), "{id}");
        }
        assert_eq!(seed_row("gpt-4o").map(|r| r.max_output), Some(4096));
        assert!(seed_row("grok-code-fast-1").is_some_and(|r| r.store && r.sampling));
        assert!(seed_row("gemini-3.1-pro-preview").is_some_and(|r| !r.store));
        assert!(seed_row("gpt-5.5").is_some_and(|r| !r.sampling));
    }
}
