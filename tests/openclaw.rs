//! OpenClaw compatibility: tool calling, streaming tool calls, multi-turn
//! tool conversations, context limits and fallback, end to end through a
//! real OpenGate in front of a scripted mock of Ollama's native API (and the
//! ChatGPT plan mock). Request bodies use the exact shapes OpenClaw
//! 2026.10.5's `openai-completions` transport sends to a custom endpoint.

mod common;

use std::{
    collections::{BTreeMap, VecDeque},
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use axum::{
    body::Body,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use common::{
    chatgpt::{self as cg, config_with, mock, seed, Inference, PLAN_SCOPES},
    serve, KEY, VM,
};
use opengate::Config;
use serde_json::{json, Value};

// ---- scripted mock Ollama ------------------------------------------------

#[derive(Clone, Default)]
struct Script {
    /// Replies to `/api/chat`, in order: (status, body).
    replies: Arc<Mutex<VecDeque<(u16, String)>>>,
    /// Every `/api/chat` request body received.
    seen: Arc<Mutex<Vec<Value>>>,
}

impl Script {
    fn requests(&self) -> Vec<Value> {
        self.seen.lock().unwrap().clone()
    }
    fn last(&self) -> Value {
        self.requests().pop().expect("Ollama was called")
    }
}

async fn ollama(replies: Vec<(u16, String)>) -> (SocketAddr, Script) {
    let script = Script {
        replies: Arc::new(Mutex::new(replies.into())),
        ..Default::default()
    };
    let router = Router::new()
        .route(
            "/api/chat",
            post(
                |State(s): State<Script>, Json(body): Json<Value>| async move {
                    s.seen.lock().unwrap().push(body);
                    let (status, body) = s
                        .replies
                        .lock()
                        .unwrap()
                        .pop_front()
                        .expect("unexpected extra request");
                    (
                        StatusCode::from_u16(status).unwrap(),
                        [("content-type", "application/x-ndjson")],
                        Body::from(body),
                    )
                        .into_response()
                },
            ),
        )
        .route(
            "/api/tags",
            get(|| async { Json(json!({ "models": [{ "name": "llama3.1:8b" }] })) }),
        )
        .route(
            "/api/ps",
            get(|| async {
                Json(json!({ "models": [{ "name": "llama3.1:8b", "context_length": 32768 }] }))
            }),
        )
        .with_state(script.clone());
    (serve(router).await, script)
}

fn ndjson(lines: &[Value]) -> (u16, String) {
    (200, lines.iter().map(|l| format!("{l}\n")).collect())
}

fn text(t: &str) -> Value {
    json!({ "model": "llama3.1:8b", "message": { "role": "assistant", "content": t }, "done": false })
}

fn done(reason: &str) -> Value {
    json!({ "model": "llama3.1:8b", "message": { "role": "assistant", "content": "" },
        "done": true, "done_reason": reason, "prompt_eval_count": 812, "eval_count": 31 })
}

fn calls(calls: Value) -> Value {
    json!({ "model": "llama3.1:8b", "message": { "role": "assistant", "content": "", "tool_calls": calls }, "done": false })
}

/// Ollama's whole non-streaming answer.
fn answer(content: &str, tool_calls: Option<Value>) -> (u16, String) {
    let mut a = done("stop");
    a["message"]["content"] = content.into();
    if let Some(c) = tool_calls {
        a["message"]["tool_calls"] = c;
    }
    (200, a.to_string())
}

// ---- gateway ---------------------------------------------------------------

struct Gateway {
    base: String,
    http: reqwest::Client,
}

fn config(ollama: SocketAddr, num_ctx: Option<&str>) -> Config {
    let num_ctx = num_ctx.map(str::to_owned);
    Config::from_lookup(|name| match name {
        "OPENGATE_API_KEY" => Some(KEY.into()),
        "OLLAMA_BASE_URL" => Some(format!("http://{ollama}")),
        "OLLAMA_MODEL" => Some("llama3.1:8b".into()),
        "OLLAMA_NUM_CTX" => num_ctx.clone(),
        _ => None,
    })
    .unwrap()
}

async fn gateway(config: Config) -> Gateway {
    Gateway {
        base: format!("http://{}", serve(opengate::app(config)).await),
        http: reqwest::Client::new(),
    }
}

impl Gateway {
    async fn send(&self, body: &Value) -> reqwest::Response {
        self.http
            .post(format!("{}/v1/chat/completions", self.base))
            .bearer_auth(KEY)
            .json(body)
            .send()
            .await
            .unwrap()
    }
}

/// The tools OpenClaw sends (custom endpoint: no `strict`).
fn tools() -> Value {
    json!([
        { "type": "function", "function": { "name": "exec", "description": "Run a shell command",
          "parameters": { "type": "object", "properties": { "command": { "type": "string" } }, "required": ["command"] } } },
        { "type": "function", "function": { "name": "read", "description": "Read a file",
          "parameters": { "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] } } },
    ])
}

/// A first agent turn, exactly as OpenClaw shapes it for `opengate/local`.
fn agent_turn(stream: bool) -> Value {
    json!({
        "model": VM,
        "messages": [
            { "role": "system", "content": "You are a personal assistant running inside OpenClaw." },
            { "role": "user", "content": "List the files in /tmp and read notes.txt" },
        ],
        "stream": stream,
        "max_completion_tokens": 4096,
        "tools": tools(),
        "tool_choice": "auto",
    })
}

/// The follow-up turn: OpenClaw ran both tools and sends the results back.
fn follow_up(stream: bool) -> Value {
    let mut body = agent_turn(stream);
    let msgs = body["messages"].as_array_mut().unwrap();
    msgs.push(json!({ "role": "assistant", "content": null, "tool_calls": [
        { "id": "call_1", "type": "function", "function": { "name": "exec", "arguments": "{\"command\":\"ls /tmp\"}" } },
        { "id": "call_2", "type": "function", "function": { "name": "read", "arguments": "{\"path\":\"/tmp/notes.txt\"}" } },
    ]}));
    msgs.push(json!({ "role": "tool", "tool_call_id": "call_1", "content": "notes.txt" }));
    msgs.push(json!({ "role": "tool", "tool_call_id": "call_2", "content": "buy milk" }));
    body
}

/// SSE `data:` payloads; asserts frame boundaries.
fn frames(body: &str) -> Vec<String> {
    assert!(
        body.ends_with("\n\n"),
        "body must end on a frame boundary: {body:?}"
    );
    body.split("\n\n")
        .filter(|f| !f.is_empty())
        .map(|f| f.strip_prefix("data: ").expect(f).to_owned())
        .collect()
}

/// Parsed chunks of a successful stream: exactly one `[DONE]`, last.
fn chunks(body: &str) -> Vec<Value> {
    let frames = frames(body);
    assert_eq!(frames.last().map(String::as_str), Some("[DONE]"), "{body}");
    assert_eq!(frames.iter().filter(|f| *f == "[DONE]").count(), 1);
    frames[..frames.len() - 1]
        .iter()
        .map(|f| serde_json::from_str(f).unwrap())
        .collect()
}

/// Accumulates streamed tool calls the way an OpenAI client does: by
/// `index`, taking `id`/`name` from the opening delta and concatenating
/// argument fragments. Returns index → (id, name, arguments).
fn reconstruct(chunks: &[Value]) -> BTreeMap<u64, (String, String, String)> {
    let mut calls: BTreeMap<u64, (String, String, String)> = BTreeMap::new();
    for c in chunks {
        for tc in c["choices"][0]["delta"]["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let entry = calls
                .entry(tc["index"].as_u64().expect("index"))
                .or_default();
            if let Some(id) = tc["id"].as_str() {
                assert!(
                    entry.0.is_empty() || entry.0 == id,
                    "tool call ID changed mid-stream"
                );
                entry.0 = id.to_owned();
            }
            if let Some(name) = tc["function"]["name"].as_str() {
                entry.1 = name.to_owned();
            }
            entry.2 += tc["function"]["arguments"].as_str().unwrap_or_default();
        }
    }
    calls
}

fn finish_reasons(chunks: &[Value]) -> Vec<String> {
    chunks
        .iter()
        .filter_map(|c| c["choices"][0]["finish_reason"].as_str().map(str::to_owned))
        .collect()
}

async fn error(res: reqwest::Response) -> (u16, Value) {
    let status = res.status().as_u16();
    (status, res.json::<Value>().await.unwrap()["error"].clone())
}

/// A stream that failed after it started: one error frame, no `[DONE]`.
async fn stream_error(res: reqwest::Response) -> Value {
    assert_eq!(res.status(), 200);
    let body = res.text().await.unwrap();
    assert!(!body.contains("[DONE]"), "{body}");
    let frames = frames(&body);
    let last: Value = serde_json::from_str(frames.last().unwrap()).unwrap();
    assert_eq!(frames.iter().filter(|f| f.contains("\"error\"")).count(), 1);
    last["error"].clone()
}

// ---- tool calling: streaming -----------------------------------------------

#[tokio::test]
async fn streaming_agent_turn_returns_multiple_tool_calls() {
    let (addr, script) = ollama(vec![ndjson(&[
        calls(json!([
            { "id": "call_ollama_7", "function": { "index": 0, "name": "exec", "arguments": { "command": "ls /tmp" } } },
            { "function": { "index": 1, "name": "read", "arguments": { "path": "/tmp/notes.txt" } } },
        ])),
        done("stop"),
    ])])
    .await;
    let gw = gateway(config(addr, Some("32768"))).await;

    let res = gw.send(&agent_turn(true)).await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-type"], "text/event-stream");
    let chunks = chunks(&res.text().await.unwrap());

    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    let calls = reconstruct(&chunks);
    assert_eq!(calls.len(), 2);
    let (id0, name0, args0) = &calls[&0];
    assert_eq!(
        (id0.as_str(), name0.as_str()),
        ("call_ollama_7", "exec"),
        "Ollama's ID is kept"
    );
    assert_eq!(
        serde_json::from_str::<Value>(args0).unwrap(),
        json!({ "command": "ls /tmp" })
    );
    let (id1, name1, args1) = &calls[&1];
    assert!(
        id1.starts_with("call_") && id1 != id0,
        "missing IDs are generated, unique"
    );
    assert_eq!(name1, "read");
    assert_eq!(
        serde_json::from_str::<Value>(args1).unwrap(),
        json!({ "path": "/tmp/notes.txt" })
    );
    assert_eq!(finish_reasons(&chunks), ["tool_calls"], "one finish event");
    for c in &chunks {
        assert_eq!(c["model"], VM);
        assert_eq!(c["id"], chunks[0]["id"]);
    }

    // What Ollama was asked: native tools, no silent truncation, the budget.
    let sent = script.last();
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["truncate"], false);
    assert_eq!(sent["shift"], false);
    assert_eq!(sent["options"]["num_ctx"], 32768);
    assert_eq!(sent["options"]["num_predict"], 4096);
    assert_eq!(sent["tools"], tools());
    assert_eq!(
        sent["messages"].as_array().unwrap().len(),
        2,
        "auto adds no instruction"
    );
}

#[tokio::test]
async fn streaming_text_with_usage_chunk() {
    let (addr, _) = ollama(vec![ndjson(&[text("Hel"), text("lo"), done("stop")])]).await;
    let gw = gateway(config(addr, None)).await;
    let mut body = agent_turn(true);
    body["stream_options"] = json!({ "include_usage": true });
    let chunks = chunks(&gw.send(&body).await.text().await.unwrap());
    let text: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text, "Hello");
    assert!(reconstruct(&chunks).is_empty());
    assert_eq!(finish_reasons(&chunks), ["stop"]);
    let usage = chunks.last().unwrap();
    assert_eq!(usage["choices"], json!([]));
    assert_eq!(
        usage["usage"],
        json!({ "prompt_tokens": 812, "completion_tokens": 31, "total_tokens": 843 })
    );
}

#[tokio::test]
async fn length_limit_is_reported_as_length() {
    let (addr, _) = ollama(vec![ndjson(&[text("partial"), done("length")])]).await;
    let gw = gateway(config(addr, None)).await;
    let chunks = chunks(&gw.send(&agent_turn(true)).await.text().await.unwrap());
    assert_eq!(finish_reasons(&chunks), ["length"]);
}

// ---- tool calling: non-streaming and multi-turn ------------------------------

#[tokio::test]
async fn nonstreaming_single_tool_call() {
    let (addr, _) = ollama(vec![answer(
        "",
        Some(json!([{ "function": { "name": "exec", "arguments": { "command": "date" } } }])),
    )])
    .await;
    let gw = gateway(config(addr, None)).await;
    let res = gw.send(&agent_turn(false)).await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-opengate-provider"], "ollama");
    let body: Value = res.json().await.unwrap();
    let choice = &body["choices"][0];
    assert_eq!(choice["finish_reason"], "tool_calls");
    assert_eq!(choice["message"]["content"], Value::Null);
    let call = &choice["message"]["tool_calls"][0];
    assert_eq!(call["type"], "function");
    assert!(call["id"].as_str().unwrap().starts_with("call_"));
    assert_eq!(call["function"]["name"], "exec");
    assert_eq!(call["function"]["arguments"], "{\"command\":\"date\"}");
    assert_eq!(body["usage"]["total_tokens"], 843);
}

#[tokio::test]
async fn tool_results_are_passed_back_for_the_final_answer() {
    let (addr, script) = ollama(vec![
        answer("You need to buy milk.", None),
        ndjson(&[text("Anything "), text("else?"), done("stop")]),
    ])
    .await;
    let gw = gateway(config(addr, None)).await;

    let body: Value = gw.send(&follow_up(false)).await.json().await.unwrap();
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "You need to buy milk."
    );
    assert_eq!(body["choices"][0]["finish_reason"], "stop");

    let sent = script.last();
    let msgs = sent["messages"].as_array().unwrap();
    assert_eq!(
        msgs[2],
        json!({ "role": "assistant", "content": "", "tool_calls": [
            { "id": "call_1", "function": { "name": "exec", "arguments": { "command": "ls /tmp" } } },
            { "id": "call_2", "function": { "name": "read", "arguments": { "path": "/tmp/notes.txt" } } },
        ]})
    );
    assert_eq!(
        msgs[3],
        json!({ "role": "tool", "content": "notes.txt", "tool_name": "exec", "tool_call_id": "call_1" })
    );
    assert_eq!(msgs[4]["tool_name"], "read");

    // Third turn: the final answer is in the history, plus a new question.
    let mut body = follow_up(true);
    let msgs = body["messages"].as_array_mut().unwrap();
    msgs.push(json!({ "role": "assistant", "content": "You need to buy milk." }));
    msgs.push(json!({ "role": "user", "content": "Thanks" }));
    let chunks = chunks(&gw.send(&body).await.text().await.unwrap());
    assert_eq!(finish_reasons(&chunks), ["stop"]);
    assert_eq!(script.last()["messages"].as_array().unwrap().len(), 7);
}

#[tokio::test]
async fn out_of_order_tool_results_are_rejected_before_calling_ollama() {
    let (addr, script) = ollama(vec![]).await;
    let gw = gateway(config(addr, None)).await;
    let mut body = follow_up(false);
    body["messages"].as_array_mut().unwrap().remove(4); // drop call_2's result
    body["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "role": "user", "content": "and?" }));
    let (status, err) = error(gw.send(&body).await).await;
    assert_eq!(
        (status, err["code"].as_str()),
        (400, Some("invalid_request"))
    );
    assert_eq!(err["param"], "messages[4]");
    assert!(script.requests().is_empty());
}

// ---- tool_choice -------------------------------------------------------------

#[tokio::test]
async fn forced_tool_choice() {
    let call = json!([{ "function": { "name": "read", "arguments": { "path": "a" } } }]);
    let (addr, script) = ollama(vec![
        answer("", Some(call)),
        answer("I'd rather not.", None),
        ndjson(&[text("I'd rather not."), done("stop")]),
    ])
    .await;
    let gw = gateway(config(addr, None)).await;
    let mut body = agent_turn(false);
    body["tool_choice"] = json!({ "type": "function", "function": { "name": "read" } });

    let res: Value = gw.send(&body).await.json().await.unwrap();
    assert_eq!(
        res["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "read"
    );
    let sent = script.last();
    assert_eq!(
        sent["tools"].as_array().unwrap().len(),
        1,
        "only the forced tool"
    );
    assert!(sent["messages"][0]["content"]
        .as_str()
        .unwrap()
        .ends_with("You must respond by calling the `read` tool."));

    // A model that ignores the requirement is an error, never a "success".
    let (status, err) = error(gw.send(&body).await).await;
    assert_eq!(
        (status, err["code"].as_str()),
        (502, Some("tool_choice_not_satisfied"))
    );
    body["stream"] = true.into();
    let err = stream_error(gw.send(&body).await).await;
    assert_eq!(err["code"], "tool_choice_not_satisfied");
}

#[tokio::test]
async fn tool_choice_none_sends_no_tools() {
    let (addr, script) = ollama(vec![answer("Just text.", None)]).await;
    let gw = gateway(config(addr, None)).await;
    let mut body = agent_turn(false);
    body["tool_choice"] = "none".into();
    let res: Value = gw.send(&body).await.json().await.unwrap();
    assert_eq!(res["choices"][0]["message"]["content"], "Just text.");
    assert!(script.last().get("tools").is_none());
}

// ---- malformed or failing upstream ---------------------------------------------

#[tokio::test]
async fn invalid_tool_arguments_from_the_model_are_an_error() {
    let bad = json!([{ "function": { "name": "exec", "arguments": "{\"command\": ls" } }]);
    let (addr, _) = ollama(vec![
        answer("", Some(bad.clone())),
        ndjson(&[calls(bad), done("stop")]),
        (200, "this is not json\n".into()),
    ])
    .await;
    let gw = gateway(config(addr, None)).await;
    let (status, err) = error(gw.send(&agent_turn(false)).await).await;
    assert_eq!(
        (status, err["code"].as_str()),
        (502, Some("upstream_invalid_tool_call"))
    );
    let err = stream_error(gw.send(&agent_turn(true)).await).await;
    assert_eq!(err["code"], "upstream_invalid_tool_call");
    let err = stream_error(gw.send(&agent_turn(true)).await).await;
    assert_eq!(err["code"], "upstream_invalid_response");
}

#[tokio::test]
async fn context_overflow_is_a_clear_400() {
    let overflow = json!({ "error": "the input length exceeds the context length" }).to_string();
    let (addr, script) = ollama(vec![
        (400, overflow.clone()),
        (500, json!({ "error": "input exceeds maximum context length and cannot be truncated further" }).to_string()),
        (200, format!("{overflow}\n")),
    ])
    .await;
    let gw = gateway(config(addr, Some("32768"))).await;

    let (status, err) = error(gw.send(&agent_turn(false)).await).await;
    assert_eq!(
        (status, err["code"].as_str()),
        (400, Some("context_length_exceeded"))
    );
    assert_eq!(err["type"], "invalid_request_error");
    let msg = err["message"].as_str().unwrap();
    // The wording OpenClaw's overflow detection (and so its compaction) matches.
    assert!(
        msg.contains("exceeds the 32768-token context window"),
        "{msg}"
    );
    assert_eq!(script.last()["options"]["num_ctx"], 32768);

    let (status, err) = error(gw.send(&agent_turn(false)).await).await;
    assert_eq!(
        (status, err["code"].as_str()),
        (400, Some("context_length_exceeded"))
    );

    let err = stream_error(gw.send(&agent_turn(true)).await).await;
    assert_eq!(err["code"], "context_length_exceeded");
}

#[tokio::test]
async fn model_without_tool_support_is_a_clear_400() {
    let (addr, _) = ollama(vec![(
        400,
        json!({ "error": "registry.ollama.ai/library/llama2:latest does not support tools" })
            .to_string(),
    )])
    .await;
    let gw = gateway(config(addr, None)).await;
    let (status, err) = error(gw.send(&agent_turn(false)).await).await;
    assert_eq!(
        (status, err["code"].as_str()),
        (400, Some("tools_not_supported"))
    );
    assert!(
        !err["message"].as_str().unwrap().contains("llama2"),
        "model name not leaked"
    );
}

#[tokio::test]
async fn tool_requests_with_ollama_unavailable_and_no_fallback() {
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let gw = gateway(config(dead, None)).await;
    let (status, err) = error(gw.send(&agent_turn(false)).await).await;
    assert_eq!(
        (status, err["code"].as_str()),
        (503, Some("upstream_unavailable"))
    );
}

// ---- validation and auth ---------------------------------------------------------

#[tokio::test]
async fn unsupported_fields_are_rejected_without_calling_ollama() {
    let (addr, script) = ollama(vec![]).await;
    let gw = gateway(config(addr, None)).await;
    for (field, value, param) in [
        ("reasoning_effort", json!("high"), "reasoning_effort"),
        ("logprobs", json!(true), "logprobs"),
        (
            "response_format",
            json!({ "type": "json_object" }),
            "response_format",
        ),
        (
            "tools",
            json!([{ "type": "function", "function": { "name": "x", "strict": true } }]),
            "tools[0].function.strict",
        ),
    ] {
        let mut body = agent_turn(false);
        body[field] = value;
        let (status, err) = error(gw.send(&body).await).await;
        assert_eq!(status, 400, "{field}");
        assert_eq!(err["param"], param, "{field}");
    }
    assert!(script.requests().is_empty());

    // Harmless OpenAI fields OpenClaw can be configured to send are accepted.
    let (addr, _) = ollama(vec![answer("ok", None)]).await;
    let gw = gateway(config(addr, None)).await;
    let mut body = agent_turn(false);
    body["store"] = false.into();
    body["prompt_cache_key"] = "session-1".into();
    body["parallel_tool_calls"] = true.into();
    assert_eq!(gw.send(&body).await.status(), 200);
}

#[tokio::test]
async fn tool_requests_need_the_api_key() {
    let (addr, script) = ollama(vec![]).await;
    let gw = gateway(config(addr, None)).await;
    let res = gw
        .http
        .post(format!("{}/v1/chat/completions", gw.base))
        .bearer_auth("not-the-key")
        .json(&agent_turn(true))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    assert!(script.requests().is_empty());
}

// ---- health ---------------------------------------------------------------------

#[tokio::test]
async fn providers_report_the_real_context_length() {
    let (addr, _) = ollama(vec![]).await;
    let gw = gateway(config(addr, Some("32768"))).await;
    let v: Value = gw
        .http
        .get(format!("{}/health/providers", gw.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["ollama"]["status"], "ready");
    assert_eq!(v["ollama"]["num_ctx"], 32768);
    assert_eq!(v["ollama"]["context_length"], 32768);
}

// ---- ChatGPT plan fallback ---------------------------------------------------------

async fn offline_with_fallback(m: &common::chatgpt::ChatGptMock) -> Gateway {
    let path = cg::temp_path();
    seed(&path, Some(cg::account(3600, PLAN_SCOPES)));
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    gateway(config_with(dead, Some(m), &path, &[])).await
}

/// OpenClaw's turn without `max_completion_tokens`, which the plan route rejects.
fn plan_turn(body: Value) -> Value {
    let mut body = body;
    body.as_object_mut()
        .unwrap()
        .remove("max_completion_tokens");
    body
}

#[tokio::test]
async fn fallback_streams_tool_calls_from_the_chatgpt_plan() {
    let m = mock().await;
    m.set_inference(Inference::ToolCalls);
    let gw = offline_with_fallback(&m).await;

    let res = gw.send(&plan_turn(agent_turn(true))).await;
    assert_eq!(res.headers()["x-opengate-provider"], "chatgpt");
    let chunks = chunks(&res.text().await.unwrap());
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    let calls = reconstruct(&chunks);
    assert_eq!(
        calls[&0],
        (
            "call_cg1".into(),
            "get_weather".into(),
            "{\"city\":\"Paris\"}".into()
        )
    );
    assert_eq!(
        calls[&1],
        ("call_cg2".into(), "get_time".into(), "{}".into())
    );
    assert_eq!(finish_reasons(&chunks), ["tool_calls"]);

    // Non-streaming: the same calls in one message.
    let res: Value = gw
        .send(&plan_turn(agent_turn(false)))
        .await
        .json()
        .await
        .unwrap();
    let msg = &res["choices"][0]["message"];
    assert_eq!(res["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(msg["content"], Value::Null);
    assert_eq!(
        msg["tool_calls"][0]["function"]["arguments"],
        "{\"city\":\"Paris\"}"
    );
    assert_eq!(msg["tool_calls"][1]["id"], "call_cg2");

    // What the plan route was sent: namespaced function tools, strict off.
    let sent = &m.bodies()[0];
    assert_eq!(sent["store"], false);
    assert_eq!(sent["tool_choice"], "auto");
    assert_eq!(sent["tools"][0]["type"], "namespace");
    assert_eq!(sent["tools"][0]["tools"][0]["name"], "exec");
    assert_eq!(sent["tools"][0]["tools"][0]["strict"], false);
}

#[tokio::test]
async fn fallback_carries_tool_history_without_duplicating_calls() {
    let m = mock().await;
    let gw = offline_with_fallback(&m).await;
    let res = gw.send(&plan_turn(follow_up(false))).await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-opengate-provider"], "chatgpt");
    let input = m.bodies()[0]["input"].clone();
    let types: Vec<_> = input
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        [
            "message",
            "message",
            "function_call",
            "function_call",
            "function_call_output",
            "function_call_output"
        ]
    );
    assert_eq!(input[2]["call_id"], "call_1");
    assert_eq!(input[2]["arguments"], "{\"command\":\"ls /tmp\"}");
    assert_eq!(
        input[5],
        json!({ "type": "function_call_output", "call_id": "call_2", "output": "buy milk" })
    );
    assert_eq!(m.responses(), 1, "sent once");
}

#[tokio::test]
async fn fallback_rejects_what_the_plan_route_cannot_do() {
    let m = mock().await;
    let gw = offline_with_fallback(&m).await;
    // OpenClaw's max_completion_tokens has no equivalent on the plan route.
    let (status, err) = error(gw.send(&agent_turn(false)).await).await;
    assert_eq!(status, 503);
    assert_eq!(err["code"], "all_providers_unavailable");
    assert_eq!(err["providers"]["chatgpt"], "unsupported_request");
    assert_eq!(err["param"], "max_tokens");
    assert!(err["message"]
        .as_str()
        .unwrap()
        .contains("CHATGPT_DROP_MAX_TOKENS"));
    assert_eq!(m.calls(), 0);

    // With the operator's explicit opt-in, the same OpenClaw turn is served.
    let path = cg::temp_path();
    seed(&path, Some(cg::account(3600, PLAN_SCOPES)));
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let gw = gateway(config_with(
        dead,
        Some(&m),
        &path,
        &[("CHATGPT_DROP_MAX_TOKENS", "true")],
    ))
    .await;
    let res = gw.send(&agent_turn(true)).await;
    assert_eq!(res.headers()["x-opengate-provider"], "chatgpt");
    assert_eq!(
        finish_reasons(&chunks(&res.text().await.unwrap())),
        ["stop"]
    );
    assert!(m.bodies()[0].get("max_output_tokens").is_none());
}

#[tokio::test]
async fn context_overflow_never_falls_back() {
    let m = mock().await;
    let path = cg::temp_path();
    seed(&path, Some(cg::account(3600, PLAN_SCOPES)));
    let (addr, _) = ollama(vec![(
        400,
        json!({ "error": "the input length exceeds the context length" }).to_string(),
    )])
    .await;
    let gw = gateway(config_with(addr, Some(&m), &path, &[])).await;
    let (status, err) = error(gw.send(&plan_turn(agent_turn(false))).await).await;
    assert_eq!(
        (status, err["code"].as_str()),
        (400, Some("context_length_exceeded"))
    );
    assert_eq!(m.calls(), 0);
}
