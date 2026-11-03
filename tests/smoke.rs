//! Optional smoke tests against a real, running OpenGate (and through it the
//! real Ollama model, or the ChatGPT plan if Ollama is unavailable).
//!
//! Ignored by default: they need a deployment, an API key, and they spend
//! real inference (and may wake the Ollama PC). Run them explicitly:
//!
//! ```sh
//! OPENGATE_SMOKE_BASE_URL=http://<VM-104-LAN-IP>:8100 \
//! OPENGATE_SMOKE_API_KEY=... \
//!   cargo test --test smoke -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! They check protocol shape, not answer quality: a small local model may
//! decline to call a tool, which is reported rather than failed where noted.

use std::time::Duration;

use serde_json::{json, Value};

struct Target {
    base: String,
    key: String,
    http: reqwest::Client,
}

fn target() -> Target {
    let var = |name: &str| {
        std::env::var(name).unwrap_or_else(|_| panic!("set {name} to run the smoke tests"))
    };
    Target {
        base: var("OPENGATE_SMOKE_BASE_URL")
            .trim_end_matches('/')
            .to_owned(),
        key: var("OPENGATE_SMOKE_API_KEY"),
        // Long enough for a Wake-on-LAN boot plus a cold model load.
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()
            .unwrap(),
    }
}

impl Target {
    async fn chat(&self, body: &Value) -> reqwest::Response {
        self.http
            .post(format!("{}/v1/chat/completions", self.base))
            .bearer_auth(&self.key)
            .json(body)
            .send()
            .await
            .unwrap()
    }
}

fn model() -> String {
    std::env::var("OPENGATE_SMOKE_MODEL").unwrap_or_else(|_| "opengate/local".into())
}

fn weather_tool() -> Value {
    json!([{ "type": "function", "function": {
        "name": "get_weather", "description": "Current weather for a city",
        "parameters": { "type": "object", "properties": { "city": { "type": "string" } }, "required": ["city"] },
    }}])
}

#[tokio::test]
#[ignore = "needs a running OpenGate; see the module docs"]
async fn smoke_chat() {
    let t = target();
    let res = t
        .chat(&json!({ "model": model(), "messages": [{ "role": "user", "content": "Reply with the word OK." }] }))
        .await;
    let provider = res.headers().get("x-opengate-provider").cloned();
    assert_eq!(res.status(), 200, "{}", res.text().await.unwrap());
    let body: Value = res.json().await.unwrap();
    println!(
        "provider={provider:?} answer={}",
        body["choices"][0]["message"]["content"]
    );
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
}

#[tokio::test]
#[ignore = "needs a running OpenGate; see the module docs"]
async fn smoke_stream() {
    let t = target();
    let res = t
        .chat(
            &json!({ "model": model(), "stream": true, "stream_options": { "include_usage": true },
            "messages": [{ "role": "user", "content": "Count from 1 to 5." }] }),
        )
        .await;
    assert_eq!(res.status(), 200);
    let body = res.text().await.unwrap();
    assert!(body.ends_with("data: [DONE]\n\n"), "{body}");
    assert_eq!(body.matches("[DONE]").count(), 1);
    assert!(body.contains("\"finish_reason\":\"stop\""), "{body}");
}

#[tokio::test]
#[ignore = "needs a running OpenGate; see the module docs"]
async fn smoke_tool_round_trip() {
    let t = target();
    let mut messages =
        vec![json!({ "role": "user", "content": "What is the weather in Paris? Use the tool." })];
    let res = t
        .chat(&json!({ "model": model(), "messages": messages, "tools": weather_tool(), "tool_choice": "auto" }))
        .await;
    assert_eq!(res.status(), 200, "{}", res.text().await.unwrap());
    let body: Value = res.json().await.unwrap();
    let msg = body["choices"][0]["message"].clone();
    let Some(call) = msg["tool_calls"].get(0).cloned() else {
        println!("the model answered without calling the tool: {msg}");
        return;
    };
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    let args: Value =
        serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
    println!("tool call: {} {args}", call["function"]["name"]);

    // OpenGate never runs tools: the test plays the client and answers.
    messages.push(msg);
    messages
        .push(json!({ "role": "tool", "tool_call_id": call["id"], "content": "18C and sunny" }));
    let res = t
        .chat(&json!({ "model": model(), "messages": messages, "tools": weather_tool(), "stream": true }))
        .await;
    assert_eq!(res.status(), 200);
    let body = res.text().await.unwrap();
    assert!(body.ends_with("data: [DONE]\n\n"), "{body}");
}
