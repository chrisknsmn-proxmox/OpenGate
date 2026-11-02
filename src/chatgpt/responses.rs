//! Chat Completions ⇄ Responses translation for the ChatGPT plan route.
//!
//! Requests: messages map one-to-one to Responses `input` items (system
//! prompts become `developer` messages, since explicit system items are
//! rejected on this route). Every request is sent with `store: false` and
//! `stream: true`, as the route requires. Parameters the route does not
//! accept (temperature, top_p, max_output_tokens, …) are rejected with
//! `unsupported_request` rather than silently dropped; sampling hints can be
//! dropped instead with `CHATGPT_DROP_UNSUPPORTED_SAMPLING=true`.
//!
//! Responses: `response.output_text.delta` events become chat chunks.
//! Success is only `response.completed` (or `response.incomplete`, which
//! maps to a `length`/`content_filter` finish reason); a `response.failed`,
//! an `error` event, a timeout or EOF ends the stream with one error frame
//! and no `[DONE]`, like the Ollama relay. A non-streaming client gets the
//! stream aggregated into one `chat.completion`.

use std::{
    io,
    pin::Pin,
    time::{Duration, Instant},
};

use axum::body::Bytes;
use futures_util::{stream, Stream, StreamExt};
use serde_json::{json, Map, Value};
use tokio::sync::OwnedSemaphorePermit;
use tracing::{info, warn};

use super::{store::unix_now, CloudError, Opened};
use crate::{error::ApiError, validate::ChatRequest};

const MAX_LINE_BYTES: usize = 1 << 20;
/// Sampling hints: changing them alters style, not the shape of the answer.
const SAMPLING: &[&str] = &[
    "temperature",
    "top_p",
    "presence_penalty",
    "frequency_penalty",
    "seed",
];

/// Returns the Responses body (without `model`) and whether usage was
/// requested via `stream_options.include_usage`.
pub(crate) fn translate(
    req: &ChatRequest,
    drop_sampling: bool,
) -> Result<(Value, bool), CloudError> {
    let fields = req.fields();
    for key in fields.keys() {
        match key.as_str() {
            "messages" | "stream_options" => {}
            k if drop_sampling && SAMPLING.contains(&k) => {}
            k => {
                let hint = if SAMPLING.contains(&k) {
                    " (set CHATGPT_DROP_UNSUPPORTED_SAMPLING=true to drop it on fallback)"
                } else {
                    ""
                };
                return Err(CloudError::Unsupported {
                    param: k.to_owned(),
                    message: format!("{k} is not supported by the ChatGPT plan route{hint}").into(),
                });
            }
        }
    }
    let input: Vec<Value> = req
        .messages()
        .as_array()
        .expect("validated")
        .iter()
        .map(|m| {
            let role = match m["role"].as_str() {
                Some("system") => "developer",
                Some(r) => r,
                None => "user",
            };
            json!({ "type": "message", "role": role, "content": m["content"] })
        })
        .collect();
    let include_usage = fields
        .get("stream_options")
        .and_then(|o| o["include_usage"].as_bool())
        .unwrap_or(false);
    Ok((
        json!({ "input": input, "store": false, "stream": true }),
        include_usage,
    ))
}

type Body = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// Server-sent events from the Responses stream, one JSON object at a time.
struct Events {
    body: Body,
    buf: Vec<u8>,
    eof: bool,
    idle_timeout: Duration,
}

impl Events {
    fn new(res: reqwest::Response, idle_timeout: Duration) -> Self {
        Events {
            body: Box::pin(res.bytes_stream()),
            buf: Vec::new(),
            eof: false,
            idle_timeout,
        }
    }

    /// The next event, or `Ok(None)` at end of stream.
    async fn next(&mut self) -> Result<Option<Value>, CloudError> {
        loop {
            if let Some(line) = self.take_line() {
                let line = line.strip_suffix(b"\r").unwrap_or(&line);
                let Some(data) = line.strip_prefix(b"data:") else {
                    continue; // event:, id:, comments, blank separators
                };
                let data = data.trim_ascii();
                if data.is_empty() || data == b"[DONE]" {
                    continue;
                }
                return serde_json::from_slice(data).map(Some).map_err(|_| {
                    CloudError::unavailable("ChatGPT plan stream contained invalid JSON")
                });
            }
            if self.eof {
                return Ok(None);
            }
            if self.buf.len() > MAX_LINE_BYTES {
                return Err(CloudError::unavailable("ChatGPT plan stream line too long"));
            }
            match tokio::time::timeout(self.idle_timeout, self.body.next()).await {
                Ok(Some(Ok(chunk))) => self.buf.extend_from_slice(&chunk),
                Ok(Some(Err(e))) => {
                    return Err(CloudError::unavailable(if e.is_timeout() {
                        "ChatGPT plan request timed out"
                    } else {
                        "ChatGPT plan stream was interrupted"
                    }))
                }
                Ok(None) => self.eof = true,
                Err(_) => {
                    return Err(CloudError::unavailable(
                        "ChatGPT plan stream stalled (idle timeout)",
                    ))
                }
            }
        }
    }

    fn take_line(&mut self) -> Option<Vec<u8>> {
        match self.buf.iter().position(|&b| b == b'\n') {
            Some(i) => {
                let mut line: Vec<u8> = self.buf.drain(..=i).collect();
                line.pop();
                Some(line)
            }
            None if self.eof && !self.buf.is_empty() => Some(std::mem::take(&mut self.buf)),
            None => None,
        }
    }
}

enum Step {
    Skip,
    Text(String),
    Refusal(String),
    End {
        finish_reason: &'static str,
        usage: Option<Value>,
    },
    Fail(CloudError),
}

/// Response identity and per-event translation, shared by both modes.
struct Translator {
    id: String,
    created: u64,
    model: String,
}

impl Translator {
    fn new(model: String) -> Self {
        Translator {
            id: String::new(),
            created: unix_now(),
            model,
        }
    }

    fn step(&mut self, ev: &Value) -> Step {
        let response = &ev["response"];
        if let Some(id) = response["id"].as_str() {
            if self.id.is_empty() {
                self.id = id.chars().take(128).collect();
            }
        }
        if let Some(created) = response["created_at"].as_u64() {
            self.created = created;
        }
        match ev["type"].as_str().unwrap_or_default() {
            "response.output_text.delta" => match ev["delta"].as_str() {
                Some(t) if !t.is_empty() => Step::Text(t.to_owned()),
                _ => Step::Skip,
            },
            "response.refusal.delta" => match ev["delta"].as_str() {
                Some(t) if !t.is_empty() => Step::Refusal(t.to_owned()),
                _ => Step::Skip,
            },
            "response.completed" => Step::End {
                finish_reason: "stop",
                usage: usage(&response["usage"]),
            },
            "response.incomplete" => Step::End {
                finish_reason: match response["incomplete_details"]["reason"].as_str() {
                    Some("content_filter") => "content_filter",
                    _ => "length",
                },
                usage: usage(&response["usage"]),
            },
            "response.failed" => Step::Fail(stream_error(&response["error"])),
            "error" => Step::Fail(stream_error(if ev["error"].is_object() {
                &ev["error"]
            } else {
                ev
            })),
            // Created/in-progress, reasoning, item and part bookkeeping.
            _ => Step::Skip,
        }
    }

    fn id(&self) -> &str {
        if self.id.is_empty() {
            "resp_unknown"
        } else {
            &self.id
        }
    }

    fn chunk(&self, delta: Value, finish_reason: Option<&str>) -> Value {
        json!({
            "id": self.id(), "object": "chat.completion.chunk", "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }],
        })
    }
}

fn stream_error(err: &Value) -> CloudError {
    CloudError::from_upstream(None, err["code"].as_str(), err["param"].as_str(), None)
}

fn usage(u: &Value) -> Option<Value> {
    let input = u["input_tokens"].as_u64()?;
    let output = u["output_tokens"].as_u64()?;
    Some(json!({
        "prompt_tokens": input,
        "completion_tokens": output,
        "total_tokens": u["total_tokens"].as_u64().unwrap_or(input + output),
    }))
}

fn sse(v: &Value) -> String {
    format!("data: {v}\n\n")
}

#[derive(PartialEq)]
enum Phase {
    Streaming,
    Finished,
}

struct Relay {
    events: Events,
    tr: Translator,
    opened_role: bool,
    include_usage: bool,
    phase: Phase,
    started: Instant,
    provider: super::ChatGpt,
    _permit: OwnedSemaphorePermit,
}

/// Relays the Responses stream as Chat Completions SSE. Dropping it (client
/// gone) drops the upstream connection, which stops generation.
pub(crate) fn stream(
    opened: Opened,
    virtual_model: String,
    permit: OwnedSemaphorePermit,
) -> impl Stream<Item = io::Result<Bytes>> {
    let idle = opened.provider.config().stream_idle_timeout;
    let relay = Relay {
        events: Events::new(opened.res, idle),
        tr: Translator::new(virtual_model),
        opened_role: false,
        include_usage: opened.include_usage,
        phase: Phase::Streaming,
        started: Instant::now(),
        provider: opened.provider,
        _permit: permit,
    };
    stream::unfold(relay, |r| async move {
        match r.phase {
            Phase::Streaming => Some(r.next_frame().await),
            Phase::Finished => None,
        }
    })
}

impl Relay {
    fn delta(&mut self, field: &str, text: String) -> Value {
        let mut d = Map::new();
        if !self.opened_role {
            self.opened_role = true;
            d.insert("role".into(), "assistant".into());
        }
        d.insert(field.into(), text.into());
        Value::Object(d)
    }

    async fn next_frame(mut self) -> (io::Result<Bytes>, Self) {
        loop {
            let step = match self.events.next().await {
                Ok(Some(ev)) => self.tr.step(&ev),
                Ok(None) => Step::Fail(CloudError::unavailable(
                    "ChatGPT plan stream ended before completion",
                )),
                Err(e) => Step::Fail(e),
            };
            match step {
                Step::Skip => continue,
                Step::Text(t) => {
                    let d = self.delta("content", t);
                    let frame = sse(&self.tr.chunk(d, None));
                    return (Ok(Bytes::from(frame)), self);
                }
                Step::Refusal(t) => {
                    let d = self.delta("refusal", t);
                    let frame = sse(&self.tr.chunk(d, None));
                    return (Ok(Bytes::from(frame)), self);
                }
                Step::End {
                    finish_reason,
                    usage,
                } => {
                    let delta = if self.opened_role {
                        json!({})
                    } else {
                        json!({ "role": "assistant", "content": "" })
                    };
                    let mut out = sse(&self.tr.chunk(delta, Some(finish_reason)));
                    if let (true, Some(usage)) = (self.include_usage, usage) {
                        let mut c = self.tr.chunk(json!({}), None);
                        c["choices"] = json!([]);
                        c["usage"] = usage;
                        out.push_str(&sse(&c));
                    }
                    out.push_str("data: [DONE]\n\n");
                    self.phase = Phase::Finished;
                    info!(
                        provider = "chatgpt",
                        finish_reason,
                        elapsed_ms = self.started.elapsed().as_millis(),
                        "stream completed"
                    );
                    return (Ok(Bytes::from(out)), self);
                }
                Step::Fail(err) => {
                    self.provider.note_failure(&err);
                    warn!(
                        provider = "chatgpt",
                        code = err.code(),
                        elapsed_ms = self.started.elapsed().as_millis(),
                        "stream failed"
                    );
                    self.phase = Phase::Finished;
                    return (Ok(ApiError::cloud(&err).sse_frame()), self);
                }
            }
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        if self.phase == Phase::Streaming {
            info!(
                provider = "chatgpt",
                elapsed_ms = self.started.elapsed().as_millis(),
                "stream cancelled by client"
            );
        }
    }
}

/// Reads the whole stream into one `chat.completion`. Only a terminal
/// success produces a completion; partial text is never returned as one.
pub(crate) async fn collect(opened: Opened, virtual_model: String) -> Result<Value, CloudError> {
    let provider = opened.provider.clone();
    let result = async {
        let mut events = Events::new(opened.res, provider.config().stream_idle_timeout);
        let mut tr = Translator::new(virtual_model);
        let (mut text, mut refusal) = (String::new(), String::new());
        loop {
            let Some(ev) = events.next().await? else {
                return Err(CloudError::unavailable(
                    "ChatGPT plan stream ended before completion",
                ));
            };
            match tr.step(&ev) {
                Step::Skip => {}
                Step::Text(t) => text.push_str(&t),
                Step::Refusal(t) => refusal.push_str(&t),
                Step::Fail(e) => return Err(e),
                Step::End {
                    finish_reason,
                    usage,
                } => {
                    let mut message = json!({ "role": "assistant", "content": text });
                    if !refusal.is_empty() {
                        message["refusal"] = refusal.into();
                    }
                    let mut completion = json!({
                        "id": tr.id(), "object": "chat.completion", "created": tr.created,
                        "model": tr.model,
                        "choices": [{ "index": 0, "message": message, "finish_reason": finish_reason }],
                    });
                    if let Some(usage) = usage {
                        completion["usage"] = usage;
                    }
                    return Ok(completion);
                }
            }
        }
    }
    .await;
    if let Err(e) = &result {
        provider.note_failure(e);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::chat_request;

    fn req(v: Value) -> ChatRequest {
        chat_request(v, "m").unwrap()
    }

    #[test]
    fn translates_messages_and_forces_store_false_stream_true() {
        let r = req(json!({ "model": "m", "stream": false, "messages": [
            { "role": "developer", "content": "be brief" },
            { "role": "user", "content": [{ "type": "text", "text": "a" }] },
            { "role": "assistant", "content": "b" },
            { "role": "user", "content": "c" },
        ]}));
        let (body, usage) = translate(&r, false).unwrap();
        assert!(!usage);
        assert_eq!(
            body,
            json!({
                "input": [
                    { "type": "message", "role": "developer", "content": "be brief" },
                    { "type": "message", "role": "user", "content": "a" },
                    { "type": "message", "role": "assistant", "content": "b" },
                    { "type": "message", "role": "user", "content": "c" },
                ],
                "store": false, "stream": true,
            })
        );
    }

    #[test]
    fn rejects_what_the_route_cannot_do() {
        let base = || json!({ "model": "m", "messages": [{ "role": "user", "content": "x" }] });
        for (k, v) in [
            ("temperature", json!(0.2)),
            ("top_p", json!(0.5)),
            ("max_tokens", json!(10)),
            ("max_completion_tokens", json!(10)),
            ("stop", json!("\n")),
            ("seed", json!(1)),
            ("presence_penalty", json!(0.1)),
        ] {
            let mut b = base();
            b[k] = v;
            let err = translate(&req(b), false).unwrap_err();
            assert_eq!(err.code(), "unsupported_request", "{k}");
            let param = if k == "max_completion_tokens" {
                "max_tokens"
            } else {
                k
            };
            assert_eq!(err.param(), Some(param));
        }
        // Opt-in: sampling hints are dropped, length limits still rejected.
        let mut b = base();
        b["temperature"] = json!(0.2);
        b["seed"] = json!(3);
        let (body, _) = translate(&req(b), true).unwrap();
        assert!(body.get("temperature").is_none() && body.get("seed").is_none());
        let mut b = base();
        b["max_tokens"] = json!(5);
        assert!(translate(&req(b), true).is_err());
    }

    #[test]
    fn include_usage_is_honoured() {
        let r = req(
            json!({ "model": "m", "stream": true, "stream_options": { "include_usage": true },
            "messages": [{ "role": "user", "content": "x" }] }),
        );
        assert!(translate(&r, false).unwrap().1);
    }

    #[test]
    fn steps() {
        let mut tr = Translator::new("opengate/local".into());
        assert!(matches!(
            tr.step(&json!({ "type": "response.created", "response": { "id": "resp_1", "created_at": 42 } })),
            Step::Skip
        ));
        assert_eq!(tr.id(), "resp_1");
        assert_eq!(tr.created, 42);
        assert!(
            matches!(tr.step(&json!({ "type": "response.output_text.delta", "delta": "Hi" })), Step::Text(t) if t == "Hi")
        );
        assert!(matches!(
            tr.step(&json!({ "type": "response.completed", "response": { "usage": { "input_tokens": 3, "output_tokens": 2 } } })),
            Step::End { finish_reason: "stop", usage: Some(u) } if u["total_tokens"] == 5
        ));
        assert!(matches!(
            tr.step(&json!({ "type": "response.incomplete", "response": { "incomplete_details": { "reason": "max_output_tokens" } } })),
            Step::End { finish_reason: "length", .. }
        ));
        assert!(matches!(
            tr.step(&json!({ "type": "response.failed", "response": { "error": { "code": "subscription_sharing_usage_limit_exceeded" } } })),
            Step::Fail(CloudError::LimitReached)
        ));
        assert!(matches!(
            tr.step(&json!({ "type": "error", "code": "server_error", "message": "x" })),
            Step::Fail(CloudError::ProviderUnavailable(_))
        ));
    }
}
