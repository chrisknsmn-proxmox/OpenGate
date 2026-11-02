//! Local-first routing with the ChatGPT plan fallback: a real OpenGate in
//! front of a mock Ollama PC (with Wake-on-LAN) and a mock of OpenAI's sign-in
//! and Responses services.

mod common;

use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{routing::post, Json, Router};
use common::{
    chatgpt::{
        self as cg, account, config_with, load, mock, seed, signed_out, ChatGptMock, Inference,
        Refresh, PLAN_SCOPES, REPLY,
    },
    hello, pc, serve, Power, KEY,
};
use opengate::{chatgpt::store::Ended, Config};
use serde_json::{json, Value};

struct Gateway {
    base: String,
    http: reqwest::Client,
}

async fn gateway(config: Config) -> Gateway {
    Gateway {
        base: format!("http://{}", serve(opengate::app(config)).await),
        http: reqwest::Client::new(),
    }
}

impl Gateway {
    async fn send(&self, body: Value) -> reqwest::Response {
        self.http
            .post(format!("{}/v1/chat/completions", self.base))
            .bearer_auth(KEY)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn chat(&self, stream: bool) -> reqwest::Response {
        self.send(hello(stream)).await
    }

    async fn providers(&self) -> Value {
        self.http
            .get(format!("{}/health/providers", self.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
}

async fn dead_port() -> SocketAddr {
    tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
}

fn provider(res: &reqwest::Response) -> String {
    res.headers()["x-opengate-provider"]
        .to_str()
        .unwrap()
        .to_owned()
}

async fn error(res: reqwest::Response) -> (u16, Value) {
    let status = res.status().as_u16();
    (status, res.json::<Value>().await.unwrap()["error"].clone())
}

/// Ollama down (WOL off), fallback pointed at `m`, credentials seeded.
async fn offline_gateway(
    m: &ChatGptMock,
    account: Option<opengate::chatgpt::store::Account>,
) -> (Gateway, std::path::PathBuf) {
    let path = cg::temp_path();
    seed(&path, account);
    let gw = gateway(config_with(dead_port().await, Some(m), &path, &[])).await;
    (gw, path)
}

fn plan_account() -> opengate::chatgpt::store::Account {
    account(3600, PLAN_SCOPES)
}

// ---- local first ---------------------------------------------------------

#[tokio::test]
async fn ollama_online_never_uses_chatgpt() {
    let m = mock().await;
    let pc = pc(Power::On).await;
    let path = cg::temp_path();
    seed(&path, Some(plan_account()));
    let mut cfg = common::config(&pc, true);
    cfg.chatgpt = config_with(pc.ollama, Some(&m), &path, &[]).chatgpt;
    let gw = gateway(cfg).await;

    for stream in [false, true] {
        let res = gw.chat(stream).await;
        assert_eq!(res.status(), 200);
        assert_eq!(provider(&res), "ollama");
        assert!(res.text().await.unwrap().contains("Hello!"));
    }
    assert_eq!(pc.counters.packets(), 0);
    assert_eq!(m.calls(), 0, "ChatGPT was never contacted");
}

#[tokio::test]
async fn offline_ollama_is_woken_and_used_without_fallback() {
    let m = mock().await;
    let pc = pc(Power::Asleep(Some(Duration::from_millis(200)))).await;
    let path = cg::temp_path();
    seed(&path, Some(plan_account()));
    let mut cfg = common::config(&pc, true);
    cfg.chatgpt = config_with(pc.ollama, Some(&m), &path, &[]).chatgpt;
    let gw = gateway(cfg).await;

    let res = gw.chat(false).await;
    assert_eq!(res.status(), 200);
    assert_eq!(provider(&res), "ollama");
    assert_eq!(pc.counters.packets(), 1, "Wake-on-LAN was attempted");
    assert_eq!(pc.counters.chats(), 1);
    assert_eq!(m.calls(), 0);
}

#[tokio::test]
async fn wol_timeout_falls_back_to_the_chatgpt_plan() {
    let m = mock().await;
    let pc = pc(Power::Asleep(None)).await;
    let path = cg::temp_path();
    seed(&path, Some(plan_account()));
    let mut cfg = common::config(&pc, true);
    cfg.wol.as_mut().unwrap().startup_timeout = Duration::from_millis(300);
    cfg.chatgpt = config_with(pc.ollama, Some(&m), &path, &[]).chatgpt;
    let gw = gateway(cfg).await;

    let start = Instant::now();
    let body = json!({ "model": "opengate/local", "messages": [
        { "role": "system", "content": "be brief" },
        { "role": "user", "content": "Say hello." },
    ]});
    let res = gw.send(body).await;
    assert!(
        start.elapsed() >= Duration::from_millis(300),
        "waited for the wake first"
    );
    assert_eq!(res.status(), 200);
    assert_eq!(provider(&res), "chatgpt");
    let v: Value = res.json().await.unwrap();
    assert_eq!(v["id"], "resp_test", "response ID preserved");
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(
        v["model"], "opengate/local",
        "clients keep seeing the virtual model"
    );
    assert_eq!(v["choices"][0]["message"]["content"], REPLY);
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    assert_eq!(v["usage"]["total_tokens"], 9);
    assert_eq!(pc.counters.packets(), 1);

    // The request that reached OpenAI follows the documented contract.
    let sent = &m.bodies()[0];
    assert_eq!(sent["store"], false);
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["model"], "gpt-test", "auto = first listed model");
    assert_eq!(
        sent["input"][0]["role"], "developer",
        "no system items on this route"
    );
    assert_eq!(
        sent["input"][1],
        json!({ "type": "message", "role": "user", "content": "Say hello." })
    );
    let keys: Vec<&String> = sent.as_object().unwrap().keys().collect();
    assert_eq!(keys.len(), 4, "only input/model/store/stream: {keys:?}");
    assert!(m.auth_headers().iter().all(|h| h == "Bearer access-0"));
}

#[tokio::test]
async fn streaming_fallback_translates_events() {
    let m = mock().await;
    let (gw, _) = offline_gateway(&m, Some(plan_account())).await;
    let mut body = hello(true);
    body["stream_options"] = json!({ "include_usage": true });
    let res = gw.send(body).await;
    assert_eq!(res.status(), 200);
    assert_eq!(provider(&res), "chatgpt");
    assert_eq!(res.headers()["content-type"], "text/event-stream");
    let text = res.text().await.unwrap();
    let frames: Vec<Value> = text
        .split("\n\n")
        .filter_map(|f| f.strip_prefix("data: "))
        .filter(|f| *f != "[DONE]")
        .map(|f| serde_json::from_str(f).unwrap())
        .collect();
    assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
    assert_eq!(
        frames[0]["choices"][0]["delta"],
        json!({ "role": "assistant", "content": "Hello " })
    );
    assert_eq!(
        frames[1]["choices"][0]["delta"],
        json!({ "content": "from ChatGPT!" })
    );
    assert_eq!(frames[2]["choices"][0]["finish_reason"], "stop");
    assert_eq!(frames[3]["choices"], json!([]));
    assert_eq!(frames[3]["usage"]["prompt_tokens"], 5);
    for f in &frames {
        assert_eq!(f["id"], "resp_test");
        assert_eq!(f["object"], "chat.completion.chunk");
        assert_eq!(f["model"], "opengate/local");
    }
}

#[tokio::test]
async fn cloud_unavailable_while_local_keeps_working() {
    let pc = pc(Power::On).await;
    let path = cg::temp_path();
    seed(&path, Some(plan_account()));
    let mut cfg = common::config(&pc, true);
    let mut chatgpt = config_with(
        pc.ollama,
        None,
        &path,
        &[("CHATGPT_FALLBACK_ENABLED", "true")],
    )
    .chatgpt
    .unwrap();
    let dead = format!("http://{}/", dead_port().await);
    chatgpt.issuer = dead.parse().unwrap();
    chatgpt.api_base = format!("{dead}v1/").parse().unwrap();
    cfg.chatgpt = Some(chatgpt);
    let gw = gateway(cfg).await;

    let res = gw.chat(true).await;
    assert_eq!(res.status(), 200);
    assert_eq!(provider(&res), "ollama");
    let health = gw.providers().await;
    assert_eq!(health["gateway"]["status"], "ok");
    assert_eq!(health["ollama"]["status"], "ready");
    assert_eq!(health["chatgpt"]["status"], "authorized");
}

#[tokio::test]
async fn fallback_disabled_keeps_existing_errors() {
    let path = cg::temp_path();
    let gw = gateway(config_with(dead_port().await, None, &path, &[])).await;
    let (status, err) = error(gw.chat(false).await).await;
    assert_eq!(status, 503);
    assert_eq!(err["code"], "upstream_unavailable");
    assert!(err.get("providers").is_none());
    assert_eq!(gw.providers().await["chatgpt"]["status"], "disabled");
}

#[tokio::test]
async fn local_rejections_and_mid_stream_failures_never_fall_back() {
    let m = mock().await;
    let path = cg::temp_path();
    seed(&path, Some(plan_account()));

    // Ollama is up but refuses this request (4xx): not an availability problem.
    let rejecting = serve(Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({ "error": { "message": "context too long" } })),
            )
        }),
    ))
    .await;
    let gw = gateway(config_with(rejecting, Some(&m), &path, &[])).await;
    let (status, err) = error(gw.chat(false).await).await;
    assert_eq!(
        (status, err["code"].as_str()),
        (502, Some("upstream_error"))
    );

    // Ollama accepted (HTTP 200) and then broke mid-stream: already committed.
    let broken = serve(Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            let chunk = json!({ "id": "c", "object": "chat.completion.chunk", "created": 1, "model": "x",
                "choices": [{ "index": 0, "delta": { "content": "Hel" }, "finish_reason": null }] });
            ([("content-type", "text/event-stream")], format!("data: {chunk}\n\n"))
        }),
    ))
    .await;
    let gw = gateway(config_with(broken, Some(&m), &path, &[])).await;
    let res = gw.chat(true).await;
    assert_eq!(provider(&res), "ollama");
    let text = res.text().await.unwrap();
    assert!(
        text.contains("Hel")
            && text.contains("upstream_stream_incomplete")
            && !text.contains("[DONE]")
    );

    assert_eq!(m.calls(), 0, "neither case reached ChatGPT");
}

// ---- authorization states -------------------------------------------------

#[tokio::test]
async fn missing_or_signed_out_credentials_are_a_safe_error() {
    let m = mock().await;
    // Never signed in: no credential file at all.
    let path = cg::temp_path();
    let gw = gateway(config_with(dead_port().await, Some(&m), &path, &[])).await;
    let (status, err) = error(gw.chat(false).await).await;
    assert_eq!(status, 503);
    assert_eq!(err["code"], "all_providers_unavailable");
    assert_eq!(
        err["providers"],
        json!({ "ollama": "ollama_unavailable", "chatgpt": "subscription_not_configured" })
    );
    assert!(err["message"]
        .as_str()
        .unwrap()
        .contains("opengate chatgpt login"));
    assert_eq!(
        gw.providers().await["chatgpt"]["status"],
        "subscription_not_configured"
    );

    // Signed out.
    let (gw, _) = offline_gateway(&m, Some(signed_out(Ended::SignedOut))).await;
    let (_, err) = error(gw.chat(true).await).await;
    assert_eq!(err["providers"]["chatgpt"], "subscription_not_authorized");

    // Signed in, but plan usage was not granted at consent.
    let (gw, _) = offline_gateway(
        &m,
        Some(account(3600, "openid profile email offline_access")),
    )
    .await;
    let (_, err) = error(gw.chat(false).await).await;
    assert_eq!(err["providers"]["chatgpt"], "subscription_not_authorized");
    assert!(err["message"].as_str().unwrap().contains("--reconsent"));
    assert_eq!(
        gw.providers().await["chatgpt"]["status"],
        "subscription_not_authorized"
    );

    assert_eq!(
        m.calls(),
        0,
        "no token or inference request without authorization"
    );
}

#[tokio::test]
async fn credentials_written_later_are_picked_up_without_restart() {
    let m = mock().await;
    let path = cg::temp_path();
    let gw = gateway(config_with(dead_port().await, Some(&m), &path, &[])).await;
    assert_eq!(error(gw.chat(false).await).await.0, 503);
    // `opengate chatgpt login` runs in another process and writes the file.
    seed(&path, Some(plan_account()));
    let res = gw.chat(false).await;
    assert_eq!(res.status(), 200);
    assert_eq!(provider(&res), "chatgpt");
}

#[tokio::test]
async fn revoked_authorization_is_a_safe_error_and_sticks() {
    let m = mock().await;
    m.set_inference(Inference::Unauthorized);
    m.set_refresh(Refresh::Revoked);
    let (gw, path) = offline_gateway(&m, Some(plan_account())).await;

    let (status, err) = error(gw.chat(false).await).await;
    assert_eq!(status, 503);
    assert_eq!(err["providers"]["chatgpt"], "subscription_not_authorized");
    assert!(!err.to_string().contains("access-0") && !err.to_string().contains("refresh-0"));
    assert_eq!(m.refreshes(), 1, "one refresh attempt after the 401");
    let a = load(&path).account.unwrap();
    assert!(a.tokens.is_none(), "unusable tokens are cleared");
    assert_eq!(a.ended, Some(Ended::Revoked));
    assert_eq!(a.client_id, cg::CLIENT_ID, "registration kept for re-login");

    // Later requests fail fast without calling OpenAI.
    let before = m.calls();
    assert_eq!(
        error(gw.chat(true).await).await.1["providers"]["chatgpt"],
        "subscription_not_authorized"
    );
    assert_eq!(m.calls(), before);
}

#[tokio::test]
async fn rejected_token_is_refreshed_once_and_the_request_succeeds() {
    let m = mock().await;
    m.set_inference(Inference::RejectInitialToken);
    let (gw, path) = offline_gateway(&m, Some(plan_account())).await;
    let res = gw.chat(false).await;
    assert_eq!(res.status(), 200);
    assert_eq!(m.refreshes(), 1);
    let tokens = load(&path).account.unwrap().tokens.unwrap();
    assert_eq!(tokens.access_token, "access-1");
    assert_eq!(
        tokens.refresh_token, "refresh-1",
        "rotated refresh token persisted"
    );
}

#[tokio::test]
async fn expired_session_is_reported_as_expired() {
    let m = mock().await;
    m.set_refresh(Refresh::Expired);
    let (gw, path) = offline_gateway(&m, Some(account(10, PLAN_SCOPES))).await;
    let (_, err) = error(gw.chat(false).await).await;
    assert_eq!(err["providers"]["chatgpt"], "subscription_expired");
    assert_eq!(load(&path).account.unwrap().ended, Some(Ended::Expired));
    assert_eq!(
        gw.providers().await["chatgpt"]["status"],
        "subscription_expired"
    );
    assert_eq!(m.responses(), 0);
}

#[tokio::test]
async fn temporary_refresh_failure_keeps_a_still_valid_token() {
    let m = mock().await;
    m.set_refresh(Refresh::Unavailable);
    // Inside the refresh margin but not yet expired.
    let (gw, path) = offline_gateway(&m, Some(account(120, PLAN_SCOPES))).await;
    assert_eq!(gw.chat(false).await.status(), 200);
    assert_eq!(m.refreshes(), 1);
    assert!(
        load(&path).account.unwrap().tokens.is_some(),
        "not erased over a 503"
    );
}

// ---- usage limits -----------------------------------------------------------

#[tokio::test]
async fn usage_limit_is_a_clear_error_with_no_other_billing() {
    let m = mock().await;
    m.set_inference(Inference::LimitHttp);
    let (gw, _) = offline_gateway(&m, Some(plan_account())).await;

    let (status, err) = error(gw.chat(false).await).await;
    assert_eq!(status, 429);
    assert_eq!(err["code"], "subscription_limit_reached");
    assert_eq!(err["type"], "rate_limit_error");
    assert_eq!(
        err["providers"],
        json!({ "ollama": "ollama_unavailable", "chatgpt": "subscription_limit_reached" })
    );
    let msg = err["message"].as_str().unwrap();
    assert!(
        msg.contains("chatgpt.com/settings/usage") && msg.contains("no other billing"),
        "{msg}"
    );
    assert_eq!(m.responses(), 1);

    // Paused: the next requests do not touch OpenAI at all.
    let before = m.calls();
    for stream in [false, true] {
        assert_eq!(
            error(gw.chat(stream).await).await.1["code"],
            "subscription_limit_reached"
        );
    }
    assert_eq!(m.calls(), before);
    assert_eq!(
        gw.providers().await["chatgpt"]["status"],
        "subscription_limit_reached"
    );
    // Only ever the OAuth access token: no API key was sent anywhere.
    assert!(m.auth_headers().iter().all(|h| h == "Bearer access-0"));
}

#[tokio::test]
async fn usage_limit_mid_stream_ends_the_stream_without_retrying() {
    let m = mock().await;
    m.set_inference(Inference::LimitMidStream);
    let (gw, _) = offline_gateway(&m, Some(plan_account())).await;
    let res = gw.chat(true).await;
    assert_eq!(res.status(), 200, "the stream had already started");
    let text = res.text().await.unwrap();
    assert!(text.contains("Partial"));
    assert!(
        text.contains("\"code\":\"subscription_limit_reached\""),
        "{text}"
    );
    assert!(
        !text.contains("[DONE]"),
        "a truncated answer never looks complete"
    );
    assert_eq!(m.responses(), 1, "no retry after output started");

    // Non-streaming: partial text is never returned as a completion.
    let (status, err) = error(gw.chat(false).await).await;
    assert_eq!(
        (status, err["code"].as_str()),
        (429, Some("subscription_limit_reached"))
    );
}

#[tokio::test]
async fn openai_api_key_in_the_environment_is_never_used() {
    let m = mock().await;
    let path = cg::temp_path();
    seed(&path, Some(plan_account()));
    let cfg = config_with(
        dead_port().await,
        Some(&m),
        &path,
        &[
            ("OPENAI_API_KEY", "sk-proj-must-never-be-sent"),
            ("OPENAI_API_BILLING_ENABLED", "false"),
        ],
    );
    let gw = gateway(cfg).await;
    m.set_inference(Inference::LimitHttp);
    assert_eq!(gw.chat(false).await.status(), 429);
    m.set_inference(Inference::Ok);
    assert!(!m.auth_headers().is_empty());
    assert!(m.auth_headers().iter().all(|h| !h.contains("sk-")));
}

#[tokio::test]
async fn unsupported_parameters_are_rejected_on_fallback_not_dropped() {
    let m = mock().await;
    let (gw, _) = offline_gateway(&m, Some(plan_account())).await;
    let mut body = hello(false);
    body["temperature"] = json!(0.3);
    let (status, err) = error(gw.send(body.clone()).await).await;
    assert_eq!(status, 503);
    assert_eq!(err["providers"]["chatgpt"], "unsupported_request");
    assert_eq!(err["param"], "temperature");
    assert_eq!(m.calls(), 0);

    // Opt-in: sampling hints are dropped on fallback.
    let path = cg::temp_path();
    seed(&path, Some(plan_account()));
    let gw = gateway(config_with(
        dead_port().await,
        Some(&m),
        &path,
        &[("CHATGPT_DROP_UNSUPPORTED_SAMPLING", "true")],
    ))
    .await;
    assert_eq!(gw.send(body).await.status(), 200);
    assert!(m.bodies()[0].get("temperature").is_none());
}

#[tokio::test]
async fn provider_unavailable_is_reported() {
    let m = mock().await;
    m.set_inference(Inference::Unavailable);
    let (gw, _) = offline_gateway(&m, Some(plan_account())).await;
    let (status, err) = error(gw.chat(false).await).await;
    assert_eq!(status, 503);
    assert_eq!(
        err["providers"]["chatgpt"],
        "subscription_provider_unavailable"
    );
    assert_eq!(
        gw.providers().await["chatgpt"]["status"],
        "subscription_provider_unavailable"
    );
}

// ---- auth, concurrency, cancellation, persistence ---------------------------

#[tokio::test]
async fn invalid_client_key_triggers_neither_wake_nor_cloud() {
    let m = mock().await;
    let pc = pc(Power::Asleep(Some(Duration::ZERO))).await;
    let path = cg::temp_path();
    seed(&path, Some(plan_account()));
    let mut cfg = common::config(&pc, true);
    cfg.chatgpt = config_with(pc.ollama, Some(&m), &path, &[]).chatgpt;
    let gw = gateway(cfg).await;
    let url = format!("{}/v1/chat/completions", gw.base);
    for key in [None, Some("wrong-key")] {
        let mut req = gw.http.post(&url).json(&hello(false));
        if let Some(k) = key {
            req = req.bearer_auth(k);
        }
        assert_eq!(req.send().await.unwrap().status(), 401);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(pc.counters.packets(), 0);
    assert_eq!(m.calls(), 0);
}

#[tokio::test]
async fn concurrent_fallbacks_share_one_wake_and_one_refresh() {
    let m = mock().await;
    let pc = pc(Power::Asleep(None)).await;
    let path = cg::temp_path();
    // Expiring soon: every request needs a fresh token.
    seed(&path, Some(account(10, PLAN_SCOPES)));
    let mut cfg = common::config(&pc, true);
    cfg.max_concurrent_requests = 8;
    cfg.wol.as_mut().unwrap().startup_timeout = Duration::from_millis(300);
    cfg.chatgpt = config_with(pc.ollama, Some(&m), &path, &[]).chatgpt;
    let gw = Arc::new(gateway(cfg).await);

    let tasks: Vec<_> = (0..6)
        .map(|i| {
            let gw = gw.clone();
            tokio::spawn(async move {
                let res = gw.chat(i % 2 == 0).await;
                assert_eq!(res.status(), 200);
                assert_eq!(provider(&res), "chatgpt");
                assert!(res.text().await.unwrap().contains("ChatGPT!"));
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(pc.counters.packets(), 1, "one wake for the burst");
    assert_eq!(m.refreshes(), 1, "refreshes are serialized and shared");
    assert_eq!(m.responses(), 6);
    let headers = m.auth_headers();
    assert!(
        headers.iter().all(|h| h == "Bearer access-1"),
        "{headers:?}"
    );
}

#[tokio::test]
async fn client_cancel_stops_the_chatgpt_stream_and_frees_the_slot() {
    let m = mock().await;
    m.set_inference(Inference::Endless);
    let path = cg::temp_path();
    seed(&path, Some(plan_account()));
    let mut cfg = config_with(dead_port().await, Some(&m), &path, &[]);
    cfg.max_concurrent_requests = 1;
    let gw = gateway(cfg).await;

    let mut res = gw.chat(true).await;
    assert_eq!(provider(&res), "chatgpt");
    assert!(String::from_utf8_lossy(&res.chunk().await.unwrap().unwrap()).contains("tick"));
    drop(res);

    let deadline = Instant::now() + Duration::from_secs(3);
    while !m.hung_up() {
        assert!(
            Instant::now() < deadline,
            "ChatGPT stream was not cancelled"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    m.set_inference(Inference::Ok);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let res = gw.chat(false).await;
        if res.status() == 200 {
            break;
        }
        assert!(Instant::now() < deadline, "slot was never released");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn persisted_credentials_survive_a_restart() {
    let m = mock().await;
    let path = cg::temp_path();
    seed(&path, Some(account(10, PLAN_SCOPES)));
    let ollama = dead_port().await;

    // First process: refreshes and persists the rotated tokens.
    let first = gateway(config_with(ollama, Some(&m), &path, &[])).await;
    assert_eq!(first.chat(false).await.status(), 200);
    assert_eq!(m.refreshes(), 1);
    drop(first);

    // "Restarted" process with the same volume and key: no new sign-in, no
    // second refresh, the rotated token is used.
    let second = gateway(config_with(ollama, Some(&m), &path, &[])).await;
    assert_eq!(second.chat(true).await.status(), 200);
    assert_eq!(m.refreshes(), 1);
    assert_eq!(m.auth_headers().last().unwrap(), "Bearer access-1");

    // A restart with the wrong key cannot read the file and does not touch it.
    let before = std::fs::read(&path).unwrap();
    let mut wrong = config_with(ollama, Some(&m), &path, &[]);
    wrong.chatgpt.as_mut().unwrap().credentials_key =
        "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8="
            .parse()
            .unwrap();
    let third = gateway(wrong).await;
    let (_, err) = error(third.chat(false).await).await;
    assert_eq!(err["providers"]["chatgpt"], "subscription_not_authorized");
    assert!(err["message"]
        .as_str()
        .unwrap()
        .contains("CHATGPT_CREDENTIALS_KEY"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
