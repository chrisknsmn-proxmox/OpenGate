//! The ChatGPT plan fallback provider.
//!
//! Uses the officially documented Sign in with ChatGPT flow for open-source
//! clients: an OAuth access token carrying `chatgpt.tokens.use.direct` is
//! sent as a bearer token to `POST https://api.openai.com/v1/responses` with
//! `store: false, stream: true`. Usage counts against the signed-in user's
//! ChatGPT plan. There is no API key and no API billing path: if the plan
//! cannot serve a request, the request fails with a clear error.
//!
//! Credentials come from the encrypted file written by `opengate chatgpt
//! login`. The gateway re-reads it whenever it changes on disk, so signing in,
//! out or again takes effect without a restart. Refreshes are serialized so
//! the rotating refresh token is never raced.

pub mod cli;
pub mod oauth;
pub(crate) mod responses;
pub mod store;

use std::{
    borrow::Cow,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::http::StatusCode;
use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, OnceCell};
use tracing::{info, warn};

use self::{
    oauth::{Endpoints, RefreshError},
    store::{unix_now, Ended, Stamp, Store, StoreError, StoreFile},
};
use crate::{config::ChatGptConfig, validate::ChatRequest};

/// Refresh this long before the access token expires.
const REFRESH_MARGIN_SECS: u64 = 300;
/// After a temporary refresh failure, wait this long before the next
/// proactive attempt while the current token is still valid.
const REFRESH_RETRY: Duration = Duration::from_secs(30);
/// How long an `auto` model choice is reused before re-reading the catalog.
const MODEL_CACHE: Duration = Duration::from_secs(3600);
/// Health reports the provider as unavailable for this long after a failure.
const UNAVAILABLE_MEMORY: Duration = Duration::from_secs(60);
const MAX_ERROR_BODY: usize = 64 << 10;
pub const USAGE_URL: &str = "https://chatgpt.com/settings/usage";

/// Why the ChatGPT plan could not serve a request. Codes are stable and
/// documented in the README; messages never contain prompts or tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudError {
    /// No credential file: `opengate chatgpt login` has never been run.
    NotConfigured,
    /// Not signed in, plan usage not granted, revoked, or not eligible.
    NotAuthorized(Cow<'static, str>),
    /// The refresh token expired; sign in again.
    Expired,
    /// The ChatGPT plan or app usage limit was reached.
    LimitReached,
    /// OpenAI or the network is unavailable; credentials are kept.
    ProviderUnavailable(Cow<'static, str>),
    /// The request uses something the ChatGPT plan route cannot do.
    Unsupported {
        param: String,
        message: Cow<'static, str>,
    },
}

impl CloudError {
    pub fn code(&self) -> &'static str {
        match self {
            CloudError::NotConfigured => "subscription_not_configured",
            CloudError::NotAuthorized(_) => "subscription_not_authorized",
            CloudError::Expired => "subscription_expired",
            CloudError::LimitReached => "subscription_limit_reached",
            CloudError::ProviderUnavailable(_) => "subscription_provider_unavailable",
            CloudError::Unsupported { .. } => "unsupported_request",
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            CloudError::LimitReached => StatusCode::TOO_MANY_REQUESTS,
            CloudError::Unsupported { .. } => StatusCode::BAD_REQUEST,
            _ => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    pub fn message(&self) -> Cow<'static, str> {
        match self {
            CloudError::NotConfigured => {
                "ChatGPT sign-in has not been set up; run `opengate chatgpt login`".into()
            }
            CloudError::NotAuthorized(why) => why.clone(),
            CloudError::Expired => {
                "the ChatGPT session expired; run `opengate chatgpt login` again".into()
            }
            CloudError::LimitReached => format!(
                "the ChatGPT plan usage limit for OpenGate was reached; no other billing is used. \
                 Review limits at {USAGE_URL}"
            )
            .into(),
            CloudError::ProviderUnavailable(why) => why.clone(),
            CloudError::Unsupported { message, .. } => message.clone(),
        }
    }

    pub fn param(&self) -> Option<&str> {
        match self {
            CloudError::Unsupported { param, .. } if !param.is_empty() => Some(param),
            _ => None,
        }
    }

    fn unavailable(why: impl Into<Cow<'static, str>>) -> Self {
        CloudError::ProviderUnavailable(why.into())
    }

    /// Maps a documented error code (HTTP or `response.failed`) to a state.
    /// `status` is `None` for errors that arrive inside the stream.
    fn from_upstream(
        status: Option<u16>,
        code: Option<&str>,
        param: Option<&str>,
        request_id: Option<&str>,
    ) -> Self {
        let rid = request_id
            .map(|r| format!(", request {r}"))
            .unwrap_or_default();
        let what = match (code, status) {
            (Some(c), _) => c.to_owned(),
            (None, Some(s)) => format!("HTTP {s}"),
            (None, None) => "unknown error".to_owned(),
        };
        match (code, status) {
            (Some("subscription_sharing_usage_limit_exceeded"), _) | (_, Some(429)) => {
                CloudError::LimitReached
            }
            (
                Some(
                    "subscription_sharing_user_not_eligible"
                    | "subscription_sharing_invalid_user"
                    | "subscription_sharing_route_not_supported"
                    | "chatpass_v2_scope_not_authorized"
                    | "chatpass_v2_invalid_authorization_context",
                ),
                _,
            )
            | (None, Some(401 | 403)) => CloudError::NotAuthorized(
                format!("ChatGPT plan usage is not authorized ({what}{rid})").into(),
            ),
            (Some("subscription_sharing_unsupported_capability"), _) | (_, Some(400)) => {
                CloudError::Unsupported {
                    param: param.unwrap_or_default().to_owned(),
                    message: format!("the ChatGPT plan route rejected the request ({what}{rid})")
                        .into(),
                }
            }
            _ => CloudError::unavailable(format!("ChatGPT plan request failed ({what}{rid})")),
        }
    }
}

/// The credential file as last seen on disk.
struct Session {
    stamp: Option<Stamp>,
    file: Result<Option<StoreFile>, StoreError>,
    /// No proactive refresh before this (set after a temporary failure).
    retry_refresh_at: Option<Instant>,
}

#[derive(Clone)]
pub(crate) struct ChatGpt(Arc<Inner>);

struct Inner {
    config: ChatGptConfig,
    http: reqwest::Client,
    store: Store,
    session: AsyncMutex<Session>,
    endpoints: OnceCell<Endpoints>,
    model: Mutex<Option<(String, Instant)>>,
    paused_until: Mutex<Option<Instant>>,
    last_unavailable: Mutex<Option<Instant>>,
}

/// An accepted Responses stream, before any event has been read.
pub(crate) struct Opened {
    pub res: reqwest::Response,
    pub include_usage: bool,
    pub provider: ChatGpt,
}

impl ChatGpt {
    pub fn new(config: ChatGptConfig) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .user_agent(concat!("OpenGate/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("HTTP client configuration is valid");
        let store = Store::new(config.credentials_path.clone(), &config.credentials_key);
        ChatGpt(Arc::new(Inner {
            session: AsyncMutex::new(Session {
                stamp: None,
                file: Ok(None),
                retry_refresh_at: None,
            }),
            endpoints: OnceCell::new(),
            model: Mutex::new(None),
            paused_until: Mutex::new(None),
            last_unavailable: Mutex::new(None),
            http,
            store,
            config,
        }))
    }

    pub fn config(&self) -> &ChatGptConfig {
        &self.0.config
    }

    /// Opens a streamed Responses request for `req`. Nothing has been sent
    /// to the client when this returns, so any error here is still a clean
    /// HTTP error response.
    pub async fn open(&self, req: &ChatRequest) -> Result<Opened, CloudError> {
        if let Some(until) = *self.0.paused_until.lock().expect("pause lock") {
            if Instant::now() < until {
                return Err(CloudError::LimitReached);
            }
        }
        let (mut body, include_usage) =
            responses::translate(req, self.0.config.drop_unsupported_sampling)?;
        body["model"] = self.model().await?.into();
        let url = self
            .0
            .config
            .api_base
            .join("responses")
            .expect("valid path");
        let timeout = self.0.config.request_timeout;
        let res = self
            .send(|http, token| {
                http.post(url.clone())
                    .bearer_auth(token)
                    .header("accept", "text/event-stream")
                    .timeout(timeout)
                    .json(&body)
            })
            .await?;
        Ok(Opened {
            res,
            include_usage,
            provider: self.clone(),
        })
    }

    /// Coarse status for `/health/providers`. Never makes an inference call
    /// (that would spend plan usage) and never contacts OpenAI.
    pub async fn health(&self) -> &'static str {
        if self.paused() {
            return CloudError::LimitReached.code();
        }
        let mut session = self.0.session.lock().await;
        self.reload_if_changed(&mut session);
        let status = match &session.file {
            Err(_) => "subscription_not_authorized",
            Ok(None) => CloudError::NotConfigured.code(),
            Ok(Some(file)) => match &file.account {
                None => "subscription_not_authorized",
                Some(a) => match (&a.tokens, a.ended) {
                    (None, Some(Ended::Expired)) => CloudError::Expired.code(),
                    (None, _) => "subscription_not_authorized",
                    (Some(_), _) if !a.plan_usage_granted() => "subscription_not_authorized",
                    (Some(t), _) if t.refresh_expires_at() <= unix_now() => {
                        CloudError::Expired.code()
                    }
                    (Some(_), _) => "authorized",
                },
            },
        };
        let recently_failed = self
            .0
            .last_unavailable
            .lock()
            .expect("lock")
            .is_some_and(|t| t.elapsed() < UNAVAILABLE_MEMORY);
        if status == "authorized" && recently_failed {
            return "subscription_provider_unavailable";
        }
        status
    }

    fn paused(&self) -> bool {
        self.0
            .paused_until
            .lock()
            .expect("pause lock")
            .is_some_and(|until| Instant::now() < until)
    }

    /// Records side effects of a failure: pause after a usage limit, as the
    /// docs ask ("pause new requests that use the user's ChatGPT plan").
    pub(crate) fn note_failure(&self, err: &CloudError) {
        match err {
            CloudError::LimitReached => {
                let until = Instant::now() + self.0.config.limit_pause;
                *self.0.paused_until.lock().expect("pause lock") = Some(until);
                warn!(
                    pause_s = self.0.config.limit_pause.as_secs(),
                    "ChatGPT plan usage limit reached; pausing fallback requests"
                );
            }
            CloudError::ProviderUnavailable(_) => {
                *self.0.last_unavailable.lock().expect("lock") = Some(Instant::now());
            }
            _ => {}
        }
    }

    async fn endpoints(&self) -> Result<&Endpoints, CloudError> {
        self.0
            .endpoints
            .get_or_try_init(|| oauth::discover(&self.0.http, &self.0.config.issuer))
            .await
            .map_err(|e| {
                warn!(error = %e, "ChatGPT OpenID discovery failed");
                CloudError::unavailable("could not reach the ChatGPT sign-in service")
            })
    }

    async fn model(&self) -> Result<String, CloudError> {
        if let Some(model) = &self.0.config.model {
            return Ok(model.clone());
        }
        if let Some((model, at)) = &*self.0.model.lock().expect("model lock") {
            if at.elapsed() < MODEL_CACHE {
                return Ok(model.clone());
            }
        }
        let url = self.0.config.api_base.join("models").expect("valid path");
        let res = self
            .send(|http, token| http.get(url.clone()).bearer_auth(token))
            .await?;
        let catalog: Value = res
            .json()
            .await
            .map_err(|_| CloudError::unavailable("ChatGPT model catalog was not valid JSON"))?;
        let model = catalog["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["visibility"] == "list")
            .find_map(|m| m["slug"].as_str())
            .ok_or_else(|| {
                CloudError::unavailable("the ChatGPT account lists no models; set CHATGPT_MODEL")
            })?
            .to_owned();
        info!(model = %model, "CHATGPT_MODEL=auto selected the account's first listed model");
        *self.0.model.lock().expect("model lock") = Some((model.clone(), Instant::now()));
        Ok(model)
    }

    /// Sends an authorized request. On 401, refreshes once and retries once
    /// (safe: nothing has been streamed yet). Errors are mapped and noted.
    async fn send(
        &self,
        build: impl Fn(&reqwest::Client, &str) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, CloudError> {
        let result = async {
            let token = self.access_token(None).await?;
            let res = build(&self.0.http, &token)
                .send()
                .await
                .map_err(from_reqwest)?;
            if res.status() != reqwest::StatusCode::UNAUTHORIZED {
                return check(res).await;
            }
            info!("ChatGPT access token was rejected; refreshing once");
            drop(res);
            let token = self.access_token(Some(&token)).await?;
            let res = build(&self.0.http, &token)
                .send()
                .await
                .map_err(from_reqwest)?;
            if res.status() == reqwest::StatusCode::UNAUTHORIZED {
                let err = check(res).await.err().unwrap_or(CloudError::NotAuthorized(
                    "ChatGPT plan access was not accepted".into(),
                ));
                // A freshly refreshed token was refused: the session is no
                // longer authorized. Stop using it; the user must sign in.
                self.end_session(Ended::Revoked).await;
                return Err(err);
            }
            check(res).await
        }
        .await;
        if let Err(e) = &result {
            self.note_failure(e);
        }
        result
    }

    fn reload_if_changed(&self, session: &mut Session) {
        let stamp = self.0.store.stamp();
        if stamp.is_some() && stamp == session.stamp {
            return;
        }
        session.file = match self.0.store.load() {
            Ok(Some((file, s))) => {
                session.stamp = Some(s);
                Ok(Some(file))
            }
            Ok(None) => {
                session.stamp = None;
                Ok(None)
            }
            Err(e) => {
                session.stamp = stamp;
                warn!(error = %e, "cannot load ChatGPT credentials");
                Err(e)
            }
        };
        if session.file.is_ok() {
            // A different account may now be signed in.
            *self.0.model.lock().expect("model lock") = None;
        }
    }

    /// A usable access token, refreshing first if it is close to expiry or
    /// if `rejected` (the token a request was just refused with) is still
    /// the current one. Serialized: concurrent callers wait for one refresh.
    async fn access_token(&self, rejected: Option<&str>) -> Result<String, CloudError> {
        let mut session = self.0.session.lock().await;
        self.reload_if_changed(&mut session);
        let file = match &session.file {
            Ok(Some(file)) => file,
            Ok(None) => return Err(CloudError::NotConfigured),
            Err(_) => {
                return Err(CloudError::NotAuthorized(
                    "ChatGPT credentials cannot be read; check CHATGPT_CREDENTIALS_KEY".into(),
                ))
            }
        };
        let account = file.account.as_ref().ok_or(CloudError::NotAuthorized(
            "not signed in to ChatGPT; run `opengate chatgpt login`".into(),
        ))?;
        if !account.plan_usage_granted() {
            return Err(CloudError::NotAuthorized(
                "ChatGPT plan usage was not granted at sign-in; run \
                 `opengate chatgpt login --reconsent`"
                    .into(),
            ));
        }
        let Some(tokens) = &account.tokens else {
            return Err(match account.ended {
                Some(Ended::Expired) => CloudError::Expired,
                Some(Ended::Revoked) => CloudError::NotAuthorized(
                    "the ChatGPT session was revoked or disconnected; run \
                     `opengate chatgpt login`"
                        .into(),
                ),
                _ => CloudError::NotAuthorized(
                    "signed out of ChatGPT; run `opengate chatgpt login`".into(),
                ),
            });
        };
        let now = unix_now();
        let forced = rejected.is_some_and(|r| r == tokens.access_token);
        let due = tokens.expires_at() <= now + REFRESH_MARGIN_SECS
            && tokens.earliest_refresh_at.is_none_or(|t| now >= t);
        if rejected.is_some() && !forced {
            // Another request already refreshed; use its token.
            return Ok(tokens.access_token.clone());
        }
        let backing_off = session
            .retry_refresh_at
            .is_some_and(|t| Instant::now() < t && tokens.expires_at() > now + 10);
        if !forced && (!due || backing_off) {
            return Ok(tokens.access_token.clone());
        }

        let endpoints = self.endpoints().await?.clone();
        let account = account.clone();
        let old = tokens.clone();
        info!(forced, "refreshing ChatGPT access token");
        match oauth::refresh(&self.0.http, &endpoints.token, &account, &old.refresh_token).await {
            Ok(fresh) => {
                let token = fresh.access_token.clone();
                let mut updated = file.clone();
                session.retry_refresh_at = None;
                if let Some(a) = updated.account.as_mut() {
                    a.tokens = Some(fresh);
                    a.ended = None;
                }
                self.persist(&mut session, updated);
                Ok(token)
            }
            Err(RefreshError::Unusable { expired }) => {
                let ended = if expired {
                    Ended::Expired
                } else {
                    Ended::Revoked
                };
                let mut updated = file.clone();
                if let Some(a) = updated.account.as_mut() {
                    a.tokens = None;
                    a.ended = Some(ended);
                }
                self.persist(&mut session, updated);
                warn!(
                    ?ended,
                    "ChatGPT refresh token is no longer usable; sign-in required"
                );
                Err(if expired {
                    CloudError::Expired
                } else {
                    CloudError::NotAuthorized(
                        "the ChatGPT session was revoked or disconnected; run \
                         `opengate chatgpt login`"
                            .into(),
                    )
                })
            }
            Err(RefreshError::InvalidClient) => Err(CloudError::NotAuthorized(
                "the ChatGPT client registration was rejected; run \
                 `opengate chatgpt login --new-account`"
                    .into(),
            )),
            Err(RefreshError::Unavailable) if !forced && old.expires_at() > now + 10 => {
                // Still valid for now; try again shortly, not on every request.
                session.retry_refresh_at = Some(Instant::now() + REFRESH_RETRY);
                Ok(old.access_token)
            }
            Err(RefreshError::Unavailable) => Err(CloudError::unavailable(
                "could not renew the ChatGPT session; will retry",
            )),
        }
    }

    /// Saves `updated` unless the file changed on disk since it was loaded
    /// (e.g. `opengate chatgpt login` or `logout` ran meanwhile), in which
    /// case the on-disk version wins and is reloaded.
    fn persist(&self, session: &mut Session, updated: StoreFile) {
        if self.0.store.stamp() != session.stamp {
            warn!("ChatGPT credentials changed on disk during a refresh; using the new file");
            session.stamp = None;
            self.reload_if_changed(session);
            return;
        }
        match self.0.store.save(&updated) {
            Ok(stamp) => {
                session.stamp = Some(stamp);
                session.file = Ok(Some(updated));
            }
            Err(e) => {
                // Keep using the new tokens in memory; the old refresh token
                // on disk is now spent, so a restart will need a new sign-in.
                warn!(error = %e, "could not save refreshed ChatGPT credentials");
                session.file = Ok(Some(updated));
            }
        }
    }

    async fn end_session(&self, ended: Ended) {
        let mut session = self.0.session.lock().await;
        self.reload_if_changed(&mut session);
        if let Ok(Some(file)) = &session.file {
            let mut updated = file.clone();
            if let Some(a) = updated.account.as_mut() {
                a.tokens = None;
                a.ended = Some(ended);
            }
            self.persist(&mut session, updated);
        }
    }
}

fn from_reqwest(e: reqwest::Error) -> CloudError {
    warn!(error = oauth::transport(&e), "ChatGPT plan request failed");
    CloudError::unavailable(format!(
        "could not reach the ChatGPT plan service ({})",
        oauth::transport(&e)
    ))
}

/// Passes a success through; maps an error response to a [`CloudError`],
/// keeping its code, param and request ID (never its body).
async fn check(res: reqwest::Response) -> Result<reqwest::Response, CloudError> {
    let status = res.status();
    if status.is_success() {
        return Ok(res);
    }
    let request_id = res
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.chars()
                .filter(char::is_ascii_graphic)
                .take(80)
                .collect::<String>()
        });
    let bytes = res.bytes().await.unwrap_or_default();
    let body: Value =
        serde_json::from_slice(&bytes[..bytes.len().min(MAX_ERROR_BODY)]).unwrap_or(Value::Null);
    let code = body["error"]["code"]
        .as_str()
        .or(body["error"].as_str())
        .map(short);
    let param = body["error"]["param"].as_str().map(short);
    warn!(
        status = status.as_u16(),
        code = code.as_deref().unwrap_or("none"),
        request_id = request_id.as_deref().unwrap_or("none"),
        "ChatGPT plan request was rejected"
    );
    Err(CloudError::from_upstream(
        Some(status.as_u16()),
        code.as_deref(),
        param.as_deref(),
        request_id.as_deref(),
    ))
}

fn short(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_graphic())
        .take(80)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_errors_map_to_states() {
        let m = |status, code| CloudError::from_upstream(status, code, None, None).code();
        assert_eq!(
            m(Some(429), Some("subscription_sharing_usage_limit_exceeded")),
            "subscription_limit_reached"
        );
        assert_eq!(
            m(None, Some("subscription_sharing_usage_limit_exceeded")),
            "subscription_limit_reached"
        );
        assert_eq!(m(Some(429), None), "subscription_limit_reached");
        assert_eq!(
            m(Some(403), Some("subscription_sharing_user_not_eligible")),
            "subscription_not_authorized"
        );
        assert_eq!(
            m(Some(401), Some("subscription_sharing_invalid_user")),
            "subscription_not_authorized"
        );
        assert_eq!(
            m(Some(403), Some("chatpass_v2_scope_not_authorized")),
            "subscription_not_authorized"
        );
        assert_eq!(m(Some(403), None), "subscription_not_authorized");
        assert_eq!(
            m(
                Some(400),
                Some("subscription_sharing_unsupported_capability")
            ),
            "unsupported_request"
        );
        assert_eq!(
            m(Some(503), Some("subscription_sharing_usage_unavailable")),
            "subscription_provider_unavailable"
        );
        assert_eq!(
            m(Some(503), Some("subscription_sharing_user_unavailable")),
            "subscription_provider_unavailable"
        );
        assert_eq!(m(Some(503), None), "subscription_provider_unavailable");
        assert_eq!(m(Some(500), None), "subscription_provider_unavailable");
        assert_eq!(
            m(None, Some("server_error")),
            "subscription_provider_unavailable"
        );
    }

    #[test]
    fn limit_message_points_to_usage_settings_and_rules_out_other_billing() {
        let msg = CloudError::LimitReached.message();
        assert!(msg.contains(USAGE_URL) && msg.contains("no other billing"));
        assert_eq!(CloudError::LimitReached.status(), 429);
    }
}
