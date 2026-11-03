//! Local-first routing.
//!
//! 1. Local Ollama is always tried first: with Wake-on-LAN enabled, the PC is
//!    woken and awaited exactly as before.
//! 2. Only if local inference is *unavailable* (unreachable, wake timed out,
//!    model missing, overloaded, timed out, 5xx) and the ChatGPT fallback is
//!    enabled is the request sent to the ChatGPT plan. A request the local
//!    model rejected (4xx) or a slow-but-successful answer never falls back,
//!    and nothing looks at the prompt to decide.
//! 3. The commit point is Ollama returning HTTP 200: from then on the request
//!    belongs to Ollama, and a failure mid-stream is reported, never retried
//!    elsewhere (the client may already have received part of the answer).
//! 4. If the fallback fails too, the client gets one error naming both
//!    outcomes (`all_providers_unavailable`, or `subscription_limit_reached`).
//!
//! 5. Fallback carries the whole conversation, tool calls and tool results
//!    included, to the ChatGPT plan. Nothing has reached the client before the
//!    switch, so no tool call is ever announced twice.
//!
//! With the fallback disabled, behaviour and error codes are unchanged.

use tracing::{info, warn};

use crate::{
    chatgpt::{ChatGpt, Opened},
    error::ApiError,
    ollama,
    upstream::Ollama,
    validate::ChatRequest,
    wol::Wol,
};

pub(crate) enum Routed {
    Local(reqwest::Response),
    /// `local` is why Ollama could not serve it, for a later error message.
    Cloud {
        opened: Opened,
        local: ApiError,
    },
}

pub(crate) struct Providers<'a> {
    pub ollama: &'a Ollama,
    pub ollama_model: &'a str,
    pub wol: Option<&'a Wol>,
    pub chatgpt: Option<&'a ChatGpt>,
}

pub(crate) async fn route(p: Providers<'_>, req: &ChatRequest) -> Result<Routed, ApiError> {
    // A request the local model cannot take as written (a 400) is refused
    // before anything is woken, and never falls back.
    let body = ollama::request_body(req, p.ollama_model, p.ollama.num_ctx())?;
    let local = async {
        // Only authenticated, valid inference requests reach this point, so
        // nothing else can wake the Ollama host. Covers streaming and not.
        if let Some(wol) = p.wol {
            wol.ensure_ready().await?;
        }
        p.ollama.chat(&body).await
    };
    let local = match local.await {
        Ok(res) => return Ok(Routed::Local(res)),
        Err(e) => e,
    };
    let (Some(chatgpt), Some(reason)) = (p.chatgpt, local.fallback_reason) else {
        return Err(local);
    };

    info!(
        reason,
        local_code = local.code,
        "local inference unavailable; trying the ChatGPT plan fallback"
    );
    match chatgpt.open(req).await {
        Ok(opened) => {
            info!(
                provider = "chatgpt",
                "serving request from the ChatGPT plan"
            );
            Ok(Routed::Cloud { opened, local })
        }
        Err(cloud) => {
            warn!(
                ollama = reason,
                chatgpt = cloud.code(),
                "no provider could serve the request"
            );
            Err(ApiError::all_providers_unavailable(&local, &cloud))
        }
    }
}
