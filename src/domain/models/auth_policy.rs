//! Provider authentication policy, independent of HTTP features.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AuthMethod {
    ApiKey,
    OAuthCode(&'static OAuthCodePolicy),
    /// Headless device login; the stored row is an OAuth credential.
    Device(&'static DevicePolicy),
}
impl AuthMethod {
    /// Operator-facing name for interactive method pickers.
    pub fn label(&self) -> &'static str {
        match self {
            Self::ApiKey => "API key",
            Self::OAuthCode(policy) => policy.label,
            Self::Device(policy) => policy.label,
        }
    }
    /// Product name in the success line of a subscription login; API keys have none.
    pub fn login_name(&self) -> Option<&'static str> {
        match self {
            Self::ApiKey => None,
            Self::OAuthCode(policy) => Some(policy.login_name),
            Self::Device(policy) => Some(policy.login_name),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Tpl {
    Lit(&'static str),
    ClientId,
    RedirectUri,
    Scope,
    State,
    CodeChallenge,
    Code,
    CodeVerifier,
    RefreshToken,
}

#[derive(Debug, Clone, Copy)]
pub enum ClientId {
    Literal(&'static str),
    Base64(&'static str),
}

/// An installed-app registration value shipped in the binary; Base64 is obfuscation,
/// not secrecy. Decoded only into the token request body; `Debug` never shows it.
#[derive(Clone, Copy)]
pub enum ClientSecret {
    Base64(&'static str),
}
impl std::fmt::Debug for ClientSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClientSecret([redacted])")
    }
}

/// PKCE and a client secret are mutually exclusive by construction.
#[derive(Debug, Clone, Copy)]
pub enum OAuthClient {
    /// Public client: S256 challenge on the authorization URL, verifier on exchange.
    PublicPkce,
    /// Confidential installed-app client: no PKCE, the secret on exchange and refresh.
    Confidential { secret: ClientSecret },
}

/// Where the browser callback listener binds and what the redirect names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackHost {
    /// `localhost` on IPv4 and, when available, IPv6.
    Localhost,
    /// `127.0.0.1` only; the redirect names `127.0.0.1`.
    Ipv4Loopback,
}

/// Provider work that must succeed after the code exchange before a login counts.
#[derive(Debug, Clone, Copy)]
pub enum PostExchangeHook {
    /// Cloud Code Assist project discovery or onboarding.
    GeminiCliProject,
}

#[derive(Debug, Clone, Copy)]
pub enum BodyEncoding {
    Json,
    Form,
}

/// Where a token request is sent. Callers resolve it per use (login or refresh), so a
/// discovered endpoint is never cached across operations.
#[derive(Debug, Clone, Copy)]
pub enum TokenUrl {
    Static(&'static str),
    /// Discovered from xAI's OpenID configuration and validated before any request.
    XaiOidc,
}

#[derive(Debug)]
pub struct TokenRequestPolicy {
    pub url: TokenUrl,
    pub encoding: BodyEncoding,
    pub timeout_ms: u64,
    pub headers: &'static [(&'static str, Tpl)],
    pub extra_params: &'static [(&'static str, Tpl)],
}

#[derive(Debug)]
pub struct TokenProjection {
    pub access: &'static str,
    pub refresh: &'static str,
    pub expires_in: &'static str,
    pub skew_ms: i64,
    pub account_id: Option<&'static str>,
    pub email: Option<&'static str>,
    pub project_id: Option<&'static str>,
    pub org_id: Option<&'static str>,
    pub org_name: Option<&'static str>,
}

#[derive(Debug, Clone, Copy)]
pub enum IdentityHook {
    AnthropicBootstrap,
    CodexJwt,
    /// Access-JWT `sub`, then a best-effort xAI OIDC userinfo lookup.
    XaiUserinfo,
    /// Best-effort Google userinfo `email`; login only.
    GoogleUserinfo,
}

#[derive(Debug)]
pub struct OAuthCodePolicy {
    pub label: &'static str,
    pub login_name: &'static str,
    pub instructions: &'static str,
    pub paste_prompt: &'static str,
    pub client_id: ClientId,
    pub client: OAuthClient,
    pub authorize_url: &'static str,
    pub scope: &'static str,
    pub authorize_extra: &'static [(&'static str, Tpl)],
    pub port: u16,
    pub port_fallback: bool,
    pub callback_host: CallbackHost,
    pub callback_path: &'static str,
    pub paste: bool,
    pub exchange: &'static TokenRequestPolicy,
    pub projection: &'static TokenProjection,
    pub identity_hook: Option<IdentityHook>,
    pub post_exchange: Option<PostExchangeHook>,
}

/// One login method, two device protocols sharing display, time, cancellation and storage.
#[derive(Debug)]
pub struct DevicePolicy {
    pub label: &'static str,
    pub login_name: &'static str,
    pub flow: DeviceFlow,
}

#[derive(Debug, Clone, Copy)]
pub enum DeviceFlow {
    Rfc8628(&'static Rfc8628Policy),
    /// OpenAI's custom device-authorization protocol; its constants are protocol-fixed.
    OpenAiCodex,
    /// GitHub Copilot's custom device hook (Enterprise prompt, routing probe and
    /// model-policy enablement); its constants are protocol-fixed.
    GitHubCopilot,
}

/// RFC 8628 device authorization grant.
#[derive(Debug)]
pub struct Rfc8628Policy {
    pub client_id: ClientId,
    /// Already joined by the provider's scope separator.
    pub scope: &'static str,
    pub device_url: &'static str,
    pub device_headers: &'static [(&'static str, Tpl)],
    /// Encoding of the device-authorization request; polls follow `token.encoding`.
    pub encoding: BodyEncoding,
    pub timeout_ms: u64,
    pub token: &'static TokenRequestPolicy,
    pub projection: &'static TokenProjection,
    pub identity_hook: Option<IdentityHook>,
    /// Displayed instructions; `{user_code}` is replaced by the issued code.
    pub instructions: &'static str,
}

#[derive(Debug)]
pub struct RefreshPolicy {
    pub client_id: ClientId,
    /// Sent only by confidential clients.
    pub client_secret: Option<ClientSecret>,
    pub request: &'static TokenRequestPolicy,
    pub projection: &'static TokenProjection,
    pub identity_hook: Option<IdentityHook>,
    /// A row without `project_id` fails before any HTTP.
    pub require_project_id: bool,
}

/// How a stored grant is renewed.
#[derive(Debug, Clone, Copy)]
pub enum Refresh {
    /// A token-endpoint refresh request.
    Http(&'static RefreshPolicy),
    /// No HTTP: the stored long-lived grant is re-served as the access token with
    /// a new lifetime (GitHub Copilot).
    StoredGrant { lifetime_ms: i64 },
}

#[derive(Debug)]
pub struct OAuthApiPolicy {
    pub api_origin: &'static str,
    /// Stored grants may also leave for an `https` host equal to this suffix or
    /// ending with `.<suffix>` (account-specific API hosts).
    pub api_host_suffix: Option<&'static str>,
    pub refresh: Refresh,
}
