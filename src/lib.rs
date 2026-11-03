//! OpenGate: an authenticated, OpenAI-compatible gateway, local-first.
//!
//! Every request goes to one local Ollama model, waking its PC with
//! Wake-on-LAN if needed. Only when local inference is unavailable, and only
//! if enabled, OpenGate falls back to the signed-in user's ChatGPT plan via
//! the official Sign in with ChatGPT flow. There is no OpenAI API-key or API
//! billing path anywhere in OpenGate.

mod app;
pub mod chatgpt;
mod config;
mod error;
mod fallback;
mod ollama;
mod relay;
mod upstream;
mod validate;
mod wol;

pub use app::app;
pub use config::{
    ChatGptConfig, Config, CredentialsKey, MacAddress, WolConfig, WolRelayConfig, WolTarget,
};
pub use wol::WolRelay;
