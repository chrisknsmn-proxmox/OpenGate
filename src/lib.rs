//! OpenGate: an authenticated, OpenAI-compatible gateway to one local Ollama model.
//!
//! OpenGate never calls a cloud provider. Cloud fallback, if any, belongs to
//! the client (OpenClaw), which decides based on the errors returned here.

mod app;
mod config;
mod error;
mod relay;
mod upstream;
mod validate;

pub use app::app;
pub use config::Config;
