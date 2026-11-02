//! OpenAI-style error responses. Messages never contain prompt content or secrets.
//!
//! Status codes are chosen so OpenClaw's model failover can tell local-side
//! failures (503/504/502, failover-eligible) from caller mistakes (400, terminal).

use std::borrow::Cow;

use axum::{
    body::Bytes,
    extract::rejection::BytesRejection,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

#[derive(Debug)]
pub(crate) struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    message: Cow<'static, str>,
    pub param: Option<String>,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<Cow<'static, str>>) -> Self {
        ApiError {
            status,
            code,
            message: message.into(),
            param: None,
        }
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
    }

    pub fn upstream_model_unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream_model_unavailable",
            "local model is not available on the model server",
        )
    }

    pub fn upstream_overloaded() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream_overloaded",
            "local model server is busy",
        )
    }

    pub fn upstream_timeout() -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            "upstream_timeout",
            "local model server timed out",
        )
    }

    pub fn upstream_error(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, "upstream_error", message)
    }

    pub fn upstream_invalid_response() -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            "upstream_invalid_response",
            "local model server returned an unexpected response",
        )
    }

    pub fn stream_incomplete() -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            "upstream_stream_incomplete",
            "local model stream ended before completion",
        )
    }

    fn body(&self) -> Value {
        let kind = match self.status {
            StatusCode::UNAUTHORIZED => "authentication_error",
            s if s.is_client_error() => "invalid_request_error",
            _ => "server_error",
        };
        json!({
            "error": { "message": self.message, "type": kind, "code": self.code, "param": self.param }
        })
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
