//! `opengate chatgpt …`: sign-in and credential management.
//!
//! These commands run interactively inside the OpenGate container (for
//! example `docker exec -it <container> opengate chatgpt login`), so the
//! sign-in flow is never exposed on the gateway's HTTP port. The browser can
//! be on another machine: forward the loopback callback port over SSH, or
//! paste the final redirect URL into the terminal (README: "Sign in").

use std::{net::SocketAddr, process::ExitCode, time::Duration};

use serde_json::json;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};

use super::{
    oauth::{
        self, BeginOptions, Callback, LoginError, Pending, CALLBACK_PATH, DEFAULT_CALLBACK_PORT,
    },
    responses,
    store::{unix_now, Ended, Store, StoreFile, PLAN_SCOPE},
    ChatGpt, USAGE_URL,
};
use crate::{config::ChatGptConfig, validate};

const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);

const HELP: &str = "\
Usage: opengate chatgpt <command>

Commands:
  init      Create the credential file and this host's ID (optional; login does it too)
  login     Sign in with ChatGPT and allow OpenGate to use your ChatGPT plan
            --port N        loopback callback port in the redirect URI (default 1455)
            --listen ADDR   where the callback listener binds (default 127.0.0.1:PORT;
                            use 0.0.0.0:PORT inside a container reached over an SSH tunnel)
            --reconsent     ask for consent again (e.g. to grant ChatGPT plan usage)
            --new-account   register a different ChatGPT account, replacing the saved one
  status    Show the saved account and session state (never prints tokens)
  logout    Revoke the session at OpenAI and delete the tokens
  test      Send one small request through the ChatGPT plan (uses a little plan usage)

Needs CHATGPT_CREDENTIALS_KEY (and CHATGPT_CREDENTIALS_PATH if not the default).";

pub async fn run(args: &[String]) -> ExitCode {
    let Some(cmd) = args.first().map(String::as_str) else {
        println!("{HELP}");
        return ExitCode::FAILURE;
    };
    if matches!(cmd, "help" | "--help" | "-h") {
        println!("{HELP}");
        return ExitCode::SUCCESS;
    }
    let cfg = match ChatGptConfig::from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = match cmd {
        "init" => init(&cfg),
        "login" => match LoginOptions::parse(&args[1..]) {
            Ok(opts) => login(&cfg, opts).await,
            Err(e) => Err(e),
        },
        "status" => status(&cfg),
        "logout" => logout(&cfg).await,
        "test" => test(&cfg).await,
        other => Err(format!("unknown command {other:?}\n\n{HELP}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn store(cfg: &ChatGptConfig) -> Store {
    Store::new(cfg.credentials_path.clone(), &cfg.credentials_key)
}

fn init(cfg: &ChatGptConfig) -> Result<(), String> {
    let store = store(cfg);
    let file = store.load_or_new().map_err(|e| e.to_string())?;
    if store.stamp().is_none() {
        store.save(&file).map_err(|e| e.to_string())?;
        println!("Created {}", store.path().display());
    }
    println!("Host ID: {}", file.ext_agent_host_id);
    Ok(())
}

pub struct LoginOptions {
    pub port: u16,
    pub listen: SocketAddr,
    pub reconsent: bool,
    pub new_account: bool,
}

impl LoginOptions {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut port = DEFAULT_CALLBACK_PORT;
        let mut listen = None;
        let (mut reconsent, mut new_account) = (false, false);
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--port" => {
                    port = it
                        .next()
                        .and_then(|p| p.parse().ok())
                        .filter(|p| *p != 0)
                        .ok_or("--port needs a port number")?
                }
                "--listen" => {
                    listen = Some(
                        it.next()
                            .and_then(|a| a.parse::<SocketAddr>().ok())
                            .ok_or("--listen needs IP:PORT")?,
                    )
                }
                "--reconsent" => reconsent = true,
                "--new-account" => new_account = true,
                other => return Err(format!("unknown login option {other:?}")),
            }
        }
        Ok(LoginOptions {
            port,
            listen: listen.unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], port))),
            reconsent,
            new_account,
        })
    }
}

async fn login(cfg: &ChatGptConfig, opts: LoginOptions) -> Result<(), String> {
    let store = store(cfg);
    // Refuses to continue (and overwrite) if the file exists but cannot be
    // decrypted, e.g. because CHATGPT_CREDENTIALS_KEY changed.
    let mut file = store.load_or_new().map_err(|e| e.to_string())?;
    // The docs ask for the host ID to be persisted before the first sign-in.
    if store.stamp().is_none() {
        store.save(&file).map_err(|e| e.to_string())?;
    }
    let http = oauth::http_client(cfg.connect_timeout);
    let endpoints = oauth::discover(&http, &cfg.issuer).await?;
    let existing = file.account.as_ref().filter(|_| !opts.new_account);
    let pending = oauth::begin(
        &endpoints,
        BeginOptions {
            host_id: &file.ext_agent_host_id,
            callback_port: opts.port,
            existing,
            reconsent: opts.reconsent,
        },
    );

    // Start the listener before the browser is sent anywhere.
    let listener = TcpListener::bind(opts.listen)
        .await
        .map_err(|e| format!("cannot listen on {}: {e}", opts.listen))?;
    println!(
        "\nSign in with ChatGPT to let OpenGate use your ChatGPT plan when the local model\n\
         is unavailable. OpenGate never uses an OpenAI API key or API billing.\n\n\
         1. Open this URL in a browser and approve access:\n\n{}\n\n\
         2. The browser is then sent to {}.\n   \
         - If that port is forwarded here (ssh -L {}:<this-host>:{}), sign-in finishes by itself.\n   \
         - Otherwise the page will not load: copy the full URL from the address bar\n     \
           and paste it here, then press Enter.\n\n\
         Waiting up to {} minutes (listening on {})…",
        pending.url,
        pending.redirect_uri,
        opts.port,
        opts.listen.port(),
        LOGIN_TIMEOUT.as_secs() / 60,
        opts.listen,
    );

    let callback = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_callback(&listener, &pending))
        .await
        .map_err(|_| "timed out waiting for the browser; run login again".to_string())?;
    drop(listener);
    let callback = match callback {
        Ok(cb) => cb,
        Err(LoginError::Declined) => {
            return Err(
                "access was declined in the browser; nothing was changed. Run \
                 `opengate chatgpt login --reconsent` to try again."
                    .into(),
            )
        }
        Err(e) => return Err(e.to_string()),
    };
    let account = oauth::complete(&http, &endpoints, &pending, callback)
        .await
        .map_err(|e| e.to_string())?;
    let granted = account.plan_usage_granted();
    let summary = (account.email.clone(), account.client_id.clone());
    // Replaces this registration's credentials only after full validation.
    file.account = Some(account);
    store.save(&file).map_err(|e| e.to_string())?;

    println!(
        "\nSigned in{} (client {}).",
        summary.0.map(|e| format!(" as {e}")).unwrap_or_default(),
        summary.1
    );
    if granted {
        println!(
            "You're using your ChatGPT plan: when the local model is unavailable, OpenGate\n\
             sends requests to your ChatGPT plan. Manage usage and limits at {USAGE_URL}\n\n\
             Next: `opengate chatgpt test` sends one small request to confirm it works."
        );
        Ok(())
    } else {
        Err(format!(
            "signed in, but ChatGPT plan usage ({PLAN_SCOPE}) was not granted, so the \
             fallback stays disabled. Run `opengate chatgpt login --reconsent` and allow it."
        ))
    }
}

/// The first callback from either the loopback listener or a URL pasted on
/// stdin. Unrelated requests to the listener (e.g. `/favicon.ico`) get 404.
async fn wait_for_callback(
    listener: &TcpListener,
    pending: &Pending,
) -> Result<Callback, LoginError> {
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut stdin_open = true;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((conn, _)) = accepted else { continue };
                if let Some(result) = handle_connection(conn, pending).await {
                    return result;
                }
            }
            line = stdin.next_line(), if stdin_open => match line {
                Ok(Some(line)) if !line.trim().is_empty() => {
                    match oauth::parse_callback(pending, &line) {
                        Err(LoginError::Failed(e)) if e.contains("not a callback") || e.contains("path") => {
                            eprintln!("That is not the callback URL ({e}); paste the full address-bar URL.");
                        }
                        result => return result,
                    }
                }
                Ok(Some(_)) => {}
                _ => stdin_open = false,
            },
        }
    }
}

async fn handle_connection(
    mut conn: TcpStream,
    pending: &Pending,
) -> Option<Result<Callback, LoginError>> {
    let mut buf = Vec::with_capacity(2048);
    let read = tokio::time::timeout(Duration::from_secs(10), async {
        let mut chunk = [0u8; 1024];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 8192 {
            let n = conn.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        Some(())
    })
    .await;
    if !matches!(read, Ok(Some(()))) {
        return None;
    }
    let head = String::from_utf8_lossy(&buf);
    let target = head
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("GET "))
        .and_then(|l| l.split(' ').next())
        .unwrap_or_default()
        .to_owned();
    if target.split('?').next() != Some(CALLBACK_PATH) {
        respond(&mut conn, 404, "Not found").await;
        return None;
    }
    let result = oauth::parse_callback(pending, &target);
    let page = match &result {
        Ok(_) => "Signed in. You can close this tab and return to the terminal.",
        Err(LoginError::Declined) => "Access was declined. You can close this tab.",
        Err(_) => "Sign-in failed. Return to the terminal for details.",
    };
    respond(&mut conn, if result.is_ok() { 200 } else { 400 }, page).await;
    Some(result)
}

async fn respond(conn: &mut TcpStream, status: u16, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>OpenGate</title>\
         <p style=\"font:16px system-ui;margin:3em\">{message}</p>"
    );
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let res = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = conn.write_all(res.as_bytes()).await;
    let _ = conn.shutdown().await;
}

fn status(cfg: &ChatGptConfig) -> Result<(), String> {
    let store = store(cfg);
    println!("Credential file: {}", store.path().display());
    let Some((file, _)) = store.load().map_err(|e| e.to_string())? else {
        println!("State: subscription_not_configured (run `opengate chatgpt login`)");
        return Ok(());
    };
    print_status(&file);
    Ok(())
}

fn print_status(file: &StoreFile) {
    println!("Host ID: {}", file.ext_agent_host_id);
    let Some(a) = &file.account else {
        println!("State: subscription_not_authorized (not signed in)");
        return;
    };
    println!("Account: {}", a.email.as_deref().unwrap_or("(no email)"));
    println!("Client ID: {}", a.client_id);
    println!("Granted scopes: {}", a.scopes.join(" "));
    println!(
        "ChatGPT plan usage: {}",
        if a.plan_usage_granted() {
            "granted"
        } else {
            "NOT granted"
        }
    );
    let now = unix_now();
    let state = match (&a.tokens, a.ended) {
        (None, Some(Ended::Expired)) => "subscription_expired (sign in again)",
        (None, Some(Ended::Revoked)) => {
            "subscription_not_authorized (revoked or disconnected; sign in again)"
        }
        (None, _) => "subscription_not_authorized (signed out)",
        (Some(_), _) if !a.plan_usage_granted() => {
            "subscription_not_authorized (plan usage not granted)"
        }
        (Some(t), _) if t.refresh_expires_at() <= now => "subscription_expired (sign in again)",
        (Some(_), _) => "authorized",
    };
    println!("State: {state}");
    if let Some(t) = &a.tokens {
        let mins = |at: u64| (at as i64 - now as i64) / 60;
        println!(
            "Access token: expires in {} min (renewed automatically)",
            mins(t.expires_at())
        );
        println!(
            "Session: renewable for {} more days without use",
            mins(t.refresh_expires_at()) / (60 * 24)
        );
    }
    println!("Usage and limits: {USAGE_URL}");
}

async fn logout(cfg: &ChatGptConfig) -> Result<(), String> {
    let store = store(cfg);
    let Some((mut file, _)) = store.load().map_err(|e| e.to_string())? else {
        println!("Not signed in.");
        return Ok(());
    };
    let Some(account) = file.account.as_mut() else {
        println!("Not signed in.");
        return Ok(());
    };
    let revoked = match &account.tokens {
        None => None,
        Some(tokens) => {
            let http = oauth::http_client(cfg.connect_timeout);
            Some(match oauth::discover(&http, &cfg.issuer).await {
                Ok(endpoints) => {
                    oauth::revoke(&http, &endpoints, &account.client_id, &tokens.refresh_token)
                        .await
                }
                Err(e) => Err(e),
            })
        }
    };
    // Keep the client ID and host ID for a later sign-in; drop every token.
    account.tokens = None;
    account.id_token = None;
    account.ended = Some(Ended::SignedOut);
    store.save(&file).map_err(|e| e.to_string())?;
    match revoked {
        None | Some(Ok(())) => {
            println!(
                "Signed out. The session was revoked at OpenAI and local tokens were deleted."
            );
            Ok(())
        }
        Some(Err(e)) => Err(format!(
            "local tokens were deleted, but remote revocation was not confirmed ({e}). \
             To be sure, disconnect OpenGate in ChatGPT Settings ({USAGE_URL})."
        )),
    }
}

/// One real request through the same code path the gateway uses. This is
/// the only proof that subscription-backed inference works for this account.
async fn test(cfg: &ChatGptConfig) -> Result<(), String> {
    let provider = ChatGpt::new(cfg.clone());
    let req = validate::chat_request(
        json!({
            "model": "opengate-cli-test",
            "messages": [{ "role": "user", "content": "Reply with exactly: OpenGate ChatGPT plan OK" }],
        }),
        "opengate-cli-test",
    )
    .map_err(|e| format!("{e:?}"))?;
    println!("Sending one request to the ChatGPT plan…");
    let failed = |e: super::CloudError| format!("{}: {}", e.code(), e.message());
    let opened = provider.open(&req).await.map_err(failed)?;
    let completion = responses::collect(opened, "chatgpt".into())
        .await
        .map_err(failed)?;
    println!(
        "OK: response {} finished with {:?}\nReply: {}",
        completion["id"].as_str().unwrap_or("?"),
        completion["choices"][0]["finish_reason"]
            .as_str()
            .unwrap_or("?"),
        completion["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default(),
    );
    if let Some(u) = completion.get("usage") {
        println!(
            "Tokens: {} in, {} out",
            u["prompt_tokens"], u["completion_tokens"]
        );
    }
    Ok(())
}
