//! Sign in with ChatGPT for open-source clients, as documented at
//! <https://developers.openai.com/siwc/token-sharing-open-source/sign-in>.
//!
//! - First sign-in registers this app with `client_id=dynamic_agent_client`,
//!   `agent_name_hint` and the host's `ext_agent_host_id`; the callback
//!   returns the issued `client_id` (`oaiapp_…`), which is saved and reused.
//! - Authorization code flow with PKCE (S256), fresh `state` and `nonce` per
//!   attempt, and an HTTP loopback redirect on `127.0.0.1`.
//! - The ID token is verified against OpenAI's JWKS (issuer, audience =
//!   issued client ID, expiry, nonce). ChatGPT plan usage additionally
//!   requires the granted `chatgpt.tokens.use.direct` scope.
//! - No client secret and no API key exist anywhere in this flow.

use std::{collections::HashMap, fmt, time::Duration};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use jsonwebtoken::{jwk::JwkSet, Algorithm, DecodingKey, Validation};
use reqwest::Url;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::warn;

use super::store::{random_bytes, unix_now, Account, Tokens};
use crate::config::CHATGPT_RESOURCE;

pub const AGENT_NAME: &str = "OpenGate";
pub const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
pub const CALLBACK_PATH: &str = "/auth/callback";
pub const DEFAULT_CALLBACK_PORT: u16 = 1455;
pub const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const MAX_ERROR_BODY: usize = 16 << 10;

/// Endpoints from `/.well-known/openid-configuration`.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub issuer: String,
    pub authorization: Url,
    pub token: Url,
    pub revocation: Url,
    pub jwks: Url,
}

pub fn http_client(connect_timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(Duration::from_secs(30))
        // Endpoints are fixed; never follow a redirect with a token in hand.
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .user_agent(concat!("OpenGate/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("HTTP client configuration is valid")
}

pub async fn discover(http: &reqwest::Client, issuer: &Url) -> Result<Endpoints, String> {
    let url = issuer
        .join(".well-known/openid-configuration")
        .map_err(|_| "invalid issuer URL")?;
    let doc: Value = http
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| format!("OpenID discovery failed: {}", transport(&e)))?
        .json()
        .await
        .map_err(|_| "OpenID discovery returned invalid JSON".to_string())?;
    let expected = issuer.as_str().trim_end_matches('/');
    if doc["issuer"].as_str() != Some(expected) {
        return Err("OpenID discovery returned an unexpected issuer".into());
    }
    let url = |key: &str| -> Result<Url, String> {
        doc[key]
            .as_str()
            .and_then(|u| u.parse().ok())
            .ok_or_else(|| format!("OpenID discovery is missing {key}"))
    };
    Ok(Endpoints {
        issuer: expected.to_owned(),
        authorization: url("authorization_endpoint")?,
        token: url("token_endpoint")?,
        revocation: url("revocation_endpoint")?,
        jwks: url("jwks_uri")?,
    })
}

/// One authorization attempt. Holds the PKCE verifier and must not outlive
/// the login command.
pub struct Pending {
    pub url: Url,
    pub redirect_uri: String,
    state: String,
    nonce: String,
    verifier: String,
    /// `None` for a new registration; else the saved issued client ID.
    client_id: Option<String>,
    /// The account being reauthorized, whose identity must not change.
    expected_subject: Option<String>,
}

impl fmt::Debug for Pending {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pending")
            .field("redirect_uri", &self.redirect_uri)
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

pub struct BeginOptions<'a> {
    pub host_id: &'a str,
    pub callback_port: u16,
    /// Reauthorize this saved registration instead of registering anew.
    pub existing: Option<&'a Account>,
    /// Ask for consent again, e.g. to grant plan usage after declining it.
    pub reconsent: bool,
}

fn random_token() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes::<32>())
}

pub fn code_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub fn begin(endpoints: &Endpoints, opts: BeginOptions<'_>) -> Pending {
    let redirect_uri = format!("http://127.0.0.1:{}{CALLBACK_PATH}", opts.callback_port);
    let (state, nonce, verifier) = (random_token(), random_token(), random_token());
    let mut url = endpoints.authorization.clone();
    {
        let mut q = url.query_pairs_mut();
        match opts.existing {
            None => {
                q.append_pair("client_id", DYNAMIC_CLIENT);
                q.append_pair("agent_name_hint", AGENT_NAME);
            }
            Some(account) => {
                q.append_pair("client_id", &account.client_id);
                // The URL is shown in a terminal, so the ID token is not sent
                // as id_token_hint; the email hint pre-selects the account.
                if let Some(email) = &account.email {
                    q.append_pair("login_hint", email);
                }
            }
        }
        q.append_pair("ext_agent_host_id", opts.host_id);
        q.append_pair("response_type", "code");
        q.append_pair("redirect_uri", &redirect_uri);
        q.append_pair("scope", SCOPES);
        q.append_pair("resource", CHATGPT_RESOURCE);
        q.append_pair("state", &state);
        q.append_pair("nonce", &nonce);
        q.append_pair("code_challenge_method", "S256");
        q.append_pair("code_challenge", &code_challenge(&verifier));
        if opts.reconsent {
            q.append_pair("prompt", "consent");
        }
    }
    Pending {
        url,
        redirect_uri,
        state,
        nonce,
        verifier,
        client_id: opts.existing.map(|a| a.client_id.clone()),
        expected_subject: opts.existing.map(|a| a.subject.clone()),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum LoginError {
    /// The user declined consent (`error=access_denied`).
    Declined,
    Failed(String),
}

impl fmt::Display for LoginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoginError::Declined => f.write_str("sign-in was declined in the browser"),
            LoginError::Failed(e) => f.write_str(e),
        }
    }
}

fn failed(e: impl Into<String>) -> LoginError {
    LoginError::Failed(e.into())
}

pub struct Callback {
    code: String,
    client_id: String,
    scope: Option<String>,
}

/// Parses the redirect the browser was sent to: a full URL, or the request
/// target `/auth/callback?…` as received by the loopback listener.
pub fn parse_callback(pending: &Pending, target: &str) -> Result<Callback, LoginError> {
    let base: Url = pending.redirect_uri.parse().expect("valid redirect URI");
    let url = base
        .join(target.trim())
        .map_err(|_| failed("not a callback URL"))?;
    if url.path() != CALLBACK_PATH {
        return Err(failed(format!("callback path must be {CALLBACK_PATH}")));
    }
    let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
    // State first: nothing from an unrelated or forged redirect is trusted.
    if q.get("state").map(String::as_str) != Some(pending.state.as_str()) {
        return Err(failed(
            "callback state does not match this sign-in attempt; start again",
        ));
    }
    match q.get("error").map(String::as_str) {
        None => {}
        Some("access_denied") => return Err(LoginError::Declined),
        Some(code) => return Err(failed(format!("authorization failed: {}", short(code)))),
    }
    let code = q
        .get("code")
        .filter(|c| !c.is_empty())
        .ok_or_else(|| failed("callback has no authorization code"))?
        .clone();
    let returned = q.get("client_id").filter(|c| !c.is_empty());
    let client_id = match (&pending.client_id, returned) {
        (None, Some(issued)) if issued != DYNAMIC_CLIENT => issued.clone(),
        (None, _) => {
            return Err(failed(
                "registration incomplete: the callback did not include an issued client_id",
            ))
        }
        (Some(saved), None) => saved.clone(),
        (Some(saved), Some(returned)) if returned == saved => saved.clone(),
        (Some(_), Some(_)) => {
            return Err(failed(
                "callback returned a different client_id than the saved registration",
            ))
        }
    };
    Ok(Callback {
        code,
        client_id,
        scope: q.get("scope").cloned(),
    })
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    token_type: Option<String>,
    expires_in: Option<u64>,
    scope: Option<String>,
    earliest_refresh_at: Option<Value>,
}

impl TokenResponse {
    fn tokens(&self, refresh_token: String, now: u64) -> Tokens {
        Tokens {
            access_token: self.access_token.clone(),
            refresh_token,
            token_type: self.token_type.clone().unwrap_or_else(|| "Bearer".into()),
            expires_in: self.expires_in.unwrap_or(3600),
            saved_at: now,
            refresh_issued_at: now,
            earliest_refresh_at: self.earliest_refresh_at.as_ref().and_then(unix_seconds),
        }
    }
}

/// `earliest_refresh_at` as Unix seconds; accepts a number or a numeric string.
fn unix_seconds(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.parse().ok())
}

/// Exchanges the code, validates the ID token, and returns the account
/// record to save. `plan_usage_granted()` on the result tells whether the
/// user allowed ChatGPT plan inference.
pub async fn complete(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    pending: &Pending,
    callback: Callback,
) -> Result<Account, LoginError> {
    let res = http
        .post(endpoints.token.clone())
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", callback.client_id.as_str()),
            ("code", callback.code.as_str()),
            ("code_verifier", pending.verifier.as_str()),
            ("redirect_uri", pending.redirect_uri.as_str()),
            ("resource", CHATGPT_RESOURCE),
        ])
        .send()
        .await
        .map_err(|e| failed(format!("token exchange failed: {}", transport(&e))))?;
    if !res.status().is_success() {
        let status = res.status().as_u16();
        let code = oauth_error_code(res).await;
        return Err(failed(format!(
            "token exchange was rejected (HTTP {status}{}); start the sign-in again",
            code.map(|c| format!(", {c}")).unwrap_or_default()
        )));
    }
    let token: TokenResponse = res
        .json()
        .await
        .map_err(|_| failed("token endpoint returned an invalid response"))?;
    let id_token = token
        .id_token
        .clone()
        .ok_or_else(|| failed("token response has no ID token"))?;
    let claims = validate_id_token(
        http,
        endpoints,
        &id_token,
        &callback.client_id,
        &pending.nonce,
    )
    .await
    .map_err(failed)?;
    let subject = claims["sub"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| failed("ID token has no subject"))?
        .to_owned();
    if let Some(expected) = &pending.expected_subject {
        if &subject != expected {
            return Err(failed(
                "signed in as a different ChatGPT account than the saved one; \
                 use --new-account to replace it",
            ));
        }
    }
    let scopes: Vec<String> = token
        .scope
        .as_deref()
        .or(callback.scope.as_deref())
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let refresh_token = token.refresh_token.clone().ok_or_else(|| {
        failed("token response has no refresh token (offline_access not granted)")
    })?;
    Ok(Account {
        issuer: endpoints.issuer.clone(),
        subject,
        email: claims["email"].as_str().map(str::to_owned),
        client_id: callback.client_id,
        id_token: Some(id_token),
        tokens: Some(token.tokens(refresh_token, unix_now())),
        ended: None,
        scopes,
    })
}

/// Verifies signature (RS256, OpenAI JWKS), issuer, audience, expiry and nonce.
pub async fn validate_id_token(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    id_token: &str,
    client_id: &str,
    nonce: &str,
) -> Result<Value, String> {
    let header =
        jsonwebtoken::decode_header(id_token).map_err(|_| "ID token is malformed".to_string())?;
    let jwks: JwkSet = http
        .get(endpoints.jwks.clone())
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| format!("could not fetch OpenAI signing keys: {}", transport(&e)))?
        .json()
        .await
        .map_err(|_| "OpenAI signing keys are not a valid JWKS".to_string())?;
    let jwk = match &header.kid {
        Some(kid) => jwks.find(kid),
        None if jwks.keys.len() == 1 => jwks.keys.first(),
        None => None,
    }
    .ok_or("ID token is signed with an unknown key")?;
    let key = DecodingKey::from_jwk(jwk).map_err(|_| "unusable signing key".to_string())?;
    let mut v = Validation::new(Algorithm::RS256);
    v.set_issuer(&[&endpoints.issuer]);
    v.set_audience(&[client_id]);
    v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    let data = jsonwebtoken::decode::<Value>(id_token, &key, &v)
        .map_err(|e| format!("ID token is invalid: {:?}", e.kind()))?;
    if data.claims["nonce"].as_str() != Some(nonce) {
        return Err("ID token nonce does not match this sign-in attempt".into());
    }
    Ok(data.claims)
}

#[derive(Debug, PartialEq, Eq)]
pub enum RefreshError {
    /// The refresh token can no longer be used; sign in again.
    /// `expired` distinguishes expiry from revocation/reuse.
    Unusable { expired: bool },
    /// `invalid_client`: the registration itself was rejected.
    InvalidClient,
    /// Network failure or 5xx; credentials are kept.
    Unavailable,
}

/// `grant_type=refresh_token`, keeping the original grant (no `scope`).
pub async fn refresh(
    http: &reqwest::Client,
    token_url: &Url,
    account: &Account,
    refresh_token: &str,
) -> Result<Tokens, RefreshError> {
    let res = http
        .post(token_url.clone())
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", account.client_id.as_str()),
            ("refresh_token", refresh_token),
            ("resource", CHATGPT_RESOURCE),
        ])
        .send()
        .await
        .map_err(|e| {
            warn!(error = transport(&e), "ChatGPT token refresh failed");
            RefreshError::Unavailable
        })?;
    let status = res.status();
    if status.is_success() {
        let token: TokenResponse = res.json().await.map_err(|_| RefreshError::Unavailable)?;
        // The response must carry the rotated refresh token; reusing the old
        // one would fail as `refresh_token_reused`.
        let rotated = token
            .refresh_token
            .clone()
            .ok_or(RefreshError::Unavailable)?;
        return Ok(token.tokens(rotated, unix_now()));
    }
    let code = oauth_error_code(res).await;
    warn!(
        status = status.as_u16(),
        code = code.as_deref().unwrap_or("none"),
        "ChatGPT token refresh was rejected"
    );
    match code.as_deref() {
        Some("token_expired" | "refresh_token_expired") => {
            Err(RefreshError::Unusable { expired: true })
        }
        Some(
            "invalid_grant"
            | "invalid_refresh_token"
            | "refresh_token_invalidated"
            | "refresh_token_reused",
        ) => Err(RefreshError::Unusable { expired: false }),
        Some("invalid_client") => Err(RefreshError::InvalidClient),
        _ if status.is_server_error() => Err(RefreshError::Unavailable),
        // An unrecognised 4xx: keep the credentials rather than discard a
        // session over a code we do not understand.
        _ => Err(RefreshError::Unavailable),
    }
}

/// Ends the renewable session. `Ok` on HTTP 200 (also for an already
/// invalid token). Retries network failures and 5xx with backoff.
pub async fn revoke(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    client_id: &str,
    refresh_token: &str,
) -> Result<(), String> {
    let mut last = String::new();
    for attempt in 0..3u32 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(500 * 2u64.pow(attempt))).await;
        }
        let res = http
            .post(endpoints.revocation.clone())
            .form(&[
                ("token", refresh_token),
                ("token_type_hint", "refresh_token"),
                ("client_id", client_id),
            ])
            .send()
            .await;
        match res {
            Ok(r) if r.status().is_success() => return Ok(()),
            Ok(r) if r.status().is_server_error() => last = format!("HTTP {}", r.status().as_u16()),
            Ok(r) => {
                return Err(format!(
                    "revocation was rejected (HTTP {})",
                    r.status().as_u16()
                ))
            }
            Err(e) => last = transport(&e).to_owned(),
        }
    }
    Err(format!("revocation could not be confirmed ({last})"))
}

/// The machine-readable code from an OAuth error body, in either the
/// standard `{"error":"…"}` or the `{"error":{"code":"…"}}` shape.
async fn oauth_error_code(res: reqwest::Response) -> Option<String> {
    let bytes = res.bytes().await.ok()?;
    let body: Value = serde_json::from_slice(&bytes[..bytes.len().min(MAX_ERROR_BODY)]).ok()?;
    body["error"]
        .as_str()
        .or(body["error"]["code"].as_str())
        .or(body["code"].as_str())
        .map(short)
}

/// Codes are short identifiers; anything else is cut so a hostile or broken
/// server cannot flood the terminal or logs.
fn short(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_graphic())
        .take(64)
        .collect()
}

pub(crate) fn transport(e: &reqwest::Error) -> &'static str {
    if e.is_connect() {
        "could not connect"
    } else if e.is_timeout() {
        "timed out"
    } else if e.is_status() {
        "unexpected HTTP status"
    } else {
        "connection failed"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chatgpt::store::PLAN_SCOPE;

    fn endpoints() -> Endpoints {
        let u = |p: &str| format!("https://auth.example/{p}").parse().unwrap();
        Endpoints {
            issuer: "https://auth.example".into(),
            authorization: u("authorize"),
            token: u("token"),
            revocation: u("revoke"),
            jwks: u("jwks"),
        }
    }

    fn query(p: &Pending) -> HashMap<String, String> {
        p.url.query_pairs().into_owned().collect()
    }

    fn account() -> Account {
        Account {
            issuer: "https://auth.example".into(),
            subject: "sub-1".into(),
            email: Some("me@example.com".into()),
            client_id: "oaiapp_saved".into(),
            id_token: Some("id".into()),
            scopes: vec![PLAN_SCOPE.into()],
            tokens: None,
            ended: None,
        }
    }

    #[test]
    fn pkce_matches_rfc7636_example() {
        // RFC 7636 Appendix B.
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn new_registration_url() {
        let p = begin(
            &endpoints(),
            BeginOptions {
                host_id: "urn:uuid:h",
                callback_port: 1455,
                existing: None,
                reconsent: false,
            },
        );
        let q = query(&p);
        assert_eq!(p.url.path(), "/authorize");
        assert_eq!(q["client_id"], "dynamic_agent_client");
        assert_eq!(q["agent_name_hint"], "OpenGate");
        assert_eq!(q["ext_agent_host_id"], "urn:uuid:h");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:1455/auth/callback");
        assert_eq!(q["scope"], SCOPES);
        assert!(q["scope"].contains("chatgpt.tokens.use.direct"));
        assert_eq!(q["resource"], "https://api.openai.com/v1");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["code_challenge"], code_challenge(&p.verifier));
        assert_eq!(q["state"], p.state);
        assert_eq!(q["nonce"], p.nonce);
        assert!(!q.contains_key("prompt") && !q.contains_key("login_hint"));
        // Fresh values per attempt.
        let p2 = begin(
            &endpoints(),
            BeginOptions {
                host_id: "urn:uuid:h",
                callback_port: 1455,
                existing: None,
                reconsent: false,
            },
        );
        assert_ne!(p.state, p2.state);
        assert_ne!(p.nonce, p2.nonce);
        assert_ne!(p.verifier, p2.verifier);
    }

    #[test]
    fn reauthorization_url() {
        let a = account();
        let p = begin(
            &endpoints(),
            BeginOptions {
                host_id: "urn:uuid:h",
                callback_port: 54321,
                existing: Some(&a),
                reconsent: true,
            },
        );
        let q = query(&p);
        assert_eq!(q["client_id"], "oaiapp_saved");
        assert!(
            !q.contains_key("agent_name_hint"),
            "only on first registration"
        );
        assert!(!q.contains_key("id_token_hint"), "never printed in a URL");
        assert_eq!(q["login_hint"], "me@example.com");
        assert_eq!(q["prompt"], "consent");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:54321/auth/callback");
    }

    fn pending(existing: Option<&Account>) -> Pending {
        begin(
            &endpoints(),
            BeginOptions {
                host_id: "h",
                callback_port: 1455,
                existing,
                reconsent: false,
            },
        )
    }

    #[test]
    fn callback_validation() {
        let p = pending(None);
        let ok = format!(
            "/auth/callback?code=c1&state={}&client_id=oaiapp_new&scope=openid",
            p.state
        );
        let cb = parse_callback(&p, &ok).unwrap();
        assert_eq!(
            (cb.code.as_str(), cb.client_id.as_str()),
            ("c1", "oaiapp_new")
        );
        // Full URL pasted from the browser works too.
        let full = format!("http://127.0.0.1:1455{ok}");
        assert!(parse_callback(&p, &full).is_ok());

        let bad = |target: String| parse_callback(&p, &target).err().unwrap();
        assert!(
            matches!(bad("/auth/callback?code=c&state=wrong&client_id=x".into()), LoginError::Failed(e) if e.contains("state"))
        );
        assert!(matches!(
            bad("/auth/callback?code=c&client_id=x".into()),
            LoginError::Failed(_)
        ));
        assert_eq!(
            bad(format!(
                "/auth/callback?error=access_denied&state={}",
                p.state
            )),
            LoginError::Declined
        );
        assert!(
            matches!(bad(format!("/callback?code=c&state={}&client_id=x", p.state)), LoginError::Failed(e) if e.contains("path"))
        );
        assert!(
            matches!(bad(format!("/auth/callback?code=c&state={}", p.state)), LoginError::Failed(e) if e.contains("client_id"))
        );
        assert!(matches!(
            bad(format!(
                "/auth/callback?code=c&state={}&client_id=dynamic_agent_client",
                p.state
            )),
            LoginError::Failed(_)
        ));
        assert!(
            matches!(bad(format!("/auth/callback?state={}&client_id=x", p.state)), LoginError::Failed(e) if e.contains("code"))
        );
    }

    #[test]
    fn reauthorization_keeps_the_saved_client() {
        let a = account();
        let p = pending(Some(&a));
        let cb = parse_callback(&p, &format!("/auth/callback?code=c&state={}", p.state)).unwrap();
        assert_eq!(cb.client_id, "oaiapp_saved");
        let cb = parse_callback(
            &p,
            &format!(
                "/auth/callback?code=c&state={}&client_id=oaiapp_saved",
                p.state
            ),
        )
        .unwrap();
        assert_eq!(cb.client_id, "oaiapp_saved");
        assert!(parse_callback(
            &p,
            &format!(
                "/auth/callback?code=c&state={}&client_id=oaiapp_other",
                p.state
            )
        )
        .is_err());
    }
}
