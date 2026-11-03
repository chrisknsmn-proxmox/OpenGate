//! Chat Completions ⇄ Responses translation for the ChatGPT plan route.
//!
//! Requests: messages map one-to-one to Responses `input` items (system
//! prompts become `developer` messages, since explicit system items are
//! rejected on this route). Assistant tool calls become `function_call`
//! items and tool results `function_call_output` items, with the client's
//! call IDs, so a conversation started on Ollama continues here unchanged.
//! Function tools are sent grouped in one namespace (`opengate`), the form
//! the Sign in with ChatGPT route documents as supported, with `strict:
//! false` (Responses would otherwise try strict mode). Every request is sent
//! with `store: false` and `stream: true`, as the route requires. Parameters
//! the route does not accept (temperature, top_p, max_output_tokens, …) are
//! rejected with `unsupported_request` rather than silently dropped;
//! sampling hints can be dropped instead with
//! `CHATGPT_DROP_UNSUPPORTED_SAMPLING=true`.
//!
//! Responses: `response.output_text.delta` events become chat chunks, and
//! `function_call` output items become `tool_calls` deltas (opening delta
//! with the call ID and name, then argument fragments as they stream).
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
use crate::{
    error::ApiError,
    validate::{ChatRequest, Message, ResponseFormat, ToolChoice},
};

const MAX_LINE_BYTES: usize = 1 << 20;
/// Sampling hints: changing them alters style, not the shape of the answer.
const SAMPLING_HINT: &str = " (set CHATGPT_DROP_UNSUPPORTED_SAMPLING=true to drop it on fallback)";
/// Namespace for the client's function tools on the ChatGPT plan route.
pub(crate) const TOOL_NAMESPACE: &str = "opengate";

fn unsupported(param: &str, hint: &str) -> CloudError {
    CloudError::Unsupported {
        param: param.to_owned(),
        message: format!("{param} is not supported by the ChatGPT plan route{hint}").into(),
    }
}

/// Returns the Responses body (without `model`) and whether usage was
/// requested via `stream_options.include_usage`.
pub(crate) fn translate(
    req: &ChatRequest,
    drop_sampling: bool,
    drop_max_tokens: bool,
) -> Result<(Value, bool), CloudError> {
    if let (false, Some((name, _))) = (drop_sampling, req.sampling.first()) {
        return Err(unsupported(name, SAMPLING_HINT));
    }
    if req.max_tokens.is_some() && !drop_max_tokens {
        return Err(unsupported(
            "max_tokens",
            " (set CHATGPT_DROP_MAX_TOKENS=true to drop it on fallback)",
        ));
    }
    if !req.stop.is_empty() {
        return Err(unsupported("stop", ""));
    }
    let mut input = Vec::with_capacity(req.messages.len());
    for m in &req.messages {
        match m {
            Message::System(c) => {
                input.push(json!({ "type": "message", "role": "developer", "content": c }))
            }
            Message::User(c) => {
                input.push(json!({ "type": "message", "role": "user", "content": c }))
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                if !content.is_empty() || tool_calls.is_empty() {
                    input.push(
                        json!({ "type": "message", "role": "assistant", "content": content }),
                    );
                }
                for c in tool_calls {
                    input.push(json!({
                        "type": "function_call", "call_id": c.id, "name": c.name,
                        "namespace": TOOL_NAMESPACE, "arguments": c.arguments.to_string(),
                    }));
                }
            }
            Message::Tool {
                call_id, content, ..
            } => input.push(json!({
                "type": "function_call_output", "call_id": call_id, "output": content,
            })),
        }
    }
    let mut body = json!({ "input": input, "store": false, "stream": true });
    if !req.tools.is_empty() {
        let functions: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                let mut f = json!({ "type": "function", "name": t.name,
                    "parameters": t.parameters, "strict": false });
                if let Some(d) = &t.description {
                    f["description"] = d.as_str().into();
                }
                f
            })
            .collect();
        body["tools"] = json!([{
            "type": "namespace", "name": TOOL_NAMESPACE,
            "description": "Functions provided by the client application.",
            "tools": functions,
        }]);
        body["tool_choice"] = match &req.tool_choice {
            ToolChoice::Auto => "auto".into(),
            ToolChoice::None => "none".into(),
            ToolChoice::Required => "required".into(),
            ToolChoice::Function(name) => json!({ "type": "function", "name": name }),
        };
        if let Some(parallel) = req.parallel_tool_calls {
            body["parallel_tool_calls"] = parallel.into();
        }
    }
    match &req.response_format {
        None => {}
        Some(ResponseFormat::JsonObject) => {
            body["text"] = json!({ "format": { "type": "json_object" } })
        }
        Some(ResponseFormat::JsonSchema {
            name,
            schema,
            description,
            strict,
        }) => {
            let mut f = json!({ "type": "json_schema", "name": name, "schema": schema });
            if let Some(d) = description {
                f["description"] = d.as_str().into();
            }
            if let Some(s) = strict {
                f["strict"] = (*s).into();
            }
            body["text"] = json!({ "format": f });
        }
    }
    Ok((body, req.include_usage))
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
    /// Part of tool call `index`: `open` (call ID, function name) on its
    /// first step, then argument fragments.
    Tool {
        index: usize,
        open: Option<(String, String)>,
        args: String,
    },
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
    /// Function calls in output order: Responses item ID and the arguments
    /// passed on so far.
    calls: Vec<(String, String)>,
}

impl Translator {
    fn new(model: String) -> Self {
        Translator {
            id: String::new(),
            created: unix_now(),
            model,
            calls: Vec::new(),
        }
    }

    /// A `function_call` item was announced (or completed without being
    /// announced first): opens tool call `index`.
    fn open_call(&mut self, item: &Value) -> Step {
        let item_id = item["id"].as_str().unwrap_or_default().to_owned();
        let call_id = item["call_id"]
            .as_str()
            .or(item["id"].as_str())
            .unwrap_or_default();
        let name = item["name"].as_str().unwrap_or_default();
        if call_id.is_empty() || name.is_empty() {
            return Step::Fail(CloudError::unavailable(
                "ChatGPT plan returned a function call without an ID or name",
            ));
        }
        let args = item["arguments"].as_str().unwrap_or_default().to_owned();
        let index = self.calls.len();
        self.calls.push((item_id, args.clone()));
        Step::Tool {
            index,
            open: Some((call_id.to_owned(), name.to_owned())),
            args,
        }
    }

    fn call_index(&self, item_id: Option<&str>) -> Option<usize> {
        let item_id = item_id?;
        self.calls.iter().position(|(id, _)| id == item_id)
    }

    /// Whatever part of the final `arguments` was not streamed yet.
    fn finish_call(&mut self, item_id: Option<&str>, item: &Value, full: Option<&str>) -> Step {
        let Some(index) = self.call_index(item_id) else {
            return if item["type"] == "function_call" {
                self.open_call(item)
            } else {
                Step::Skip
            };
        };
        let Some(full) = full else {
            return Step::Skip;
        };
        let sent = &mut self.calls[index].1;
        match full.strip_prefix(sent.as_str()) {
            Some("") => Step::Skip,
            Some(rest) => {
                let rest = rest.to_owned();
                sent.push_str(&rest);
                Step::Tool {
                    index,
                    open: None,
                    args: rest,
                }
            }
            None => Step::Fail(CloudError::unavailable(
                "ChatGPT plan streamed inconsistent function call arguments",
            )),
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
            "response.output_item.added" if ev["item"]["type"] == "function_call" => {
                self.open_call(&ev["item"])
            }
            "response.function_call_arguments.delta" => {
                match (
                    self.call_index(ev["item_id"].as_str()),
                    ev["delta"].as_str(),
                ) {
                    (Some(index), Some(d)) if !d.is_empty() => {
                        self.calls[index].1.push_str(d);
                        Step::Tool {
                            index,
                            open: None,
                            args: d.to_owned(),
                        }
                    }
                    (None, _) => Step::Fail(CloudError::unavailable(
                        "ChatGPT plan streamed arguments for an unknown function call",
                    )),
                    _ => Step::Skip,
                }
            }
            "response.function_call_arguments.done" => self.finish_call(
                ev["item_id"].as_str(),
                &Value::Null,
                ev["arguments"].as_str(),
            ),
            "response.output_item.done" if ev["item"]["type"] == "function_call" => {
                let item = &ev["item"];
                self.finish_call(item["id"].as_str(), item, item["arguments"].as_str())
            }
            "response.completed" => Step::End {
                finish_reason: if self.calls.is_empty() {
                    "stop"
                } else {
                    "tool_calls"
                },
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
    fn delta(&mut self, field: &str, value: Value) -> Value {
        let mut d = Map::new();
        if !self.opened_role {
            self.opened_role = true;
            d.insert("role".into(), "assistant".into());
        }
        d.insert(field.into(), value);
        Value::Object(d)
    }

    fn tool_frames(
        &mut self,
        index: usize,
        open: Option<(String, String)>,
        args: String,
    ) -> String {
        let mut out = String::new();
        if let Some((id, name)) = open {
            let call = json!([{ "index": index, "id": id, "type": "function",
                "function": { "name": name, "arguments": "" } }]);
            let d = self.delta("tool_calls", call);
            out.push_str(&sse(&self.tr.chunk(d, None)));
        }
        if !args.is_empty() {
            let call = json!([{ "index": index, "function": { "arguments": args } }]);
            let d = self.delta("tool_calls", call);
            out.push_str(&sse(&self.tr.chunk(d, None)));
        }
        out
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
                    let d = self.delta("content", t.into());
                    let frame = sse(&self.tr.chunk(d, None));
                    return (Ok(Bytes::from(frame)), self);
                }
                Step::Refusal(t) => {
                    let d = self.delta("refusal", t.into());
                    let frame = sse(&self.tr.chunk(d, None));
                    return (Ok(Bytes::from(frame)), self);
                }
                Step::Tool { index, open, args } => {
                    let frames = self.tool_frames(index, open, args);
                    if frames.is_empty() {
                        continue;
                    }
                    return (Ok(Bytes::from(frames)), self);
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
        // (call ID, name, arguments) by tool call index.
        let mut calls: Vec<(String, String, String)> = Vec::new();
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
                Step::Tool { index, open, args } => {
                    if let Some((id, name)) = open {
                        calls.push((id, name, String::new()));
                    }
                    if let Some(call) = calls.get_mut(index) {
                        call.2.push_str(&args);
                    }
                }
                Step::Fail(e) => return Err(e),
                Step::End {
                    finish_reason,
                    usage,
                } => {
                    let mut message = json!({ "role": "assistant", "content": text });
                    if !calls.is_empty() {
                        if text.is_empty() {
                            message["content"] = Value::Null;
                        }
                        message["tool_calls"] = calls
                            .iter()
                            .map(|(id, name, args)| json!({ "id": id, "type": "function",
                                "function": { "name": name, "arguments": args } }))
                            .collect();
                    }
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
        let (body, usage) = translate(&r, false, false).unwrap();
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
            let err = translate(&req(b), false, false).unwrap_err();
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
        let (body, _) = translate(&req(b), true, false).unwrap();
        assert!(body.get("temperature").is_none() && body.get("seed").is_none());
        let mut b = base();
        b["max_tokens"] = json!(5);
        assert!(translate(&req(b.clone()), true, false).is_err());
        // Opt-in: max_tokens dropped; stop is still rejected.
        assert!(translate(&req(b.clone()), false, true).is_ok());
        b["stop"] = json!("x");
        assert!(translate(&req(b), false, true).is_err());
    }

    #[test]
    fn include_usage_is_honoured() {
        let r = req(
            json!({ "model": "m", "stream": true, "stream_options": { "include_usage": true },
            "messages": [{ "role": "user", "content": "x" }] }),
        );
        assert!(translate(&r, false, false).unwrap().1);
    }

    #[test]
    fn translates_tools_and_tool_history() {
        let r = req(
            json!({ "model": "m", "stream": true, "tool_choice": "required", "parallel_tool_calls": false,
            "tools": [{ "type": "function", "function": { "name": "get_weather", "description": "d",
                "parameters": { "type": "object", "properties": {} } } }],
            "messages": [
                { "role": "user", "content": "weather?" },
                { "role": "assistant", "content": null, "tool_calls": [
                    { "id": "call_a", "type": "function", "function": { "name": "get_weather", "arguments": "{\"city\":\"Paris\"}" } } ] },
                { "role": "tool", "tool_call_id": "call_a", "content": "18C" },
            ]}),
        );
        let (body, _) = translate(&r, false, false).unwrap();
        assert_eq!(
            body["input"],
            json!([
                { "type": "message", "role": "user", "content": "weather?" },
                { "type": "function_call", "call_id": "call_a", "name": "get_weather",
                  "namespace": TOOL_NAMESPACE, "arguments": "{\"city\":\"Paris\"}" },
                { "type": "function_call_output", "call_id": "call_a", "output": "18C" },
            ])
        );
        assert_eq!(body["tools"][0]["type"], "namespace");
        assert_eq!(
            body["tools"][0]["tools"],
            json!([{ "type": "function", "name": "get_weather", "description": "d",
                "parameters": { "type": "object", "properties": {} }, "strict": false }])
        );
        assert_eq!(body["tool_choice"], "required");
        assert_eq!(body["parallel_tool_calls"], false);

        let forced = req(
            json!({ "model": "m", "messages": [{ "role": "user", "content": "x" }],
            "tools": [{ "type": "function", "function": { "name": "f" } }],
            "tool_choice": { "type": "function", "function": { "name": "f" } } }),
        );
        assert_eq!(
            translate(&forced, false, false).unwrap().0["tool_choice"],
            json!({ "type": "function", "name": "f" })
        );
        let fmt = req(
            json!({ "model": "m", "messages": [{ "role": "user", "content": "x" }],
            "response_format": { "type": "json_schema", "json_schema": { "name": "o", "schema": { "type": "object" }, "strict": true } } }),
        );
        assert_eq!(
            translate(&fmt, false, false).unwrap().0["text"],
            json!({ "format": { "type": "json_schema", "name": "o", "schema": { "type": "object" }, "strict": true } })
        );
    }

    #[test]
    fn function_call_events() {
        let mut tr = Translator::new("opengate/local".into());
        let added = json!({ "type": "response.output_item.added", "output_index": 0,
            "item": { "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "get_weather", "arguments": "" } });
        assert!(
            matches!(tr.step(&added), Step::Tool { index: 0, open: Some((id, name)), args }
            if id == "call_1" && name == "get_weather" && args.is_empty())
        );
        for d in ["{\"ci", "ty\":\"Paris\"}"] {
            let ev = json!({ "type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": d });
            assert!(matches!(tr.step(&ev), Step::Tool { index: 0, open: None, args } if args == d));
        }
        let done = json!({ "type": "response.function_call_arguments.done", "item_id": "fc_1", "arguments": "{\"city\":\"Paris\"}" });
        assert!(matches!(tr.step(&done), Step::Skip), "nothing left to send");
        // A call that arrives whole, only in output_item.done.
        let whole = json!({ "type": "response.output_item.done",
            "item": { "type": "function_call", "id": "fc_2", "call_id": "call_2", "name": "get_time", "arguments": "{}" } });
        assert!(
            matches!(tr.step(&whole), Step::Tool { index: 1, open: Some(_), args } if args == "{}")
        );
        assert!(matches!(
            tr.step(&json!({ "type": "response.completed", "response": {} })),
            Step::End {
                finish_reason: "tool_calls",
                ..
            }
        ));
        let stray = json!({ "type": "response.function_call_arguments.delta", "item_id": "fc_9", "delta": "x" });
        assert!(matches!(tr.step(&stray), Step::Fail(_)));
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
