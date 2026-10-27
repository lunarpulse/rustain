//! Provider authentication policy, independent of HTTP features.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AuthMethod {
    ApiKey,
    OAuthCode(&'static OAuthCodePolicy),
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

#[derive(Debug, Clone, Copy)]
pub enum BodyEncoding {
    Json,
    Form,
}

#[derive(Debug)]
pub struct TokenRequestPolicy {
    pub url: &'static str,
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
}

/// PKCE is intrinsic: a confidential client secret cannot be represented.
#[derive(Debug)]
pub struct OAuthCodePolicy {
    pub label: &'static str,
    pub login_name: &'static str,
    pub instructions: &'static str,
    pub paste_prompt: &'static str,
    pub client_id: ClientId,
    pub authorize_url: &'static str,
    pub scope: &'static str,
    pub authorize_extra: &'static [(&'static str, Tpl)],
    pub port: u16,
    pub port_fallback: bool,
    pub callback_path: &'static str,
    pub paste: bool,
    pub exchange: &'static TokenRequestPolicy,
    pub projection: &'static TokenProjection,
    pub identity_hook: Option<IdentityHook>,
}

#[derive(Debug)]
pub struct RefreshPolicy {
    pub client_id: ClientId,
    pub request: &'static TokenRequestPolicy,
    pub projection: &'static TokenProjection,
    pub identity_hook: Option<IdentityHook>,
}

#[derive(Debug)]
pub struct OAuthApiPolicy {
    pub api_origin: &'static str,
    pub refresh: &'static RefreshPolicy,
}
