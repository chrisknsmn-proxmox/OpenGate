//! OpenAI-style error responses. Messages never contain prompt content or secrets.
//!
//! Status codes let clients tell server-side failures (503/504/502, worth
//! retrying later) from caller mistakes (400, terminal). Errors raised by the
//! local provider also record whether OpenGate's own ChatGPT plan fallback may
//! take over (`fallback_reason`); see `fallback.rs`.

use std::borrow::Cow;

use axum::{
    body::Bytes,
    extract::rejection::BytesRejection,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use crate::chatgpt::CloudError;

#[derive(Debug)]
pub(crate) struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    message: Cow<'static, str>,
    pub param: Option<String>,
    /// Set on local-provider failures that happened before any output was
    /// produced, naming why local inference is unavailable. Only these may
    /// fall back to the ChatGPT plan; everything else is returned as is.
    pub fallback_reason: Option<&'static str>,
    /// Per-provider outcome, included in the body when a fallback was tried.
    providers: Option<Value>,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<Cow<'static, str>>) -> Self {
        ApiError {
            status,
            code,
            message: message.into(),
            param: None,
            fallback_reason: None,
            providers: None,
        }
    }

    fn local(mut self, reason: &'static str) -> Self {
        self.fallback_reason = Some(reason);
        self
    }

    pub fn with_param(mut self, param: impl Into<String>) -> Self {
        self.param = Some(param.into());
        self
    }

    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "missing or invalid API key",
        )
    }

    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "no such endpoint")
    }

    pub fn invalid_request(code: &'static str, message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    pub fn from_body_rejection(rejection: BytesRejection) -> Self {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            Self::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "request body is too large",
            )
        } else {
            Self::invalid_request("invalid_request", "could not read request body")
        }
    }

    pub fn overloaded() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "gateway_overloaded",
            "too many concurrent requests; retry shortly",
        )
    }

    pub fn upstream_unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream_unavailable",
            "local model server is unreachable",
        )
        .local("ollama_unavailable")
    }

    /// Same response as [`Self::upstream_unavailable`], so existing clients
    /// see no change, but the fallback reports that waking the PC failed.
    pub fn wol_timeout() -> Self {
        Self::upstream_unavailable().local("wol_timeout")
    }

    pub fn upstream_model_unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream_model_unavailable",
            "local model is not available on the model server",
        )
        .local("ollama_unavailable")
    }

    pub fn upstream_overloaded() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream_overloaded",
            "local model server is busy",
        )
        .local("ollama_unavailable")
    }

    pub fn upstream_timeout() -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            "upstream_timeout",
            "local model server timed out",
        )
        .local("ollama_unavailable")
    }

    /// The local server failed (5xx or transport error).
    pub fn upstream_error(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, "upstream_error", message).local("ollama_unavailable")
    }

    /// The local server refused this particular request (4xx). The model is
    /// up, so this is not a reason to fall back.
    pub fn upstream_rejected(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, "upstream_error", message)
    }

    pub fn upstream_invalid_response() -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            "upstream_invalid_response",
            "local model server returned an unexpected response",
        )
        .local("ollama_unavailable")
    }

    /// The prompt does not fit the local model's context window. Worded so
    /// that clients which compact on overflow (OpenClaw) recognise it. A 4xx:
    /// retrying the same request elsewhere is not a fix for its size.
    pub fn context_length_exceeded(num_ctx: Option<u32>, detail: &str) -> Self {
        let window = match num_ctx {
            Some(n) => format!("the {n}-token context window"),
            None => "the context window".to_owned(),
        };
        Self::new(
            StatusCode::BAD_REQUEST,
            "context_length_exceeded",
            format!(
                "input exceeds {window} of the local model; shorten or compact the \
                 conversation, or raise OLLAMA_NUM_CTX if memory allows (Ollama: {detail})"
            ),
        )
    }

    /// The configured Ollama model cannot do tool calling at all.
    pub fn local_tools_unsupported() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "tools_not_supported",
            "the local model does not support tool calling; set OLLAMA_MODEL to a \
             tools-capable model, or send the request without tools",
        )
    }

    /// The model produced a tool call OpenGate cannot pass on faithfully
    /// (no function name, arguments that are not a JSON object).
    pub fn upstream_invalid_tool_call() -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            "upstream_invalid_tool_call",
            "the local model produced a malformed tool call",
        )
    }

    /// The model's answer does not honour `tool_choice` or
    /// `parallel_tool_calls: false`, which the local model cannot enforce.
    pub fn tool_choice_not_satisfied(message: &'static str) -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            "tool_choice_not_satisfied",
            message,
        )
    }

    pub fn stream_incomplete() -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            "upstream_stream_incomplete",
            "local model stream ended before completion",
        )
    }

    /// A ChatGPT plan failure, for use mid-stream or when the local model was
    /// not involved. Codes are listed in README "Errors".
    pub fn cloud(e: &CloudError) -> Self {
        let mut err = Self::new(e.status(), e.code(), e.message());
        err.param = e.param().map(str::to_owned);
        err
    }

    /// Local inference was unavailable and the ChatGPT plan fallback failed
    /// too. A usage limit keeps its own 429 code so it is easy to act on.
    pub fn all_providers_unavailable(local: &ApiError, cloud: &CloudError) -> Self {
        let local_reason = local.fallback_reason.unwrap_or("ollama_unavailable");
        let message = format!(
            "local model unavailable ({local_reason}: {}); ChatGPT plan fallback failed ({}: {})",
            local.message,
            cloud.code(),
            cloud.message()
        );
        let mut err = match cloud {
            CloudError::LimitReached => Self::new(
                StatusCode::TOO_MANY_REQUESTS,
                "subscription_limit_reached",
                message,
            ),
            _ => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "all_providers_unavailable",
                message,
            ),
        };
        err.param = cloud.param().map(str::to_owned);
        err.providers = Some(json!({ "ollama": local_reason, "chatgpt": cloud.code() }));
        err
    }

    fn body(&self) -> Value {
        let kind = match self.status {
            StatusCode::UNAUTHORIZED => "authentication_error",
            StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
            s if s.is_client_error() => "invalid_request_error",
            _ => "server_error",
        };
        let mut body = json!({
            "error": { "message": self.message, "type": kind, "code": self.code, "param": self.param }
        });
        if let Some(providers) = &self.providers {
            body["error"]["providers"] = providers.clone();
        }
        body
    }

    /// The error as one SSE frame, for failures after a stream has started.
    pub fn sse_frame(&self) -> Bytes {
        Bytes::from(format!("data: {}\n\n", self.body()))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut res = (self.status, Json(self.body())).into_response();
        let headers = res.headers_mut();
        match self.code {
            "invalid_api_key" => {
                headers.insert(
                    header::WWW_AUTHENTICATE,
                    header::HeaderValue::from_static("Bearer"),
                );
            }
            "gateway_overloaded" => {
                headers.insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
            }
            _ => {}
        }
        res
    }
}
