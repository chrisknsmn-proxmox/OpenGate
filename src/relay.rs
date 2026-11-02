//! Relays Ollama's SSE stream to the client one frame at a time.
//!
//! `data: [DONE]` is only sent when the upstream sent it. Any other ending —
//! upstream error, idle timeout, overall timeout, EOF without `[DONE]` — sends
//! one OpenAI-style error frame and ends the stream without `[DONE]`, so a
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
use serde_json::Value;
use tokio::sync::OwnedSemaphorePermit;
use tracing::{info, warn};

use crate::{error::ApiError, upstream::from_reqwest};

/// Longest single SSE line accepted from upstream.
const MAX_LINE_BYTES: usize = 1 << 20;

type Upstream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

#[derive(PartialEq)]
enum Phase {
    Streaming,
    Finished,
}

struct Relay {
    upstream: Upstream,
    buf: Vec<u8>,
    eof: bool,
    phase: Phase,
    virtual_model: String,
    idle_timeout: Duration,
    started: Instant,
    _permit: OwnedSemaphorePermit,
}

pub(crate) fn relay(
    res: reqwest::Response,
    virtual_model: String,
    idle_timeout: Duration,
    permit: OwnedSemaphorePermit,
) -> impl Stream<Item = io::Result<Bytes>> {
    let relay = Relay {
        upstream: Box::pin(res.bytes_stream()),
        buf: Vec::new(),
        eof: false,
        phase: Phase::Streaming,
        virtual_model,
        idle_timeout,
        started: Instant::now(),
        _permit: permit,
    };
    stream::unfold(relay, |r| async move {
        match r.phase {
            Phase::Streaming => Some(r.next_frame().await),
            Phase::Finished => None,
        }
    })
}

enum Line {
    Skip,
    Frame(Bytes),
    Done,
    Failed(ApiError),
}

impl Relay {
    async fn next_frame(mut self) -> (io::Result<Bytes>, Self) {
        loop {
            if let Some(line) = self.take_line() {
                match self.parse(&line) {
                    Line::Skip => continue,
                    Line::Frame(frame) => return (Ok(frame), self),
                    Line::Done => {
                        self.phase = Phase::Finished;
                        info!(elapsed_ms = self.elapsed_ms(), "stream completed");
                        return (Ok(Bytes::from_static(b"data: [DONE]\n\n")), self);
                    }
                    Line::Failed(err) => return self.fail(err),
                }
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

    fn parse(&self, line: &[u8]) -> Line {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(data) = line.strip_prefix(b"data:") else {
            // Blank separators, comments, and event/id fields carry nothing we relay.
            return Line::Skip;
        };
        let data = data.trim_ascii_start();
        if data == b"[DONE]" {
            return Line::Done;
        }
        let Ok(mut chunk) = serde_json::from_slice::<Value>(data) else {
            return Line::Failed(ApiError::upstream_invalid_response());
        };
        if chunk.get("error").is_some() {
            return Line::Failed(ApiError::upstream_error(
                "local model server reported an error mid-stream",
            ));
        }
        if chunk.is_object() {
            chunk["model"] = self.virtual_model.as_str().into();
        }
        Line::Frame(Bytes::from(format!("data: {chunk}\n\n")))
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
        if self.phase == Phase::Streaming {
            info!(elapsed_ms = self.elapsed_ms(), "stream cancelled by client");
        }
    }
}
