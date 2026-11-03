//! Relays Ollama's native stream (newline-delimited JSON from `/api/chat`)
//! to the client as OpenAI `chat.completion.chunk` server-sent events.
//!
//! The first frame is the assistant role. Text, reasoning and tool calls are
//! translated as they arrive (see `ollama.rs`); each tool call is sent as an
//! opening delta (`index`, `id`, `type`, `function.name`) followed by its
//! complete arguments, so concatenating the argument fragments of an index
//! always yields the JSON object the model produced. The final chunk carries
//! the finish reason, then the usage chunk if `stream_options.include_usage`
//! was set, then `data: [DONE]`, exactly once.
//!
//! `data: [DONE]` is only sent after Ollama's final (`done: true`) chunk. Any
//! other ending (upstream error, idle timeout, overall timeout, EOF without
//! `done`, malformed data, an answer that breaks `tool_choice`) sends one
//! OpenAI-style error frame and ends the stream without `[DONE]`, so a
//! truncated answer can never look like a successful one. (Aborting the
//! connection instead would make hyper discard the buffered error frame.)
//! Dropping the stream (client disconnect) drops the upstream request, which
//! makes Ollama stop generating.

use std::{
    io,
    pin::Pin,
    time::{Duration, Instant},
};

use axum::body::Bytes;
use futures_util::{stream, Stream, StreamExt};
use serde_json::{json, Value};
use tokio::sync::OwnedSemaphorePermit;
use tracing::{info, warn};

use crate::{
    error::ApiError,
    ollama::{Event, Reply},
    upstream::from_reqwest,
};

/// Longest single line accepted from upstream.
const MAX_LINE_BYTES: usize = 1 << 20;

type Upstream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

#[derive(PartialEq)]
enum Phase {
    /// Nothing sent yet; the next frame is the assistant role.
    Start,
    Streaming,
    Finished,
}

struct Relay {
    upstream: Upstream,
    buf: Vec<u8>,
    eof: bool,
    phase: Phase,
    reply: Reply,
    include_usage: bool,
    idle_timeout: Duration,
    started: Instant,
    _permit: OwnedSemaphorePermit,
}

pub(crate) fn relay(
    res: reqwest::Response,
    reply: Reply,
    include_usage: bool,
    idle_timeout: Duration,
    permit: OwnedSemaphorePermit,
) -> impl Stream<Item = io::Result<Bytes>> {
    let relay = Relay {
        upstream: Box::pin(res.bytes_stream()),
        buf: Vec::new(),
        eof: false,
        phase: Phase::Start,
        reply,
        include_usage,
        idle_timeout,
        started: Instant::now(),
        _permit: permit,
    };
    stream::unfold(relay, |r| async move {
        match r.phase {
            Phase::Start | Phase::Streaming => Some(r.next_frame().await),
            Phase::Finished => None,
        }
    })
}

fn sse(v: &Value) -> String {
    format!("data: {v}\n\n")
}

impl Relay {
    async fn next_frame(mut self) -> (io::Result<Bytes>, Self) {
        if self.phase == Phase::Start {
            self.phase = Phase::Streaming;
            let role = self
                .reply
                .chunk(json!({ "role": "assistant", "content": "" }), None);
            return (Ok(Bytes::from(sse(&role))), self);
        }
        loop {
            if let Some(line) = self.take_line() {
                let line = line.trim_ascii();
                if line.is_empty() {
                    continue;
                }
                let Ok(chunk) = serde_json::from_slice::<Value>(line) else {
                    return self.fail(ApiError::upstream_invalid_response());
                };
                let mut events = Vec::new();
                if let Err(err) = self.reply.step(&chunk, &mut events) {
                    return self.fail(err);
                }
                if events.is_empty() {
                    continue;
                }
                let frames = self.frames(events);
                if self.reply.done {
                    self.phase = Phase::Finished;
                    info!(elapsed_ms = self.elapsed_ms(), "stream completed");
                }
                return (Ok(Bytes::from(frames)), self);
            }
            if self.eof {
                return self.fail(ApiError::stream_incomplete());
            }
            if self.buf.len() > MAX_LINE_BYTES {
                return self.fail(ApiError::upstream_invalid_response());
            }
            match tokio::time::timeout(self.idle_timeout, self.upstream.next()).await {
                Ok(Some(Ok(chunk))) => self.buf.extend_from_slice(&chunk),
                Ok(Some(Err(e))) => return self.fail(from_reqwest(e)),
                Ok(None) => self.eof = true,
                Err(_) => return self.fail(ApiError::upstream_timeout()),
            }
        }
    }

    /// SSE frames for one upstream chunk's events, written together.
    fn frames(&self, events: Vec<Event>) -> String {
        let mut out = String::new();
        for ev in events {
            match ev {
                Event::Reasoning(t) => {
                    out += &sse(&self.reply.chunk(json!({ "reasoning_content": t }), None))
                }
                Event::Text(t) => out += &sse(&self.reply.chunk(json!({ "content": t }), None)),
                Event::ToolCall {
                    index,
                    id,
                    name,
                    arguments,
                } => {
                    let open = json!({ "tool_calls": [{ "index": index, "id": id, "type": "function",
                        "function": { "name": name, "arguments": "" } }] });
                    let args = json!({ "tool_calls": [{ "index": index, "function": { "arguments": arguments } }] });
                    out += &sse(&self.reply.chunk(open, None));
                    out += &sse(&self.reply.chunk(args, None));
                }
                Event::Done {
                    finish_reason,
                    usage,
                } => {
                    out += &sse(&self.reply.chunk(json!({}), Some(finish_reason)));
                    if let (true, Some(usage)) = (self.include_usage, usage) {
                        let mut c = self.reply.chunk(json!({}), None);
                        c["choices"] = json!([]);
                        c["usage"] = usage;
                        out += &sse(&c);
                    }
                    out += "data: [DONE]\n\n";
                }
            }
        }
        out
    }

    /// The next complete line, or the unterminated remainder once upstream ended.
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

    fn fail(mut self, err: ApiError) -> (io::Result<Bytes>, Self) {
        warn!(
            code = err.code,
            elapsed_ms = self.elapsed_ms(),
            "stream failed"
        );
        self.phase = Phase::Finished;
        (Ok(err.sse_frame()), self)
    }

    fn elapsed_ms(&self) -> u128 {
        self.started.elapsed().as_millis()
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        if self.phase != Phase::Finished {
            info!(elapsed_ms = self.elapsed_ms(), "stream cancelled by client");
        }
    }
}
