//! Log privacy. A separate test binary so a global subscriber captures every
//! task, at every level, including hyper's and reqwest's own trace output.

use std::sync::{Arc, Mutex};

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

async fn serve(router: Router) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    addr
}

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

    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("path=/v1/chat/completions status=200"),
        "requests are logged:\n{logs}"
    );
    for secret in [prompt, reply, KEY, "wrong-key-attempt"] {
        assert!(!logs.contains(secret), "log leaked {secret:?}:\n{logs}");
    }
}
