//! The local provider: Ollama's native API (`/api/chat`, `/api/tags`, `/api/ps`).
//! Request and response translation lives in `ollama.rs`.

use std::time::Duration;

use reqwest::{StatusCode, Url};
use serde_json::Value;
use tracing::warn;

use crate::{config::Config, error::ApiError, ollama};

/// Readiness probes get this long beyond the connect timeout, so an
/// unreachable host is reported as unreachable rather than as slow.
const PROBE_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub(crate) struct Ollama {
    http: reqwest::Client,
    chat_url: Url,
    tags_url: Url,
    ps_url: Url,
    model: String,
    num_ctx: Option<u32>,
    request_timeout: Duration,
    probe_timeout: Duration,
}

impl Ollama {
    pub fn new(config: &Config) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            // The upstream is fixed by config; never follow it anywhere else.
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .expect("HTTP client configuration is valid");
        let url = |path| {
            config
                .ollama_base_url
                .join(path)
                .expect("base URL ends in /")
        };
        Ollama {
            http,
            chat_url: url("api/chat"),
            tags_url: url("api/tags"),
            ps_url: url("api/ps"),
            model: config.ollama_model.clone(),
            num_ctx: config.ollama_num_ctx,
            request_timeout: config.request_timeout,
            probe_timeout: config.connect_timeout + PROBE_GRACE,
        }
    }

    /// Send a chat request. The timeout covers the whole exchange, including
    /// reading a streamed body, so it is the overall cap for a request.
    pub async fn chat(&self, body: &Value) -> Result<reqwest::Response, ApiError> {
        let res = self
            .http
            .post(self.chat_url.clone())
            .timeout(self.request_timeout)
            .json(body)
            .send()
            .await
            .map_err(from_reqwest)?;
        let status = res.status();
        if status.is_success() {
            return Ok(res);
        }
        warn!(
            upstream_status = status.as_u16(),
            "upstream rejected chat request"
        );
        let detail = error_detail(res).await;
        // A message naming the cause (context overflow, no tool support)
        // beats the status code, which varies between Ollama versions.
        if let Some(err) = ollama::classify_error(&detail, self.num_ctx) {
            return Err(err);
        }
        Err(match status {
            StatusCode::NOT_FOUND => ApiError::upstream_model_unavailable(),
            StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
                ApiError::upstream_overloaded()
            }
            _ => {
                let detail = if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {}", ollama::short(&detail))
                };
                let message = format!(
                    "local model server returned HTTP {}{detail}",
                    status.as_u16()
                );
                // A 4xx is about this request, not about Ollama being down.
                if status.is_client_error() {
                    ApiError::upstream_rejected(message)
                } else {
                    ApiError::upstream_error(message)
                }
            }
        })
    }

    pub fn num_ctx(&self) -> Option<u32> {
        self.num_ctx
    }

    /// The context length Ollama actually allocated for the model, if it is
    /// loaded right now (`/api/ps`). Never loads the model or wakes the host;
    /// call only after a successful probe.
    pub async fn loaded_context_length(&self) -> Option<u64> {
        let res = self
            .http
            .get(self.ps_url.clone())
            .timeout(self.probe_timeout)
            .send()
            .await
            .ok()?;
        let ps: Value = res.json().await.ok()?;
        ps["models"].as_array()?.iter().find(|m| {
            m["name"]
                .as_str()
                .is_some_and(|n| same_model(n, &self.model))
        })?["context_length"]
            .as_u64()
    }

    pub fn probe_timeout(&self) -> Duration {
        self.probe_timeout
    }

    /// Connectivity check for Wake-on-LAN: does anything answer `/api/tags`?
    /// Any HTTP response means the host is awake (waking it cannot help), so
    /// only transport failures count as unreachable.
    pub async fn reachable(&self, timeout: Duration) -> Result<(), &'static str> {
        let res = self
            .http
            .get(self.tags_url.clone())
            .timeout(timeout)
            .send()
            .await
            .map_err(probe_failure)?;
        if !res.status().is_success() {
            warn!(
                upstream_status = res.status().as_u16(),
                "Ollama is reachable but /api/tags returned an error"
            );
        }
        Ok(())
    }

    /// Bounded readiness check: is Ollama reachable and is the model installed?
    pub async fn probe(&self) -> Result<(), &'static str> {
        let res = self
            .http
            .get(self.tags_url.clone())
            .timeout(self.probe_timeout)
            .send()
            .await
            .map_err(probe_failure)?;
        if !res.status().is_success() {
            return Err("upstream_error");
        }
        let tags: Value = res.json().await.map_err(|_| "upstream_error")?;
        let installed = tags["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["name"].as_str())
            .any(|name| same_model(name, &self.model));
        if installed {
            Ok(())
        } else {
            Err("model_not_installed")
        }
    }
}

fn probe_failure(e: reqwest::Error) -> &'static str {
    if e.is_connect() {
        "upstream_unreachable"
    } else if e.is_timeout() {
        "upstream_timeout"
    } else {
        "upstream_error"
    }
}

/// Ollama lists untagged models with an implicit `:latest`.
fn same_model(installed: &str, wanted: &str) -> bool {
    installed == wanted
        || (!wanted.contains(':') && installed.strip_suffix(":latest") == Some(wanted))
}

// Ollama's error messages are short and describe the failure, not the prompt;
// they go back to the caller (who sent the prompt) but are never logged.
async fn error_detail(res: reqwest::Response) -> String {
    let Ok(body) = res.json::<Value>().await else {
        return String::new();
    };
    body["error"]["message"]
        .as_str()
        .or(body["error"].as_str())
        .unwrap_or_default()
        .to_owned()
}

pub(crate) fn from_reqwest(e: reqwest::Error) -> ApiError {
    // Check connect first: a connect timeout means "unreachable", not "slow".
    if e.is_connect() {
        ApiError::upstream_unavailable()
    } else if e.is_timeout() {
        ApiError::upstream_timeout()
    } else if e.is_decode() {
        ApiError::upstream_invalid_response()
    } else {
        ApiError::upstream_error("connection to local model server failed")
    }
}

#[cfg(test)]
mod tests {
    use super::same_model;

    #[test]
    fn model_names() {
        assert!(same_model("llama3.1:8b", "llama3.1:8b"));
        assert!(same_model("llama3:latest", "llama3"));
        assert!(!same_model("llama3.1:70b", "llama3.1:8b"));
        assert!(!same_model("llama3:latest", "llama3:8b"));
    }
}
