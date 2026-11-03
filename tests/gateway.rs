//! End-to-end tests: a real OpenGate server in front of a mock Ollama.

use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use axum::{
    body::{Body, Bytes},
    extract::State,
    routing::{get, post},
    Json, Router,
};
use futures_util::stream;
use opengate::Config;
use serde_json::{json, Value};

const KEY: &str = "test-key-0123456789abcdef0123456789";
const VM: &str = "opengate/local";

// ---- harness -------------------------------------------------------------

async fn serve(router: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    addr
}

fn config(upstream: SocketAddr) -> Config {
    Config::from_lookup(|name| match name {
        "OPENGATE_API_KEY" => Some(KEY.into()),
        "OLLAMA_BASE_URL" => Some(format!("http://{upstream}")),
        "OLLAMA_MODEL" => Some("llama3.1:8b".into()),
        _ => None,
    })
    .unwrap()
}

struct Gateway {
    base: String,
    http: reqwest::Client,
}

async fn gateway(upstream: SocketAddr, tweak: impl FnOnce(&mut Config)) -> Gateway {
    let mut config = config(upstream);
    tweak(&mut config);
    let addr = serve(opengate::app(config)).await;
    Gateway {
        base: format!("http://{addr}"),
        http: reqwest::Client::new(),
    }
}

impl Gateway {
    async fn chat(&self, body: Value) -> reqwest::Response {
        self.http
            .post(format!("{}/v1/chat/completions", self.base))
            .bearer_auth(KEY)
            .json(&body)
            .send()
            .await
            .unwrap()
    }
}

fn hello(stream: bool) -> Value {
    json!({ "model": VM, "messages": [{ "role": "user", "content": "Say hello." }], "stream": stream })
}

async fn error_code(res: reqwest::Response) -> String {
    let body: Value = res.json().await.unwrap();
    body["error"]["code"].as_str().unwrap().to_owned()
}

/// A port with nothing listening on it.
async fn dead_port() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap()
}

// ---- mock Ollama ---------------------------------------------------------

/// Ollama's native non-streaming `/api/chat` answer.
fn completion(model: &str) -> Value {
    json!({
        "model": model, "created_at": "2026-11-03T10:00:00Z",
        "message": { "role": "assistant", "content": "Hello!" },
        "done": true, "done_reason": "stop", "prompt_eval_count": 5, "eval_count": 2,
    })
}

/// One line of Ollama's native stream.
fn chunk(content: &str) -> String {
    let c = json!({
        "model": "llama3.1:8b", "created_at": "2026-11-03T10:00:00Z",
        "message": { "role": "assistant", "content": content }, "done": false,
    });
    format!("{c}\n")
}

/// The final line of Ollama's native stream.
fn done() -> String {
    let c = json!({
        "model": "llama3.1:8b", "created_at": "2026-11-03T10:00:00Z",
        "message": { "role": "assistant", "content": "" },
        "done": true, "done_reason": "stop", "prompt_eval_count": 5, "eval_count": 3,
    });
    format!("{c}\n")
}

/// Mock that echoes a completion and records the body it received.
async fn recording_mock() -> (SocketAddr, Arc<Mutex<Option<Value>>>) {
    let seen = Arc::new(Mutex::new(None));
    let router = Router::new()
        .route(
            "/api/chat",
            post(|State(seen): State<Arc<Mutex<Option<Value>>>>, Json(body): Json<Value>| async move {
                let model = body["model"].as_str().unwrap_or_default().to_owned();
                *seen.lock().unwrap() = Some(body);
                Json(completion(&model))
            }),
        )
        .with_state(seen.clone());
    (serve(router).await, seen)
}

/// Mock whose chat endpoint streams `parts` verbatim, sleeping `gap` before each.
async fn streaming_mock(parts: Vec<String>, gap: Duration) -> SocketAddr {
    let router = Router::new().route(
        "/api/chat",
        post(move || {
            let parts = parts.clone();
            async move {
                let body = stream::unfold(parts.into_iter(), move |mut it| async move {
                    let part = it.next()?;
                    tokio::time::sleep(gap).await;
                    Some((Ok::<_, std::io::Error>(Bytes::from(part)), it))
                });
                (
                    [("content-type", "application/x-ndjson")],
                    Body::from_stream(body),
                )
            }
        }),
    );
    serve(router).await
}

/// Mock that streams forever and flags when OpenGate hangs up on it.
async fn endless_mock() -> (SocketAddr, Arc<AtomicBool>) {
    struct Hangup(Arc<AtomicBool>);
    impl Drop for Hangup {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let hung_up = Arc::new(AtomicBool::new(false));
    let flag = hung_up.clone();
    let router = Router::new().route(
        "/api/chat",
        post(move || {
            let guard = Hangup(flag.clone());
            async move {
                let body = stream::unfold(guard, |g| async move {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Some((Ok::<_, std::io::Error>(Bytes::from(chunk("tick"))), g))
                });
                (
                    [("content-type", "text/event-stream")],
                    Body::from_stream(body),
                )
            }
        }),
    );
    (serve(router).await, hung_up)
}

// ---- health and auth -----------------------------------------------------

#[tokio::test]
async fn live_needs_no_auth() {
    let gw = gateway(dead_port().await, |_| {}).await;
    let res = gw
        .http
        .get(format!("{}/health/live", gw.base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn ready_reflects_upstream() {
    let tags = |names: &'static [&'static str]| {
        Router::new().route(
            "/api/tags",
            get(move || async move {
                Json(json!({ "models": names.iter().map(|n| json!({ "name": n })).collect::<Vec<_>>() }))
            }),
        )
    };
    let ready = |addr| async move {
        let gw = gateway(addr, |_| {}).await;
        let res = gw
            .http
            .get(format!("{}/health/ready", gw.base))
            .send()
            .await
            .unwrap();
        (res.status().as_u16(), res.json::<Value>().await.unwrap())
    };

    let (status, body) = ready(serve(tags(&["llama3.1:8b", "other:1b"])).await).await;
    assert_eq!((status, body["status"].as_str()), (200, Some("ready")));

    let (status, body) = ready(serve(tags(&["other:1b"])).await).await;
    assert_eq!(
        (status, body["reason"].as_str()),
        (503, Some("model_not_installed"))
    );

    let (status, body) = ready(dead_port().await).await;
    assert_eq!(
        (status, body["reason"].as_str()),
        (503, Some("upstream_unreachable"))
    );
    assert!(
        !body.to_string().contains("127.0.0.1"),
        "must not reveal the upstream address"
    );
}

#[tokio::test]
async fn v1_requires_valid_key() {
    let gw = gateway(dead_port().await, |_| {}).await;
    let url = format!("{}/v1/models", gw.base);

    let res = gw.http.get(&url).send().await.unwrap();
    assert_eq!(res.status(), 401);
    assert_eq!(res.headers()["www-authenticate"], "Bearer");
    assert_eq!(error_code(res).await, "invalid_api_key");

    for bad in [
        "Bearer wrong",
        &format!("Basic {KEY}"),
        &format!("Bearer {KEY}x"),
        KEY,
    ] {
        let res = gw
            .http
            .get(&url)
            .header("authorization", bad)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401, "{bad}");
    }

    let res = gw
        .http
        .post(format!("{}/v1/chat/completions", gw.base))
        .json(&hello(false))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);

    let res = gw
        .http
        .get(&url)
        .header("authorization", format!("bearer {KEY}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn lists_only_the_virtual_model() {
    let gw = gateway(dead_port().await, |_| {}).await;
    let res = gw
        .http
        .get(format!("{}/v1/models", gw.base))
        .bearer_auth(KEY)
        .send()
        .await
        .unwrap();
    let body: Value = res.json().await.unwrap();
    let ids: Vec<_> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [VM]);
}

// ---- request validation --------------------------------------------------

#[tokio::test]
async fn rejects_bad_requests_before_calling_upstream() {
    let (addr, seen) = recording_mock().await;
    let gw = gateway(addr, |_| {}).await;
    let msgs = json!([{ "role": "user", "content": "x" }]);
    let tools = json!([{ "type": "function", "function": { "name": "f", "parameters": {} } }]);

    for (body, code) in [
        (
            json!({ "model": "llama3.1:8b", "messages": msgs }),
            "model_not_supported",
        ),
        (
            json!({ "model": VM, "messages": msgs, "tools": tools, "tool_choice": "sometimes" }),
            "invalid_request",
        ),
        (
            json!({ "model": VM, "messages": msgs, "logprobs": true }),
            "unsupported_parameter",
        ),
        (json!({ "model": VM, "messages": [] }), "invalid_request"),
    ] {
        let res = gw.chat(body).await;
        assert_eq!(res.status(), 400);
        assert_eq!(error_code(res).await, code);
    }

    let res = gw
        .http
        .post(format!("{}/v1/chat/completions", gw.base))
        .bearer_auth(KEY)
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(
        (res.status().as_u16(), error_code(res).await.as_str()),
        (400, "invalid_json")
    );

    let huge =
        json!({ "model": VM, "messages": [{ "role": "user", "content": "x".repeat(5 << 20) }] });
    let res = gw.chat(huge).await;
    assert_eq!(
        (res.status().as_u16(), error_code(res).await.as_str()),
        (413, "request_too_large")
    );

    assert!(
        seen.lock().unwrap().is_none(),
        "upstream must not be called"
    );
}

// ---- nonstreaming --------------------------------------------------------

#[tokio::test]
async fn nonstreaming_success() {
    let (addr, seen) = recording_mock().await;
    let gw = gateway(addr, |_| {}).await;
    let mut req = hello(false);
    req["max_completion_tokens"] = json!(32);
    let res = gw.chat(req).await;
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();

    assert_eq!(body["model"], VM, "the real model name is not exposed");
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["choices"][0]["message"]["content"], "Hello!");
    assert_eq!(body["usage"]["total_tokens"], 7);

    let sent = seen.lock().unwrap().clone().unwrap();
    assert_eq!(sent["model"], "llama3.1:8b");
    assert_eq!(sent["stream"], false);
    assert_eq!(sent["options"]["num_predict"], 32);
    assert_eq!(sent["truncate"], false, "never silently truncate history");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
}

#[tokio::test]
async fn upstream_unreachable_is_503() {
    let gw = gateway(dead_port().await, |_| {}).await;
    let res = gw.chat(hello(false)).await;
    assert_eq!(res.status(), 503);
    assert_eq!(error_code(res).await, "upstream_unavailable");
}

#[tokio::test]
async fn upstream_missing_model_is_503() {
    let router = Router::new().route(
        "/api/chat",
        post(|| async {
            (axum::http::StatusCode::NOT_FOUND, Json(json!({ "error": { "message": "model \"llama3.1:8b\" not found, try pulling it first" } })))
        }),
    );
    let gw = gateway(serve(router).await, |_| {}).await;
    let res = gw.chat(hello(false)).await;
    assert_eq!(res.status(), 503);
    assert_eq!(error_code(res).await, "upstream_model_unavailable");
}

#[tokio::test]
async fn upstream_server_error_is_502() {
    let router = Router::new().route(
        "/api/chat",
        post(|| async {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": { "message": "llama runner process has terminated" } })),
            )
        }),
    );
    let gw = gateway(serve(router).await, |_| {}).await;
    let res = gw.chat(hello(false)).await;
    assert_eq!(res.status(), 502);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["code"], "upstream_error");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("HTTP 500: llama runner"));
}

#[tokio::test]
async fn upstream_timeout_is_504() {
    let router = Router::new().route(
        "/api/chat",
        post(|| async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Json(completion("llama3.1:8b"))
        }),
    );
    let gw = gateway(serve(router).await, |c| {
        c.request_timeout = Duration::from_millis(300)
    })
    .await;
    let res = gw.chat(hello(false)).await;
    assert_eq!(res.status(), 504);
    assert_eq!(error_code(res).await, "upstream_timeout");
}

// ---- streaming -----------------------------------------------------------

/// The `data:` payloads of an SSE body, in order.
fn frames(body: &str) -> Vec<&str> {
    assert!(
        body.ends_with("\n\n"),
        "body must end on a frame boundary: {body:?}"
    );
    body.split("\n\n")
        .filter(|f| !f.is_empty())
        .map(|f| f.strip_prefix("data: ").expect(f))
        .collect()
}

#[tokio::test]
async fn streaming_success() {
    // Deliberately split frames across writes, as TCP may.
    let one = chunk("One");
    let (a, b) = one.split_at(20);
    let parts = vec![
        a.into(),
        b.into(),
        chunk(", two") + &chunk(", three."),
        done(),
    ];
    let gw = gateway(
        streaming_mock(parts, Duration::from_millis(20)).await,
        |_| {},
    )
    .await;

    let res = gw.chat(hello(true)).await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-type"], "text/event-stream");
    let body = res.text().await.unwrap();
    let frames = frames(&body);

    assert_eq!(frames.last(), Some(&"[DONE]"));
    assert_eq!(frames.iter().filter(|f| **f == "[DONE]").count(), 1);
    let chunks: Vec<Value> = frames[..frames.len() - 1]
        .iter()
        .map(|f| serde_json::from_str(f).unwrap())
        .collect();
    let id = chunks[0]["id"].as_str().unwrap();
    assert!(id.starts_with("chatcmpl-"));
    for c in &chunks {
        assert_eq!(c["model"], VM);
        assert_eq!(c["object"], "chat.completion.chunk");
        assert_eq!(c["id"], id, "one ID for the whole stream");
    }
    assert_eq!(
        chunks[0]["choices"][0]["delta"],
        json!({ "role": "assistant", "content": "" })
    );
    let text: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text, "One, two, three.");
    let finishes: Vec<_> = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["finish_reason"].as_str())
        .collect();
    assert_eq!(
        finishes,
        ["stop"],
        "exactly one finish event, on the last chunk"
    );
    assert!(
        chunks.iter().all(|c| c.get("usage").is_none()),
        "usage not requested"
    );
}

#[tokio::test]
async fn streaming_is_incremental() {
    let parts = vec![chunk("first"), chunk("second"), done()];
    let gw = gateway(
        streaming_mock(parts, Duration::from_millis(400)).await,
        |_| {},
    )
    .await;
    let mut res = gw.chat(hello(true)).await;
    let start = std::time::Instant::now();
    // The assistant role frame is sent at once, before any model output.
    let role = res.chunk().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&role).contains("\"role\":\"assistant\""));
    let mut seen = String::new();
    while !seen.contains("first") {
        seen += &String::from_utf8_lossy(&res.chunk().await.unwrap().unwrap());
    }
    assert!(!seen.contains("second"), "frames are not batched");
    assert!(
        start.elapsed() < Duration::from_millis(700),
        "first frame must not wait for the whole response"
    );
}

async fn assert_failed_stream(res: reqwest::Response, code: &str) {
    assert_eq!(res.status(), 200, "headers were already sent");
    let body = res.text().await.unwrap();
    assert!(
        !body.contains("[DONE]"),
        "a failed stream must not look successful: {body}"
    );
    let frames = frames(&body);
    let last: Value = serde_json::from_str(frames.last().unwrap()).unwrap();
    assert_eq!(last["error"]["code"], code, "{body}");
    assert_eq!(frames.iter().filter(|f| f.contains("\"error\"")).count(), 1);
}

#[tokio::test]
async fn stream_ending_without_done_is_an_error() {
    let parts = vec![chunk("partial")];
    let gw = gateway(streaming_mock(parts, Duration::ZERO).await, |_| {}).await;
    assert_failed_stream(gw.chat(hello(true)).await, "upstream_stream_incomplete").await;
}

#[tokio::test]
async fn stream_upstream_error_frame_is_an_error() {
    let parts = vec![chunk("partial"), "{\"error\":\"boom\"}\n".into()];
    let gw = gateway(streaming_mock(parts, Duration::ZERO).await, |_| {}).await;
    assert_failed_stream(gw.chat(hello(true)).await, "upstream_error").await;
}

#[tokio::test]
async fn stream_idle_timeout() {
    let parts = vec![chunk("one"), chunk("late"), done()];
    let gw = gateway(
        streaming_mock(parts, Duration::from_millis(600)).await,
        |c| c.stream_idle_timeout = Duration::from_millis(200),
    )
    .await;
    assert_failed_stream(gw.chat(hello(true)).await, "upstream_timeout").await;
}

#[tokio::test]
async fn stream_overall_timeout() {
    let (addr, _) = endless_mock().await;
    let gw = gateway(addr, |c| c.request_timeout = Duration::from_millis(500)).await;
    assert_failed_stream(gw.chat(hello(true)).await, "upstream_timeout").await;
}

// ---- concurrency and cancellation ----------------------------------------

#[tokio::test]
async fn concurrency_limit_rejects_then_recovers() {
    let router = Router::new().route(
        "/api/chat",
        post(|| async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            Json(completion("llama3.1:8b"))
        }),
    );
    let gw = Arc::new(gateway(serve(router).await, |c| c.max_concurrent_requests = 1).await);

    let first = tokio::spawn({
        let gw = gw.clone();
        async move { gw.chat(hello(false)).await.status() }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;

    let res = gw.chat(hello(false)).await;
    assert_eq!(res.status(), 503);
    assert_eq!(res.headers()["retry-after"], "1");
    assert_eq!(error_code(res).await, "gateway_overloaded");

    assert_eq!(first.await.unwrap(), 200);
    assert_eq!(
        gw.chat(hello(false)).await.status(),
        200,
        "permit is released afterwards"
    );
}

#[tokio::test]
async fn client_abort_cancels_upstream_and_frees_the_slot() {
    let (addr, hung_up) = endless_mock().await;
    let gw = gateway(addr, |c| c.max_concurrent_requests = 1).await;

    let mut res = gw.chat(hello(true)).await;
    assert_eq!(res.status(), 200);
    res.chunk().await.unwrap().unwrap();
    drop(res);

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !hung_up.load(Ordering::SeqCst) {
        assert!(
            std::time::Instant::now() < deadline,
            "upstream request was not cancelled"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The slot is free again. Poll briefly: the permit drops just after the hang-up.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let res = gw.chat(hello(true)).await;
        if res.status() == 200 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "slot was never released"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
