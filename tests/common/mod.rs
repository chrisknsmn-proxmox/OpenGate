//! Shared harness for the Wake-on-LAN tests: a mock Windows PC whose Ollama
//! refuses connections until it receives the right magic packet over UDP.
#![allow(dead_code)]

use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use axum::{
    body::Body,
    extract::State,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use opengate::Config;
use serde_json::{json, Value};
use tokio::net::{TcpSocket, UdpSocket};

pub const KEY: &str = "wol-test-key-0123456789abcdef0123456";
pub const VM: &str = "opengate/local";
/// A locally administered test MAC; the real one lives only in `.env`.
pub const MAC: &str = "02:00:5e:10:00:01";
const MAC_BYTES: [u8; 6] = [0x02, 0x00, 0x5e, 0x10, 0x00, 0x01];

pub async fn serve(router: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    addr
}

pub fn magic_packet() -> Vec<u8> {
    let mut p = vec![0xFF; 6];
    for _ in 0..16 {
        p.extend_from_slice(&MAC_BYTES);
    }
    p
}

#[derive(Clone, Default)]
pub struct Counters {
    /// Valid magic packets received.
    pub packets: Arc<AtomicUsize>,
    pub tags: Arc<AtomicUsize>,
    pub chats: Arc<AtomicUsize>,
}

impl Counters {
    pub fn packets(&self) -> usize {
        self.packets.load(Ordering::SeqCst)
    }
    pub fn tags(&self) -> usize {
        self.tags.load(Ordering::SeqCst)
    }
    pub fn chats(&self) -> usize {
        self.chats.load(Ordering::SeqCst)
    }
}

pub struct Pc {
    pub ollama: SocketAddr,
    /// UDP address the PC's NIC listens on for magic packets.
    pub nic: SocketAddr,
    pub counters: Counters,
}

pub enum Power {
    On,
    /// Asleep; boots this long after the first magic packet, or never.
    Asleep(Option<Duration>),
    /// Asleep; the first packet is lost, and it boots this long after the second.
    LosesFirstPacket(Duration),
}

fn ollama_mock(counters: Counters) -> Router {
    Router::new()
        .route(
            "/api/tags",
            get(|State(c): State<Counters>| async move {
                c.tags.fetch_add(1, Ordering::SeqCst);
                Json(json!({ "models": [{ "name": "llama3.1:8b" }] }))
            }),
        )
        .route(
            "/v1/chat/completions",
            post(
                |State(c): State<Counters>, Json(body): Json<Value>| async move {
                    c.chats.fetch_add(1, Ordering::SeqCst);
                    chat_reply(body["stream"] == true)
                },
            ),
        )
        .with_state(counters)
}

fn chat_reply(stream: bool) -> Response {
    if stream {
        let chunk = json!({
            "id": "c", "object": "chat.completion.chunk", "created": 1, "model": "llama3.1:8b",
            "choices": [{ "index": 0, "delta": { "content": "Hello!" }, "finish_reason": null }],
        });
        return (
            [("content-type", "text/event-stream")],
            Body::from(format!("data: {chunk}\n\ndata: [DONE]\n\n")),
        )
            .into_response();
    }
    Json(json!({
        "id": "c", "object": "chat.completion", "created": 1, "model": "llama3.1:8b",
        "choices": [{ "index": 0, "message": { "role": "assistant", "content": "Hello!" }, "finish_reason": "stop" }],
    }))
    .into_response()
}

pub async fn pc(power: Power) -> Pc {
    let counters = Counters::default();
    // Bound but not listening: connections are refused, like a host that is off.
    let socket = TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let ollama = socket.local_addr().unwrap();
    let router = ollama_mock(counters.clone());
    if let Power::On = power {
        // Listen before returning, so the very first request finds it up.
        let listener = socket.listen(128).unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        return Pc {
            ollama,
            nic: nic(counters.packets.clone(), None, 0).await,
            counters,
        };
    }
    let boot = move |delay: Duration| async move {
        tokio::time::sleep(delay).await;
        let listener = socket.listen(128).unwrap();
        axum::serve(listener, router).await.unwrap();
    };
    let (delay, boots_on_packet) = match power {
        Power::On => unreachable!(),
        Power::Asleep(None) => (Duration::ZERO, 0),
        Power::Asleep(Some(delay)) => (delay, 1),
        Power::LosesFirstPacket(delay) => (delay, 2),
    };
    let boot: Boot = Box::new(move || {
        tokio::spawn(boot(delay));
    });
    Pc {
        ollama,
        nic: nic(counters.packets.clone(), Some(boot), boots_on_packet).await,
        counters,
    }
}

type Boot = Box<dyn FnOnce() + Send>;

/// The PC's network card: counts valid magic packets and boots on the
/// `boots_on_packet`-th one (0 = never).
async fn nic(
    packets: Arc<AtomicUsize>,
    mut boot: Option<Boot>,
    boots_on_packet: usize,
) -> SocketAddr {
    let nic = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = nic.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        loop {
            let (len, _) = nic.recv_from(&mut buf).await.unwrap();
            if buf[..len] != magic_packet()[..] {
                continue;
            }
            let n = packets.fetch_add(1, Ordering::SeqCst) + 1;
            if n == boots_on_packet {
                if let Some(boot) = boot.take() {
                    boot();
                }
            }
        }
    });
    addr
}

/// Config pointing at `pc`, with WOL broadcasting straight to its NIC and
/// test-sized timings. `wol: false` leaves WOL disabled.
pub fn config(pc: &Pc, wol: bool) -> Config {
    let mut config = Config::from_lookup(|name| match name {
        "OPENGATE_API_KEY" => Some(KEY.into()),
        "OLLAMA_BASE_URL" => Some(format!("http://{}", pc.ollama)),
        "OLLAMA_MODEL" => Some("llama3.1:8b".into()),
        "WOL_ENABLED" => Some(wol.to_string()),
        "WOL_MAC_ADDRESS" => Some(MAC.into()),
        "WOL_BROADCAST_ADDRESS" => Some(pc.nic.ip().to_string()),
        "WOL_PORT" => Some(pc.nic.port().to_string()),
        _ => None,
    })
    .unwrap();
    if let Some(w) = config.wol.as_mut() {
        w.startup_timeout = Duration::from_secs(3);
        w.poll_interval = Duration::from_millis(50);
        w.retry_cooldown = Duration::from_millis(400);
    }
    config
}

pub fn hello(stream: bool) -> Value {
    json!({ "model": VM, "messages": [{ "role": "user", "content": "Say hello." }], "stream": stream })
}
