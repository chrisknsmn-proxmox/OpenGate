# OpenGate — Step 1: Minimal Gateway

**Goal:** one Rust server, one endpoint, three providers. No streaming, no database, no UI yet.

```
your app ──► OpenGate :8080 ──► ollama/*   → local Ollama
                             ├─► claude/*   → Anthropic API (API key)
                             └─► chatgpt/*  → ChatGPT subscription (Step 2)
```

The model name picks the provider: `ollama/llama3`, `claude/claude-haiku-5-5`, `chatgpt/...`.

---

## 1. Create the project

```bash
cargo new opengate && cd opengate
```

**Cargo.toml**

```toml
[package]
name = "opengate"
version = "0.1.0"
edition = "2021"

[dependencies]
axum = "0.8"
tokio = { version = "1", features = ["full"] }
reqwest = { version = "0.12", features = ["json"] }
serde_json = "1"
anyhow = "1"
```

## 2. src/main.rs

```rust
use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use serde_json::{json, Value};
use std::{env, sync::Arc, time::Instant};

struct AppState {
    http: reqwest::Client,
    ollama_url: String,
    anthropic_key: Option<String>,
}

#[tokio::main]
async fn main() {
    let state = Arc::new(AppState {
        http: reqwest::Client::new(),
        ollama_url: env::var("OLLAMA_URL").unwrap_or("http://localhost:11434".into()),
        anthropic_key: env::var("ANTHROPIC_API_KEY").ok(),
    });

    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
    println!("OpenGate listening on :8080");
    axum::serve(listener, app).await.unwrap();
}

async fn chat(
    State(s): State<Arc<AppState>>,
    Json(mut body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let model = body["model"].as_str().unwrap_or("").to_string();
    let (provider, name) = model
        .split_once('/')
        .ok_or((StatusCode::BAD_REQUEST, "model must be provider/name".to_string()))?;

    body["model"] = json!(name);
    body["stream"] = json!(false); // streaming comes later

    let start = Instant::now();
    let result = match provider {
        "ollama" => ollama(&s, &body).await,
        "claude" => claude(&s, &body).await,
        "chatgpt" => Err(anyhow::anyhow!("chatgpt provider: see Step 2")),
        other => Err(anyhow::anyhow!("unknown provider: {other}")),
    };

    // Step 1 logging: stdout. SQLite comes later.
    println!("{model} {:?} ok={}", start.elapsed(), result.is_ok());

    result
        .map(Json)
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))
}

// Ollama already speaks the OpenAI format — pure passthrough.
async fn ollama(s: &AppState, body: &Value) -> anyhow::Result<Value> {
    let res = s
        .http
        .post(format!("{}/v1/chat/completions", s.ollama_url))
        .json(body)
        .send()
        .await?
        .error_for_status()?;
    Ok(res.json().await?)
}

// Anthropic uses its own format — translate in, translate out.
async fn claude(s: &AppState, body: &Value) -> anyhow::Result<Value> {
    let key = s
        .anthropic_key
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("ANTHROPIC_API_KEY not set"))?;

    let msgs = body["messages"].as_array().cloned().unwrap_or_default();
    let system: Vec<String> = msgs
        .iter()
        .filter(|m| m["role"] == "system")
        .filter_map(|m| m["content"].as_str().map(String::from))
        .collect();
    let chat: Vec<Value> = msgs.into_iter().filter(|m| m["role"] != "system").collect();

    let mut req = json!({
        "model": body["model"],
        "max_tokens": body["max_tokens"].as_u64().unwrap_or(1024),
        "messages": chat,
    });
    if !system.is_empty() {
        req["system"] = json!(system.join("\n"));
    }

    let res: Value = s
        .http
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .json(&req)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let text: String = res["content"]
        .as_array()
        .map(|blocks| blocks.iter().filter_map(|b| b["text"].as_str()).collect())
        .unwrap_or_default();

    Ok(json!({
        "id": res["id"],
        "object": "chat.completion",
        "model": res["model"],
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": text },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": res["usage"]["input_tokens"],
            "completion_tokens": res["usage"]["output_tokens"]
        }
    }))
}
```

## 3. Run it

```bash
export ANTHROPIC_API_KEY=sk-ant-...   # optional
cargo run
```

## 4. Test it

```bash
# Local model (use a name from `ollama list`)
curl localhost:8080/v1/chat/completions -H "Content-Type: application/json" -d '{
  "model": "ollama/YOUR_MODEL",
  "messages": [{"role": "user", "content": "Say hi in five words"}]
}'

# Claude
curl localhost:8080/v1/chat/completions -H "Content-Type: application/json" -d '{
  "model": "claude/claude-haiku-5-5",
  "messages": [{"role": "user", "content": "Say hi in five words"}]
}'
```

Run the same prompt against both of your Ollama models and compare the timings in the terminal. That's your first comparison.

**Step 1 is done when:** both curl calls return answers and the terminal logs model + latency.

---

## Step 2: ChatGPT subscription (no API key)

Use OpenAI's official **Sign in with ChatGPT** flow for open-source apps:

1. Follow OpenAI's Cookbook guide, *Integrating Sign in with ChatGPT in your open-source app*.
2. Run the sign-in once, approve **Use your ChatGPT plan**, and store the token outside the repo.
3. Add a `chatgpt` function next to `claude`, sending requests through the Responses API with that token and translating the reply the same way.
4. Never log the token.

Limits to know:
- Text only. Image generation isn't supported on plan usage.
- Usage draws from your plan's Codex/Work allowance. Set a weekly cap for OpenGate in ChatGPT → Settings → Usage.
- Keep it personal: if OpenGate goes public, lock this provider to your own key.

## What's next (not now)

1. Streaming (SSE)
2. SQLite logging
3. Profiles in a config file
4. Shadow mode to compare models
