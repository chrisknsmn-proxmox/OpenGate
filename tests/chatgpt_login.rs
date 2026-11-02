//! The Sign in with ChatGPT login flow against a mock authorization server:
//! dynamic registration, PKCE, ID-token signature/nonce/audience checks,
//! plan-scope detection and revocation.

mod common;

use std::collections::HashMap;

use common::chatgpt::{mock, sign_id_token, ChatGptMock, LoginExpect, CLIENT_ID, PLAN_SCOPES};
use opengate::chatgpt::oauth::{self, BeginOptions, LoginError, Pending};
use serde_json::json;

async fn start(
    m: &ChatGptMock,
) -> (
    reqwest::Client,
    oauth::Endpoints,
    Pending,
    HashMap<String, String>,
) {
    let http = oauth::http_client(std::time::Duration::from_secs(3));
    let endpoints = oauth::discover(&http, &m.base().parse().unwrap())
        .await
        .unwrap();
    let pending = oauth::begin(
        &endpoints,
        BeginOptions {
            host_id: "urn:uuid:test-host",
            callback_port: 1455,
            existing: None,
            reconsent: false,
        },
    );
    let q: HashMap<String, String> = pending.url.query_pairs().into_owned().collect();
    (http, endpoints, pending, q)
}

fn expect(m: &ChatGptMock, q: &HashMap<String, String>, aud: &str, scope: &str) {
    *m.0.login.lock().unwrap() = Some(LoginExpect {
        nonce: q["nonce"].clone(),
        code_challenge: q["code_challenge"].clone(),
        id_token_aud: aud.into(),
        scope: scope.into(),
        sub: "user-sub-1".into(),
    });
}

fn callback(q: &HashMap<String, String>) -> String {
    format!(
        "http://127.0.0.1:1455/auth/callback?code=auth-code-1&scope=openid&state={}&client_id={CLIENT_ID}",
        q["state"]
    )
}

#[tokio::test]
async fn full_registration_and_sign_in() {
    let m = mock().await;
    let (http, endpoints, pending, q) = start(&m).await;
    assert_eq!(
        pending.url.as_str().split('?').next().unwrap(),
        format!("{}authorize", m.base())
    );
    assert_eq!(q["client_id"], "dynamic_agent_client");
    assert_eq!(q["ext_agent_host_id"], "urn:uuid:test-host");
    expect(&m, &q, CLIENT_ID, PLAN_SCOPES);

    let cb = oauth::parse_callback(&pending, &callback(&q)).unwrap();
    let account = oauth::complete(&http, &endpoints, &pending, cb)
        .await
        .unwrap();
    assert_eq!(
        account.client_id, CLIENT_ID,
        "the issued ID, not dynamic_agent_client"
    );
    assert_eq!(account.subject, "user-sub-1");
    assert_eq!(account.email.as_deref(), Some("me@example.com"));
    assert!(account.plan_usage_granted());
    let t = account.tokens.as_ref().unwrap();
    assert_eq!(
        (t.access_token.as_str(), t.refresh_token.as_str()),
        ("access-login", "refresh-login")
    );
    assert_eq!(t.expires_in, 3600);

    // Signing out revokes the refresh token with the issued client ID.
    oauth::revoke(&http, &endpoints, &account.client_id, &t.refresh_token)
        .await
        .unwrap();
    let forms = m.0.seen.forms.lock().unwrap().clone();
    let revoke = forms.last().unwrap();
    assert_eq!(revoke["token"], "refresh-login");
    assert_eq!(revoke["token_type_hint"], "refresh_token");
    assert_eq!(revoke["client_id"], CLIENT_ID);
}

#[tokio::test]
async fn declined_plan_scope_signs_in_without_plan_usage() {
    let m = mock().await;
    let (http, endpoints, pending, q) = start(&m).await;
    expect(&m, &q, CLIENT_ID, "openid profile email offline_access");
    let cb = oauth::parse_callback(&pending, &callback(&q)).unwrap();
    let account = oauth::complete(&http, &endpoints, &pending, cb)
        .await
        .unwrap();
    assert!(
        !account.plan_usage_granted(),
        "a valid ID token alone is not plan permission"
    );
}

#[tokio::test]
async fn consent_declined_in_browser() {
    let m = mock().await;
    let (_, _, pending, q) = start(&m).await;
    let target = format!("/auth/callback?error=access_denied&state={}", q["state"]);
    assert_eq!(
        oauth::parse_callback(&pending, &target).err(),
        Some(LoginError::Declined)
    );
}

#[tokio::test]
async fn id_token_for_another_audience_is_rejected() {
    let m = mock().await;
    let (http, endpoints, pending, q) = start(&m).await;
    expect(&m, &q, "oaiapp_someone_else", PLAN_SCOPES);
    let cb = oauth::parse_callback(&pending, &callback(&q)).unwrap();
    let err = oauth::complete(&http, &endpoints, &pending, cb)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("ID token is invalid"), "{err}");
}

#[tokio::test]
async fn id_token_checks_nonce_and_signature() {
    let m = mock().await;
    let (http, endpoints, _, _) = start(&m).await;
    let iss = m.base().trim_end_matches('/').to_owned();
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 600;
    let good = sign_id_token(
        &json!({ "iss": iss, "aud": CLIENT_ID, "sub": "s", "nonce": "n1", "exp": exp }),
    );
    assert!(
        oauth::validate_id_token(&http, &endpoints, &good, CLIENT_ID, "n1")
            .await
            .is_ok()
    );
    let err = oauth::validate_id_token(&http, &endpoints, &good, CLIENT_ID, "n2")
        .await
        .unwrap_err();
    assert!(err.contains("nonce"));

    // Tampered payload: the signature no longer matches.
    let mut parts: Vec<String> = good.split('.').map(str::to_owned).collect();
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let mut claims: serde_json::Value =
        serde_json::from_slice(&b64.decode(&parts[1]).unwrap()).unwrap();
    claims["sub"] = "attacker".into();
    parts[1] = b64.encode(claims.to_string());
    assert!(
        oauth::validate_id_token(&http, &endpoints, &parts.join("."), CLIENT_ID, "n1")
            .await
            .is_err()
    );

    // Wrong issuer.
    let other = sign_id_token(
        &json!({ "iss": "https://evil", "aud": CLIENT_ID, "sub": "s", "nonce": "n1", "exp": exp }),
    );
    assert!(
        oauth::validate_id_token(&http, &endpoints, &other, CLIENT_ID, "n1")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn discovery_must_match_the_issuer() {
    let m = mock().await;
    let http = oauth::http_client(std::time::Duration::from_secs(3));
    // Same server, but asked to act as a different issuer.
    let url: reqwest::Url = m.base().replace("127.0.0.1", "localhost").parse().unwrap();
    assert!(oauth::discover(&http, &url)
        .await
        .unwrap_err()
        .contains("issuer"));
}
