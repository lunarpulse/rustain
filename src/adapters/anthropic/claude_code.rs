//! Claude Code request profile, adapted from oh-my-pi.
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

use super::types::{
    AnthropicContent, AnthropicMetadata, AnthropicRequest, AnthropicSystem, CacheControl,
    SystemBlock,
};
use crate::adapters::install_id::{install_id, uuid};
use crate::domain::errors::ProviderError;
use crate::domain::models::{AuthHeaderScheme, AuthSource, ResolvedAuth};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::sync::LazyLock;

const IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const COMMON: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05";

static ADOPTED: LazyLock<arc_swap::ArcSwapOption<String>> =
    LazyLock::new(arc_swap::ArcSwapOption::empty);

pub(crate) fn adopted_version() -> Option<String> {
    ADOPTED.load_full().map(|version| (*version).clone())
}

fn semver(value: &str) -> Option<[(usize, &str); 3]> {
    let mut parts = value.split('.');
    let mut numeric = [(0, ""); 3];
    for part in &mut numeric {
        let digits = parts.next()?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let digits = digits.trim_start_matches('0');
        *part = (digits.len(), digits);
    }
    parts.next().is_none().then_some(numeric)
}

/// Only a strictly newer wire version earns another attempt (omp fingerprint policy).
pub(super) fn adopt_required(body: &str, sent_version: &str) -> bool {
    if crate::infrastructure::utils::env_var_trimmed("RUSTAIN_CLAUDE_CODE_VERSION").is_some()
        || !body.contains("claude_code_version_too_old")
    {
        return false;
    }
    static REQUIRED: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?i)version (\d+\.\d+\.\d+) or newer is required")
            .expect("valid version regex")
    });
    let Some(required) = REQUIRED.captures(body).and_then(|captures| captures.get(1)) else {
        return false;
    };
    let (Some(next), Some(sent)) = (semver(required.as_str()), semver(sent_version)) else {
        return false;
    };
    if next <= sent {
        return false;
    }
    let required = std::sync::Arc::new(required.as_str().to_owned());
    // Concurrent rejections cannot downgrade the process-wide version.
    ADOPTED.rcu(|old| match old {
        Some(old) if semver(old).is_some_and(|version| version >= next) => Some(old.clone()),
        _ => Some(required.clone()),
    });
    true
}

pub struct AuthPlan {
    pub scheme: AuthHeaderScheme,
    pub profile: bool,
}

/// Pure precedence: credential type, explicit override, named CC env, default.
pub fn anthropic_auth(
    source: &AuthSource,
    auth: &ResolvedAuth,
    cfg_header: Option<AuthHeaderScheme>,
) -> AuthPlan {
    // One consolidated expose site: token shape selects the subscription protocol.
    let profile = matches!(auth, ResolvedAuth::OAuth { .. })
        || auth.bearer_secret().expose_secret().contains("sk-ant-oat");
    let scheme = if profile {
        AuthHeaderScheme::Bearer
    } else {
        cfg_header.unwrap_or_else(|| match source {
            AuthSource::Env { var } if var == "ANTHROPIC_AUTH_TOKEN" => AuthHeaderScheme::Bearer,
            _ => AuthHeaderScheme::ApiKey,
        })
    };
    AuthPlan { scheme, profile }
}

#[derive(Clone)]
pub struct ClaudeCodeIdentity {
    pub version: String,
    pub install_id: String,
    pub session: String,
}
#[cfg(feature = "test-instrumentation")]
static PIN: std::sync::LazyLock<arc_swap::ArcSwapOption<ClaudeCodeIdentity>> =
    std::sync::LazyLock::new(arc_swap::ArcSwapOption::empty);
#[cfg(feature = "test-instrumentation")]
pub fn pin_identity_for_test(identity: ClaudeCodeIdentity) {
    PIN.store(Some(std::sync::Arc::new(identity)));
}
#[cfg(feature = "test-instrumentation")]
pub fn reset_for_test() {
    PIN.store(None);
    ADOPTED.store(None);
}
static SESSION: LazyLock<String> = LazyLock::new(uuid);

fn identity_error() -> ProviderError {
    ProviderError::Other("Could not initialize Claude Code identity".into())
}

pub(super) struct Profile {
    pub version: String,
    pub session: String,
    install_id: Option<String>,
}
impl Profile {
    pub fn new(account: Option<&str>) -> Result<Self, ProviderError> {
        #[cfg(feature = "test-instrumentation")]
        if let Some(pin) = PIN.load_full() {
            return Ok(Self {
                version: crate::infrastructure::utils::env_var_trimmed(
                    "RUSTAIN_CLAUDE_CODE_VERSION",
                )
                .or_else(adopted_version)
                .unwrap_or_else(|| pin.version.clone()),
                session: pin.session.clone(),
                install_id: account.map(|_| pin.install_id.clone()),
            });
        }
        Ok(Self {
            version: crate::adapters::auth::policies::claude_code_version(),
            session: SESSION.clone(),
            install_id: account.map(|_| install_id()).transpose()?,
        })
    }
    pub fn headers(
        &self,
        mut request: reqwest::RequestBuilder,
        tools: bool,
    ) -> reqwest::RequestBuilder {
        let beta = if tools {
            format!(
                "claude-code-20250219,{COMMON},mid-conversation-system-2026-04-07,fallback-credit-2026-06-01"
            )
        } else {
            format!("{COMMON},structured-outputs-2025-12-15")
        };
        let os = match std::env::consts::OS {
            "linux" => "Linux".into(),
            "macos" => "MacOS".into(),
            "windows" => "Windows".into(),
            "freebsd" => "FreeBSD".into(),
            other => format!("Other::{other}"),
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => "x64".into(),
            "aarch64" => "arm64".into(),
            "x86" => "x86".into(),
            other => format!("other::{other}"),
        };
        for (key, value) in [
            ("accept", "application/json"),
            ("content-type", "application/json"),
            ("x-stainless-lang", "js"),
            ("x-stainless-package-version", "0.112.1"),
            ("x-stainless-retry-count", "0"),
            ("x-stainless-runtime", "node"),
            ("x-stainless-runtime-version", "v26.3.0"),
            ("x-stainless-timeout", "600"),
            ("anthropic-dangerous-direct-browser-access", "true"),
            ("x-app", "cli"),
            ("connection", "keep-alive"),
        ] {
            request = request.header(key, value);
        }
        request
            .header(
                "user-agent",
                format!("claude-cli/{} (external, cli)", self.version),
            )
            .header("x-claude-code-session-id", &self.session)
            .header("x-stainless-os", os)
            .header("x-stainless-arch", arch)
            .header("anthropic-beta", beta)
    }
    pub fn apply(
        &self,
        body: &mut AnthropicRequest,
        account: Option<&str>,
        official: bool,
    ) -> Result<(), ProviderError> {
        let cache = CacheControl {
            kind: "ephemeral",
            ttl: official.then_some("1h"),
        };
        let first = body
            .messages
            .iter()
            .find(|m| m.role == "user")
            .and_then(|m| {
                m.content.iter().find_map(|c| match c {
                    AnthropicContent::Text { text } if !text.trim().is_empty() => {
                        Some(text.as_str())
                    }
                    _ => None,
                })
            })
            .unwrap_or("");
        let system = body.system.take();
        let mut blocks = match system {
            Some(AnthropicSystem::Text(text))
                if text.starts_with("x-anthropic-billing-header:") =>
            {
                vec![block(text)]
            }
            other => {
                let billing = format!(
                    "x-anthropic-billing-header: cc_version={}.{}; cc_entrypoint=cli; cch=00000;",
                    self.version,
                    billing_suffix(first, &self.version)
                );
                let mut blocks = vec![block(billing), block(IDENTITY.to_owned())];
                match other {
                    Some(AnthropicSystem::Text(text)) if !text.is_empty() => {
                        blocks.push(block(text))
                    }
                    Some(AnthropicSystem::Blocks(extra)) => blocks.extend(extra),
                    _ => {}
                }
                blocks
            }
        };
        if let Some(last) = blocks.last_mut() {
            last.cache_control = Some(cache.clone());
        }
        body.system = Some(AnthropicSystem::Blocks(blocks));
        let tools = body.tools.get_or_insert_with(Vec::new);
        for tool in tools.iter_mut() {
            prefix(&mut tool.name);
        }
        if let Some(last) = tools.last_mut() {
            last.cache_control = Some(cache);
        }
        for message in &mut body.messages {
            for content in &mut message.content {
                if let AnthropicContent::ToolUse { name, .. } = content {
                    prefix(name);
                }
            }
        }
        #[derive(Serialize)]
        struct UserId<'a> {
            session_id: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            account_uuid: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            device_id: Option<String>,
        }
        let device_id = account
            .zip(self.install_id.as_deref())
            .map(|(account, install)| {
                let mut hash = Sha256::new();
                hash.update(b"rustain-claude-device-id-v2\0");
                hash.update(install.as_bytes());
                hash.update(b"\0");
                hash.update(account.as_bytes());
                hex::encode(hash.finalize())
            });
        let user_id = serde_json::to_string(&UserId {
            session_id: &self.session,
            account_uuid: account,
            device_id,
        })
        .map_err(|_| identity_error())?;
        body.metadata = Some(AnthropicMetadata {
            user_id: Some(user_id),
        });
        Ok(())
    }
}
fn block(text: String) -> SystemBlock {
    SystemBlock {
        kind: "text",
        text,
        cache_control: None,
    }
}
fn prefix(name: &mut String) {
    if !["web_search", "code_execution", "text_editor", "computer"]
        .iter()
        .any(|builtin| name.eq_ignore_ascii_case(builtin))
    {
        name.insert(0, '_');
    }
}
fn billing_suffix(text: &str, version: &str) -> String {
    let mut selected = [b'0' as u16; 3];
    for (i, unit) in text.encode_utf16().take(21).enumerate() {
        match i {
            4 => selected[0] = unit,
            7 => selected[1] = unit,
            20 => selected[2] = unit,
            _ => {}
        }
    }
    let mut hash = Sha256::new();
    hash.update(b"59cf53e54c78");
    for character in char::decode_utf16(selected) {
        let mut utf8 = [0; 4];
        hash.update(
            character
                .unwrap_or(char::REPLACEMENT_CHARACTER)
                .encode_utf8(&mut utf8)
                .as_bytes(),
        );
    }
    hash.update(version.as_bytes());
    let digest = hash.finalize();
    format!("{:02x}{:x}", digest[0], digest[1] >> 4)
}

pub(super) fn attest(bytes: &mut [u8]) {
    const MARKER: &[u8] = b"\"system\":[{\"type\":\"text\",\"text\":\"x-anthropic-billing-header:";
    let Some(marker) = bytes.windows(MARKER.len()).position(|w| w == MARKER) else {
        return;
    };
    let start = marker + MARKER.len();
    let Some(offset) = bytes[start..]
        .windows(9)
        .position(|w| w == b"cch=00000")
        .filter(|&p| p <= 150)
    else {
        tracing::warn!(
            "anthropic: cch billing placeholder present but not patched; sending unattested request"
        );
        return;
    };
    let hash = xxh64(bytes, 0x4d659218e32a3268);
    for i in 0..5 {
        bytes[start + offset + 4 + i] =
            b"0123456789abcdef"[((hash >> ((4 - i) * 4)) & 15) as usize];
    }
}

fn xxh64(bytes: &[u8], seed: u64) -> u64 {
    const P1: u64 = 11400714785074694791;
    const P2: u64 = 14029467366897019727;
    const P3: u64 = 1609587929392839161;
    const P4: u64 = 9650029242287828579;
    const P5: u64 = 2870177450012600261;
    fn round(acc: u64, lane: u64) -> u64 {
        acc.wrapping_add(lane.wrapping_mul(P2))
            .rotate_left(31)
            .wrapping_mul(P1)
    }
    fn lane(bytes: &[u8]) -> u64 {
        u64::from_le_bytes(bytes[..8].try_into().unwrap())
    }
    let mut rest = bytes;
    let mut hash = if bytes.len() >= 32 {
        let mut acc = [
            seed.wrapping_add(P1).wrapping_add(P2),
            seed.wrapping_add(P2),
            seed,
            seed.wrapping_sub(P1),
        ];
        while rest.len() >= 32 {
            for (i, a) in acc.iter_mut().enumerate() {
                *a = round(*a, lane(&rest[i * 8..]));
            }
            rest = &rest[32..];
        }
        let mut h = acc[0]
            .rotate_left(1)
            .wrapping_add(acc[1].rotate_left(7))
            .wrapping_add(acc[2].rotate_left(12))
            .wrapping_add(acc[3].rotate_left(18));
        for a in acc {
            h = (h ^ round(0, a)).wrapping_mul(P1).wrapping_add(P4);
        }
        h
    } else {
        seed.wrapping_add(P5)
    };
    hash = hash.wrapping_add(bytes.len() as u64);
    while rest.len() >= 8 {
        hash = (hash ^ round(0, lane(rest)))
            .rotate_left(27)
            .wrapping_mul(P1)
            .wrapping_add(P4);
        rest = &rest[8..];
    }
    if rest.len() >= 4 {
        hash = (hash ^ (u32::from_le_bytes(rest[..4].try_into().unwrap()) as u64).wrapping_mul(P1))
            .rotate_left(23)
            .wrapping_mul(P2)
            .wrapping_add(P3);
        rest = &rest[4..];
    }
    for &b in rest {
        hash = (hash ^ (b as u64).wrapping_mul(P5))
            .rotate_left(11)
            .wrapping_mul(P1);
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(P2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(P3);
    hash ^ (hash >> 32)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn published_xxh64_vectors() {
        assert_eq!(xxh64(b"", 0), 0xef46db3751d8e999);
        // Upstream XSUM_XXH64_testdata[1], tests/sanity_test_vectors.h.
        assert_eq!(xxh64(b"", 0x9e3779b1), 0xac75fda2929b17ef);
        assert_eq!(xxh64(b"a", 0), 0xd24ec4f1a98c6e5b);
        assert_eq!(xxh64(b"abc", 0x4d659218e32a3268), 0xdfc4f4d6913699b6);
    }
    #[test]
    fn billing_suffix_uses_utf16_indices_and_replacement_for_lone_surrogates() {
        assert_eq!(billing_suffix("hello", "2.1.280"), "790");
        assert_eq!(billing_suffix("hi", "2.1.280"), "d7b");
        assert_eq!(billing_suffix("abcd😀", "2.1.280"), "fc4");
    }
}
