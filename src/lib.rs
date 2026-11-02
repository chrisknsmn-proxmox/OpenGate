//! OpenGate: an authenticated, OpenAI-compatible gateway to one local Ollama model.
//!
//! OpenGate never calls a cloud provider. Cloud fallback, if any, belongs to
//! the client (OpenClaw), which decides based on the errors returned here.
//! If the Ollama host is asleep, OpenGate can wake it with Wake-on-LAN.

mod app;
mod config;
mod error;
mod relay;
mod upstream;
mod validate;
mod wol;

pub use app::app;
pub use config::{Config, MacAddress, WolConfig, WolRelayConfig, WolTarget};
pub use wol::WolRelay;
