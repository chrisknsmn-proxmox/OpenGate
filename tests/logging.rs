//! Log privacy. A separate test binary so a global subscriber captures every
//! task, at every level, including hyper's and reqwest's own trace output.

mod common;

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{routing::post, Json, Router};
use opengate::Config;
use serde_json::{json, Value};

const KEY: &str = "log-test-key-0123456789abcdef012345";

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

use common::serve;

#[tokio::test]
async fn logs_never_contain_prompts_responses_or_keys() {
    let logs = Capture::default();
    let writer = logs.clone();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .init();

    let reply = "purple-elephant-model-reply";
    let mock = Router::new().route(
        "/v1/chat/completions",
        post(move || async move {
            Json(json!({
                "id": "c", "object": "chat.completion", "created": 1, "model": "llama3.1:8b",
                "choices": [{ "index": 0, "message": { "role": "assistant", "content": reply }, "finish_reason": "stop" }],
            }))
        }),
    );
    let upstream = serve(mock).await;
    let config = Config::from_lookup(|name| match name {
        "OPENGATE_API_KEY" => Some(KEY.into()),
        "OLLAMA_BASE_URL" => Some(format!("http://{upstream}")),
        "OLLAMA_MODEL" => Some("llama3.1:8b".into()),
        _ => None,
    })
    .unwrap();
    let base = format!("http://{}", serve(opengate::app(config)).await);
    let http = reqwest::Client::new();
    let chat = |body: Value, key: &'static str| {
        http.post(format!("{base}/v1/chat/completions"))
            .bearer_auth(key)
            .json(&body)
            .send()
    };

    let prompt = "purple-elephant-secret-prompt";
    let messages = json!([{ "role": "user", "content": prompt }]);
    let ok = chat(
        json!({ "model": "opengate/local", "messages": messages }),
        KEY,
    )
    .await
    .unwrap();
    assert_eq!(ok.status(), 200);
    assert!(ok.text().await.unwrap().contains(reply));
    let bad = json!({ "model": "opengate/local", "messages": messages, "temperature": "hot" });
    assert_eq!(chat(bad, KEY).await.unwrap().status(), 400);
    let wrong = json!({ "model": "opengate/local", "messages": messages });
    assert_eq!(
        chat(wrong, "wrong-key-attempt").await.unwrap().status(),
        401
    );

    // A full Wake-on-LAN cycle is logged, still without secrets.
    let pc = common::pc(common::Power::Asleep(Some(Duration::from_millis(100)))).await;
    let base = format!(
        "http://{}",
        serve(opengate::app(common::config(&pc, true))).await
    );
    for _ in 0..2 {
        // The first request wakes the PC; the second finds it online.
        let res = http
            .post(format!("{base}/v1/chat/completions"))
            .bearer_auth(common::KEY)
            .json(&json!({ "model": "opengate/local", "messages": messages }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    // A ChatGPT plan fallback with a token refresh and a usage-limit error.
    let m = common::chatgpt::mock().await;
    let path = common::chatgpt::temp_path();
    common::chatgpt::seed(
        &path,
        Some(common::chatgpt::account(10, common::chatgpt::PLAN_SCOPES)),
    );
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let cfg = common::chatgpt::config_with(dead, Some(&m), &path, &[]);
    let base = format!("http://{}", serve(opengate::app(cfg)).await);
    let send = |stream: bool| {
        http.post(format!("{base}/v1/chat/completions"))
            .bearer_auth(common::KEY)
            .json(&json!({ "model": "opengate/local", "messages": messages, "stream": stream }))
            .send()
    };
    let res = send(false).await.unwrap();
    assert_eq!(res.status(), 200);
    assert!(res.text().await.unwrap().contains(common::chatgpt::REPLY));
    assert_eq!(send(true).await.unwrap().status(), 200);
    m.set_inference(common::chatgpt::Inference::LimitHttp);
    assert_eq!(send(false).await.unwrap().status(), 429);

    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    for event in [
        "refreshing ChatGPT access token",
        "trying the ChatGPT plan fallback",
        "serving request from the ChatGPT plan",
        "ChatGPT plan usage limit reached",
    ] {
        assert!(logs.contains(event), "missing log {event:?}:\n{logs}");
    }
    for secret in [
        "access-0",
        "refresh-0",
        "access-1",
        "refresh-1",
        "Hello from",
        common::chatgpt::CRED_KEY,
    ] {
        assert!(!logs.contains(secret), "log leaked {secret:?}:\n{logs}");
    }
    assert!(
        logs.contains("path=/v1/chat/completions status=200"),
        "requests are logged:\n{logs}"
    );
    for event in [
        "Ollama connectivity check: unreachable",
        "sent Wake-on-LAN magic packet",
        "waiting for the Ollama host to start",
        "Ollama not reachable yet",
        "Ollama became available after wake",
        "resuming request after wake",
        "Ollama connectivity check: online",
    ] {
        assert!(logs.contains(event), "missing log {event:?}:\n{logs}");
    }
    for secret in [prompt, reply, KEY, common::KEY, "wrong-key-attempt"] {
        assert!(!logs.contains(secret), "log leaked {secret:?}:\n{logs}");
    }
}
