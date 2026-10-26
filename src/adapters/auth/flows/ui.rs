use crate::domain::models::SecretString;
use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

/// Host-owned interaction; engines never access a terminal or standard streams.
pub trait LoginUi: Send + Sync {
    fn on_auth_url(&self, url: &str, launch_url: &str, instructions: &str);
    fn on_progress(&self, message: &str);
    fn prompt_secret(&self, label: &str) -> BoxFuture<'_, Option<SecretString>>;
    fn manual_input(&self, prompt: &str) -> Option<BoxFuture<'static, Option<String>>>;
    fn cancel_token(&self) -> CancellationToken;
}
