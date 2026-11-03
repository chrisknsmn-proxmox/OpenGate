//! A mock of the two OpenAI services the ChatGPT plan fallback talks to:
//! the Sign in with ChatGPT authorization server (OIDC discovery, JWKS,
//! token, revoke) and the Responses API (`/v1/models`, `/v1/responses`).
//! It records every bearer token and body it receives, so tests can prove
//! that only OAuth access tokens are ever sent.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use axum::{
    body::{Body, Bytes},
    extract::{Form, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::stream;
use opengate::{
    chatgpt::store::{Account, Ended, Store, StoreFile, Tokens},
    Config, CredentialsKey,
};
use serde_json::{json, Value};

pub const CRED_KEY: &str = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=";
pub const CLIENT_ID: &str = "oaiapp_test_client";
pub const PLAN_SCOPES: &str =
    "chatgpt.tokens.use.direct email offline_access openid profile resource.invoke";
pub const REPLY: &str = "Hello from ChatGPT!";

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Inference {
    /// Streams a short reply and `response.completed`.
    Ok,
    /// 401 for every token except those issued by a refresh.
    RejectInitialToken,
    /// 401 for every token.
    Unauthorized,
    /// HTTP 429 `subscription_sharing_usage_limit_exceeded` before the stream.
    LimitHttp,
    /// Starts streaming, then `response.failed` with the usage-limit code.
    LimitMidStream,
    /// HTTP 503 `subscription_sharing_usage_unavailable`.
    Unavailable,
    /// Streams forever until the client hangs up.
    Endless,
    /// Streams two function calls (arguments in fragments) and completes.
    ToolCalls,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Refresh {
    Ok,
    Revoked,
    Expired,
    Unavailable,
}

/// What the token endpoint expects for an authorization-code exchange.
#[derive(Clone)]
pub struct LoginExpect {
    pub nonce: String,
    pub code_challenge: String,
    pub id_token_aud: String,
    pub scope: String,
    pub sub: String,
}

#[derive(Default)]
pub struct Seen {
    pub responses: AtomicUsize,
    pub models: AtomicUsize,
    pub refreshes: AtomicUsize,
    pub revokes: AtomicUsize,
    pub exchanges: AtomicUsize,
    /// Every `Authorization` header received on the API.
    pub auth_headers: Mutex<Vec<String>>,
    pub bodies: Mutex<Vec<Value>>,
    pub refresh_tokens_presented: Mutex<Vec<String>>,
    pub forms: Mutex<Vec<HashMap<String, String>>>,
    pub hung_up: AtomicBool,
}

pub struct MockState {
    pub base: String,
    pub inference: Mutex<Inference>,
    pub refresh: Mutex<Refresh>,
    pub login: Mutex<Option<LoginExpect>>,
    pub seen: Seen,
}

#[derive(Clone)]
pub struct ChatGptMock(pub Arc<MockState>);

impl ChatGptMock {
    pub fn base(&self) -> &str {
        &self.0.base
    }
    pub fn set_inference(&self, i: Inference) {
        *self.0.inference.lock().unwrap() = i;
    }
    pub fn set_refresh(&self, r: Refresh) {
        *self.0.refresh.lock().unwrap() = r;
    }
    pub fn responses(&self) -> usize {
        self.0.seen.responses.load(Ordering::SeqCst)
    }
    pub fn refreshes(&self) -> usize {
        self.0.seen.refreshes.load(Ordering::SeqCst)
    }
    pub fn auth_headers(&self) -> Vec<String> {
        self.0.seen.auth_headers.lock().unwrap().clone()
    }
    pub fn bodies(&self) -> Vec<Value> {
        self.0.seen.bodies.lock().unwrap().clone()
    }
    pub fn hung_up(&self) -> bool {
        self.0.seen.hung_up.load(Ordering::SeqCst)
    }
    /// Total requests of any kind, to prove nothing was called at all.
    pub fn calls(&self) -> usize {
        let s = &self.0.seen;
        [
            &s.responses,
            &s.models,
            &s.refreshes,
            &s.revokes,
            &s.exchanges,
        ]
        .iter()
        .map(|c| c.load(Ordering::SeqCst))
        .sum()
    }
}

pub async fn mock() -> ChatGptMock {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = ChatGptMock(Arc::new(MockState {
        base: format!("http://{addr}/"),
        inference: Mutex::new(Inference::Ok),
        refresh: Mutex::new(Refresh::Ok),
        login: Mutex::new(None),
        seen: Seen::default(),
    }));
    let router = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route(
            "/jwks",
            get(|| async {
                Json(
                    serde_json::from_str::<Value>(include_str!("../fixtures/test_jwks.json"))
                        .unwrap(),
                )
            }),
        )
        .route("/oauth/token", post(token))
        .route("/oauth/revoke", post(revoke))
        .route("/v1/models", get(models))
        .route("/v1/responses", post(responses))
        .with_state(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    state
}

async fn discovery(State(m): State<ChatGptMock>) -> Json<Value> {
    let b = m.base();
    Json(json!({
        "issuer": b.trim_end_matches('/'),
        "authorization_endpoint": format!("{b}authorize"),
        "token_endpoint": format!("{b}oauth/token"),
        "revocation_endpoint": format!("{b}oauth/revoke"),
        "jwks_uri": format!("{b}jwks"),
    }))
}

pub fn sign_id_token(claims: &Value) -> String {
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(include_bytes!(
        "../fixtures/test_id_token_key.pem"
    ))
    .unwrap();
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some("test-key-1".into());
    jsonwebtoken::encode(&header, claims, &key).unwrap()
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

async fn token(
    State(m): State<ChatGptMock>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let seen = &m.0.seen;
    seen.forms.lock().unwrap().push(form.clone());
    match form.get("grant_type").map(String::as_str) {
        Some("refresh_token") => {
            let n = seen.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
            seen.refresh_tokens_presented
                .lock()
                .unwrap()
                .push(form.get("refresh_token").cloned().unwrap_or_default());
            // Simulates the network: lets concurrent callers pile up.
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(form.get("client_id").map(String::as_str), Some(CLIENT_ID));
            assert_eq!(
                form.get("resource").map(String::as_str),
                Some("https://api.openai.com/v1")
            );
            assert!(
                !form.contains_key("scope"),
                "refresh must keep the original grant"
            );
            match *m.0.refresh.lock().unwrap() {
                Refresh::Ok => Json(json!({
                    "access_token": format!("access-{n}"),
                    "refresh_token": format!("refresh-{n}"),
                    "token_type": "Bearer", "expires_in": 3600, "scope": PLAN_SCOPES,
                }))
                .into_response(),
                Refresh::Revoked => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "refresh_token_invalidated" })),
                )
                    .into_response(),
                Refresh::Expired => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "refresh_token_expired" })),
                )
                    .into_response(),
                Refresh::Unavailable => StatusCode::SERVICE_UNAVAILABLE.into_response(),
            }
        }
        Some("authorization_code") => {
            seen.exchanges.fetch_add(1, Ordering::SeqCst);
            let Some(expect) = m.0.login.lock().unwrap().clone() else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            let verifier = form.get("code_verifier").cloned().unwrap_or_default();
            let ok = form.get("code").map(String::as_str) == Some("auth-code-1")
                && opengate::chatgpt::oauth::code_challenge(&verifier) == expect.code_challenge
                && form.get("client_id").map(String::as_str) == Some(CLIENT_ID)
                && form.get("resource").map(String::as_str) == Some("https://api.openai.com/v1")
                && form
                    .get("redirect_uri")
                    .is_some_and(|r| r.starts_with("http://127.0.0.1:"))
                && !form.contains_key("client_secret");
            if !ok {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "invalid_grant" })),
                )
                    .into_response();
            }
            let id_token = sign_id_token(&json!({
                "iss": m.base().trim_end_matches('/'), "aud": expect.id_token_aud,
                "sub": expect.sub, "email": "me@example.com", "nonce": expect.nonce,
                "iat": now(), "exp": now() + 3600,
            }));
            Json(json!({
                "access_token": "access-login", "refresh_token": "refresh-login",
                "id_token": id_token, "token_type": "Bearer", "expires_in": 3600,
                "scope": expect.scope,
            }))
            .into_response()
        }
        _ => StatusCode::BAD_REQUEST.into_response(),
    }
}

async fn revoke(
    State(m): State<ChatGptMock>,
    Form(form): Form<HashMap<String, String>>,
) -> StatusCode {
    m.0.seen.revokes.fetch_add(1, Ordering::SeqCst);
    m.0.seen.forms.lock().unwrap().push(form);
    StatusCode::OK
}

fn record_auth(m: &ChatGptMock, headers: &HeaderMap) -> String {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    m.0.seen.auth_headers.lock().unwrap().push(auth.clone());
    auth
}

fn token_accepted(m: &ChatGptMock, auth: &str) -> bool {
    match *m.0.inference.lock().unwrap() {
        Inference::Unauthorized => false,
        // Tokens from a refresh look like `access-<n>`.
        Inference::RejectInitialToken => {
            auth.starts_with("Bearer access-") && auth != "Bearer access-0"
        }
        _ => auth.starts_with("Bearer access-"),
    }
}

async fn models(State(m): State<ChatGptMock>, headers: HeaderMap) -> Response {
    m.0.seen.models.fetch_add(1, Ordering::SeqCst);
    let auth = record_auth(&m, &headers);
    if !token_accepted(&m, &auth) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(json!({ "models": [
        { "slug": "hidden-model", "display_name": "Hidden", "visibility": "hide" },
        { "slug": "gpt-test", "display_name": "GPT Test", "visibility": "list" },
        { "slug": "gpt-other", "display_name": "Other", "visibility": "list" },
    ]}))
    .into_response()
}

pub fn event(v: Value) -> String {
    format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap())
}

pub fn ok_events() -> Vec<String> {
    vec![
        event(
            json!({ "type": "response.created", "response": { "id": "resp_test", "created_at": 1_700_000_000 } }),
        ),
        event(json!({ "type": "response.output_item.added", "item": { "type": "message" } })),
        event(json!({ "type": "response.output_text.delta", "delta": "Hello " })),
        event(json!({ "type": "response.output_text.delta", "delta": "from ChatGPT!" })),
        event(
            json!({ "type": "response.completed", "response": { "id": "resp_test",
            "usage": { "input_tokens": 5, "output_tokens": 4, "total_tokens": 9 } } }),
        ),
    ]
}

/// Two function calls: the first streams its arguments in fragments, the
/// second arrives whole in `output_item.done`.
pub fn tool_call_events() -> Vec<String> {
    let fc = |id: &str, call: &str, name: &str, args: &str| {
        json!({ "type": "function_call", "id": id, "call_id": call, "name": name,
            "namespace": "opengate", "arguments": args })
    };
    vec![
        event(
            json!({ "type": "response.created", "response": { "id": "resp_tools", "created_at": 1_700_000_000 } }),
        ),
        event(
            json!({ "type": "response.output_item.added", "output_index": 0, "item": fc("fc_1", "call_cg1", "get_weather", "") }),
        ),
        event(
            json!({ "type": "response.function_call_arguments.delta", "item_id": "fc_1", "output_index": 0, "delta": "{\"city\":" }),
        ),
        event(
            json!({ "type": "response.function_call_arguments.delta", "item_id": "fc_1", "output_index": 0, "delta": "\"Paris\"}" }),
        ),
        event(
            json!({ "type": "response.function_call_arguments.done", "item_id": "fc_1", "output_index": 0, "arguments": "{\"city\":\"Paris\"}" }),
        ),
        event(
            json!({ "type": "response.output_item.done", "output_index": 0, "item": fc("fc_1", "call_cg1", "get_weather", "{\"city\":\"Paris\"}") }),
        ),
        event(
            json!({ "type": "response.output_item.done", "output_index": 1, "item": fc("fc_2", "call_cg2", "get_time", "{}") }),
        ),
        event(
            json!({ "type": "response.completed", "response": { "id": "resp_tools",
            "usage": { "input_tokens": 20, "output_tokens": 9, "total_tokens": 29 } } }),
        ),
    ]
}

fn sse(parts: Vec<String>) -> Response {
    let body = stream::iter(
        parts
            .into_iter()
            .map(|p| Ok::<_, std::io::Error>(Bytes::from(p))),
    );
    (
        [("content-type", "text/event-stream")],
        Body::from_stream(body),
    )
        .into_response()
}

async fn responses(
    State(m): State<ChatGptMock>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    m.0.seen.responses.fetch_add(1, Ordering::SeqCst);
    let auth = record_auth(&m, &headers);
    m.0.seen.bodies.lock().unwrap().push(body);
    if !token_accepted(&m, &auth) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(
                json!({ "error": { "code": "subscription_sharing_invalid_user", "message": "x" } }),
            ),
        )
            .into_response();
    }
    let mode = *m.0.inference.lock().unwrap();
    match mode {
        Inference::Ok | Inference::RejectInitialToken | Inference::Unauthorized => sse(ok_events()),
        Inference::ToolCalls => sse(tool_call_events()),
        Inference::LimitHttp => (
            StatusCode::TOO_MANY_REQUESTS,
            [("x-request-id", "req_limit_1")],
            Json(json!({ "error": { "code": "subscription_sharing_usage_limit_exceeded", "message": "limit" } })),
        )
            .into_response(),
        Inference::LimitMidStream => sse(vec![
            event(json!({ "type": "response.created", "response": { "id": "resp_partial" } })),
            event(json!({ "type": "response.output_text.delta", "delta": "Partial" })),
            event(json!({ "type": "response.failed", "response": { "id": "resp_partial",
                "error": { "code": "subscription_sharing_usage_limit_exceeded", "message": "limit" } } })),
        ]),
        Inference::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": { "code": "subscription_sharing_usage_unavailable" } })),
        )
            .into_response(),
        Inference::Endless => {
            struct Hangup(ChatGptMock);
            impl Drop for Hangup {
                fn drop(&mut self) {
                    self.0 .0.seen.hung_up.store(true, Ordering::SeqCst);
                }
            }
            let guard = Hangup(m.clone());
            let body = stream::unfold(guard, |g| async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let e = event(json!({ "type": "response.output_text.delta", "delta": "tick " }));
                Some((Ok::<_, std::io::Error>(Bytes::from(e)), g))
            });
            ([("content-type", "text/event-stream")], Body::from_stream(body)).into_response()
        }
    }
}

// ---- credential seeding ----------------------------------------------------

pub fn temp_path() -> PathBuf {
    let n: u64 = rand_u64();
    std::env::temp_dir()
        .join(format!("opengate-test-{n:x}"))
        .join("chatgpt")
        .join("credentials.json")
}

fn rand_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    );
    h.finish()
}

pub fn key() -> CredentialsKey {
    CRED_KEY.parse().unwrap()
}

pub fn store(path: &Path) -> Store {
    Store::new(path.to_path_buf(), &key())
}

/// `expires_in_secs` small (< 300) makes the gateway refresh before use.
pub fn account(expires_in_secs: u64, scopes: &str) -> Account {
    Account {
        issuer: "test".into(),
        subject: "user-sub-1".into(),
        email: Some("me@example.com".into()),
        client_id: CLIENT_ID.into(),
        id_token: Some("old-id-token".into()),
        scopes: scopes.split_whitespace().map(str::to_owned).collect(),
        tokens: Some(Tokens {
            access_token: "access-0".into(),
            refresh_token: "refresh-0".into(),
            token_type: "Bearer".into(),
            expires_in: expires_in_secs,
            saved_at: now(),
            refresh_issued_at: now(),
            earliest_refresh_at: None,
        }),
        ended: None,
    }
}

pub fn seed(path: &Path, account: Option<Account>) {
    store(path)
        .save(&StoreFile {
            ext_agent_host_id: "urn:uuid:00000000-0000-4000-8000-000000000001".into(),
            account,
        })
        .unwrap();
}

pub fn signed_out(ended: Ended) -> Account {
    let mut a = account(3600, PLAN_SCOPES);
    a.tokens = None;
    a.ended = Some(ended);
    a
}

pub fn load(path: &Path) -> StoreFile {
    store(path).load().unwrap().unwrap().0
}

/// Gateway config: `ollama` as the local upstream (WOL off), the ChatGPT
/// fallback pointed at `mock`, credentials at `path`.
pub fn config_with(
    ollama: std::net::SocketAddr,
    mock: Option<&ChatGptMock>,
    path: &Path,
    extra: &[(&str, &str)],
) -> Config {
    let path_s = path.to_string_lossy().to_string();
    let extra: HashMap<String, String> = extra
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let mut c = Config::from_lookup(|name| {
        if let Some(v) = extra.get(name) {
            return Some(v.clone());
        }
        match name {
            "OPENGATE_API_KEY" => Some(super::KEY.into()),
            "OLLAMA_BASE_URL" => Some(format!("http://{ollama}")),
            "OLLAMA_MODEL" => Some("llama3.1:8b".into()),
            "CHATGPT_FALLBACK_ENABLED" => Some(mock.is_some().to_string()),
            "CHATGPT_CREDENTIALS_KEY" => Some(CRED_KEY.into()),
            "CHATGPT_CREDENTIALS_PATH" => Some(path_s.clone()),
            _ => None,
        }
    })
    .unwrap();
    if let (Some(cg), Some(m)) = (c.chatgpt.as_mut(), mock) {
        point_at(cg, m);
    }
    c
}

pub fn point_at(cg: &mut opengate::ChatGptConfig, m: &ChatGptMock) {
    cg.issuer = m.base().parse().unwrap();
    cg.api_base = format!("{}v1/", m.base()).parse().unwrap();
    cg.limit_pause = Duration::from_secs(60);
}
