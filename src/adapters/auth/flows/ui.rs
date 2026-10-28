use crate::domain::models::SecretString;
use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

/// Host-owned interaction; engines never access a terminal or standard streams.
pub trait LoginUi: Send + Sync {
    fn on_auth_url(&self, url: &str, launch_url: &str, instructions: &str);
    /// Device login: show `instructions` (which carry `user_code`) and the URL to visit.
    fn on_device_code(&self, verification_url: &str, user_code: &str, instructions: &str);
    fn on_progress(&self, message: &str);
    fn prompt_secret(&self, label: &str) -> BoxFuture<'_, Option<SecretString>>;
    fn manual_input(&self, prompt: &str) -> Option<BoxFuture<'static, Option<String>>>;
    /// One plain, non-secret line; works without a terminal (piped input).
    /// `prompt` is shown verbatim without a newline. End of input yields `None`.
    fn prompt_line(&self, prompt: &str) -> BoxFuture<'_, Option<String>>;
    fn cancel_token(&self) -> CancellationToken;
}
