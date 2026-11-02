use std::{
    io::{IsTerminal, Read, Write},
    net::{SocketAddr, TcpStream},
    process::ExitCode,
    time::Duration,
};

use opengate::Config;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    // Used by the container HEALTHCHECK so the runtime image needs no curl.
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        return healthcheck();
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(std::io::stdout().is_terminal())
        .init();

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(e) => {
            error!("invalid configuration: {e}");
            return ExitCode::FAILURE;
        }
    };
    if config.log_prompt_content {
        warn!("LOG_PROMPT_CONTENT=true: prompt messages will be written to the log");
    }
    info!(?config, "starting");

    let addr = config.listen_addr();
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            error!(%addr, "cannot listen: {e}");
            return ExitCode::FAILURE;
        }
    };
    info!(%addr, "OpenGate listening");

    if let Err(e) = axum::serve(listener, opengate::app(config))
        .with_graceful_shutdown(shutdown())
        .await
    {
        error!("server error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn shutdown() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("can install SIGTERM handler");
    tokio::select! {
        _ = ctrl_c => {}
        _ = term.recv() => {}
    }
    info!("shutting down");
}

fn healthcheck() -> ExitCode {
    let port = std::env::var("OPENGATE_PORT")
        .ok()
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or(8000);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let timeout = Duration::from_secs(3);
    let check = || -> std::io::Result<bool> {
        let mut conn = TcpStream::connect_timeout(&addr, timeout)?;
        conn.set_read_timeout(Some(timeout))?;
        conn.write_all(
            b"GET /health/live HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )?;
        let mut head = [0u8; 12];
        conn.read_exact(&mut head)?;
        Ok(head.starts_with(b"HTTP/1.1 200"))
    };
    match check() {
        Ok(true) => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}
