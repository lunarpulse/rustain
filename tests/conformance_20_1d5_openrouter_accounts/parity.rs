//! The inherited Lane B parity keystones that ride the same seams as the
//! OpenRouter routes: the Chat stream terminal (K14), Copilot Messages retry
//! parity (K15), the Copilot/Gemini CLI catalogue (K16), per-account Copilot
//! roots (K17) and the Gemini schema degrade residual (K19).

#[path = "parity/catalog.rs"]
mod catalog;
#[path = "parity/chat_terminal.rs"]
mod chat_terminal;
#[path = "parity/messages_retry.rs"]
mod messages_retry;
#[path = "parity/roots.rs"]
mod roots;
#[path = "parity/schema.rs"]
mod schema;
