use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    env,
    sync::Arc,
    time::{Duration, Instant},
};

struct AppState {
    http: reqwest::Client,
    api_key: Option<String>,
    ollama_url: String,
    ollama_timeout: Duration,
    anthropic_key: Option<String>,
    openai_url: String,
    openai_key: Option<String>,
    router_model: String,
    router_prompt: Option<String>,
    local_model: String,
    chatgpt_model: String,
}

fn var(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.into())
}

// Coolify can leave variables defined but blank — treat those as unset.
fn secret(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.is_empty())
}

#[tokio::main]
async fn main() {
    let state = Arc::new(AppState {
        http: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5)) // fail fast so fallback kicks in
            .build()
            .unwrap(),
        api_key: secret("OPENGATE_API_KEY"),
        ollama_url: var("OLLAMA_URL", "http://localhost:11434"),
        ollama_timeout: Duration::from_secs(var("OLLAMA_TIMEOUT_SECS", "120").parse().unwrap_or(120)),
        anthropic_key: secret("ANTHROPIC_API_KEY"),
        openai_url: var("OPENAI_URL", "https://api.openai.com"),
        openai_key: secret("OPENAI_API_KEY"),
        router_model: var("ROUTER_MODEL", "tev1"),
        router_prompt: secret("ROUTER_PROMPT"),
        local_model: var("LOCAL_MODEL", "llama3"),
        chatgpt_model: var("CHATGPT_MODEL", "gpt-5-mini"),
    });
    if state.api_key.is_none() {
        println!("warning: OPENGATE_API_KEY not set — anyone who can reach this port can use it");
    }

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .with_state(state);

    let port = var("PORT", "8080");
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await.unwrap();
    println!("OpenGate listening on :{port}");
    axum::serve(listener, app).await.unwrap();
}

fn authorized(s: &AppState, headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    let Some(key) = &s.api_key else { return Ok(()) };
    let sent = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if sent == Some(key.as_str()) {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "invalid or missing API key".to_string()))
    }
}

async fn models(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    authorized(&s, &headers)?;
    let ids = [
        "auto".to_string(),
        format!("ollama/{}", s.local_model),
        format!("chatgpt/{}", s.chatgpt_model),
    ];
    let data: Vec<Value> = ids
        .iter()
        .map(|id| json!({ "id": id, "object": "model", "owned_by": "opengate" }))
        .collect();
    Ok(Json(json!({ "object": "list", "data": data })))
}

async fn chat(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    authorized(&s, &headers)?;
    let model = body["model"].as_str().unwrap_or("").to_string();
    body["stream"] = json!(false); // streaming comes later

    let start = Instant::now();
    let (route, result) = if model == "auto" {
        auto(&s, body).await
    } else {
        let (provider, name) = model
            .split_once('/')
            .ok_or((StatusCode::BAD_REQUEST, "model must be auto or provider/name".to_string()))?;
        (model.clone(), call(&s, provider, name, body).await)
    };

    // Step 1 logging: stdout. SQLite comes later.
    println!("{model} -> {route} {:?} ok={}", start.elapsed(), result.is_ok());

    result
        .map(Json)
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))
}

async fn call(s: &AppState, provider: &str, name: &str, mut body: Value) -> anyhow::Result<Value> {
    body["model"] = json!(name);
    match provider {
        "ollama" => ollama(s, &body).await,
        "claude" => claude(s, &body).await,
        "chatgpt" => chatgpt(s, &body).await,
        other => Err(anyhow::anyhow!("unknown provider: {other}")),
    }
}

// The router model picks local or chatgpt; if local fails, ChatGPT is the fallback.
async fn auto(s: &AppState, body: Value) -> (String, anyhow::Result<Value>) {
    let use_chatgpt = decide(s, &body).await.unwrap_or_else(|e| {
        println!("router {} failed, trying local: {e}", s.router_model);
        false
    });
    let chatgpt_route = format!("chatgpt/{}", s.chatgpt_model);
    if use_chatgpt {
        return (chatgpt_route, call(s, "chatgpt", &s.chatgpt_model, body).await);
    }

    let local_route = format!("ollama/{}", s.local_model);
    match call(s, "ollama", &s.local_model, body.clone()).await {
        Ok(v) => (local_route, Ok(v)),
        Err(e) => {
            println!("{local_route} failed, falling back to chatgpt: {e}");
            let result = call(s, "chatgpt", &s.chatgpt_model, body).await;
            (format!("{chatgpt_route} (fallback)"), result)
        }
    }
}

// Ask the router model about the latest user message. Returns true for chatgpt.
async fn decide(s: &AppState, body: &Value) -> anyhow::Result<bool> {
    let mut messages = vec![];
    if let Some(prompt) = &s.router_prompt {
        messages.push(json!({ "role": "system", "content": prompt }));
    }
    messages.push(json!({ "role": "user", "content": last_user_text(body) }));

    let res = ollama(
        s,
        &json!({ "model": s.router_model, "messages": messages, "temperature": 0, "stream": false }),
    )
    .await?;
    let reply = res["choices"][0]["message"]["content"].as_str().unwrap_or("");
    // Skip any <think>…</think> block, then take whichever answer appears first.
    let reply = reply.rsplit("</think>").next().unwrap_or_default().to_lowercase();
    println!("router {} said {:?}", s.router_model, reply.trim());

    let first = |words: &[&str]| words.iter().filter_map(|w| reply.find(w)).min();
    Ok(match (first(&["chatgpt", "cloud", "remote"]), first(&["local"])) {
        (Some(c), Some(l)) => c < l,
        (c, _) => c.is_some(),
    })
}

fn last_user_text(body: &Value) -> String {
    let content = body["messages"]
        .as_array()
        .and_then(|msgs| msgs.iter().rev().find(|m| m["role"] == "user"))
        .map(|m| m["content"].clone())
        .unwrap_or_default();
    match content {
        Value::String(text) => text,
        // OpenAI-style content parts: keep only the text ones.
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

// Send a request and return its JSON, keeping the upstream error message on failure.
async fn send(req: reqwest::RequestBuilder) -> anyhow::Result<Value> {
    let res = req.send().await?;
    let status = res.status();
    if !status.is_success() {
        anyhow::bail!("upstream {status}: {}", res.text().await.unwrap_or_default());
    }
    Ok(res.json().await?)
}

// Ollama already speaks the OpenAI format — pure passthrough.
async fn ollama(s: &AppState, body: &Value) -> anyhow::Result<Value> {
    send(
        s.http
            .post(format!("{}/v1/chat/completions", s.ollama_url))
            .timeout(s.ollama_timeout)
            .json(body),
    )
    .await
}

// OpenAI is the native format; newer models only accept max_completion_tokens.
async fn chatgpt(s: &AppState, body: &Value) -> anyhow::Result<Value> {
    let key = s
        .openai_key
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("OPENAI_API_KEY not set"))?;

    let mut body = body.clone();
    if let Some(max) = body.as_object_mut().and_then(|b| b.remove("max_tokens")) {
        body["max_completion_tokens"] = max;
    }

    send(
        s.http
            .post(format!("{}/v1/chat/completions", s.openai_url))
            .bearer_auth(key)
            .json(&body),
    )
    .await
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

    let res = send(
        s.http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .json(&req),
    )
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
