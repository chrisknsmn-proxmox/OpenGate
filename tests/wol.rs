//! Wake-on-LAN: a real OpenGate in front of a mock PC that is on, asleep, or
//! never wakes. Packets are sent to the mock's UDP "NIC" on loopback.

mod common;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use common::{config, hello, pc, serve, Pc, Power, KEY, MAC};
use opengate::{Config, WolRelay, WolRelayConfig, WolTarget};
use serde_json::Value;
use tokio::net::UdpSocket;

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
    async fn chat(&self, stream: bool) -> reqwest::Response {
        self.http
            .post(format!("{}/v1/chat/completions", self.base))
            .bearer_auth(KEY)
            .json(&hello(stream))
            .send()
            .await
            .unwrap()
    }
}

async fn assert_ok(res: reqwest::Response, stream: bool) {
    assert_eq!(res.status(), 200);
    let body = res.text().await.unwrap();
    if stream {
        assert!(
            body.contains("Hello!") && body.ends_with("data: [DONE]\n\n"),
            "{body}"
        );
    } else {
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["choices"][0]["message"]["content"], "Hello!");
    }
}

async fn assert_unavailable(res: reqwest::Response) {
    assert_eq!(res.status(), 503);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["code"], "upstream_unavailable");
}

#[tokio::test]
async fn online_ollama_is_used_without_waking() {
    let pc = pc(Power::On).await;
    let gw = gateway(config(&pc, true)).await;

    assert_ok(gw.chat(false).await, false).await;
    assert_ok(gw.chat(true).await, true).await;

    assert_eq!(pc.counters.packets(), 0, "no wake packet when Ollama is up");
    assert_eq!(pc.counters.tags(), 2, "one connectivity check per request");
    assert_eq!(pc.counters.chats(), 2);
}

#[tokio::test]
async fn offline_ollama_is_woken_then_the_request_resumes() {
    let pc = pc(Power::Asleep(Some(Duration::from_millis(300)))).await;
    let gw = gateway(config(&pc, true)).await;

    let start = Instant::now();
    assert_ok(gw.chat(false).await, false).await;
    assert!(
        start.elapsed() >= Duration::from_millis(300),
        "waited for boot"
    );
    assert_eq!(pc.counters.packets(), 1);
    assert_eq!(pc.counters.chats(), 1);

    // Now awake: later requests go straight through.
    assert_ok(gw.chat(true).await, true).await;
    assert_eq!(pc.counters.packets(), 1);
}

#[tokio::test]
async fn offline_streaming_request_waits_for_wake() {
    let pc = pc(Power::Asleep(Some(Duration::from_millis(200)))).await;
    let gw = gateway(config(&pc, true)).await;
    assert_ok(gw.chat(true).await, true).await;
    assert_eq!(pc.counters.packets(), 1);
}

#[tokio::test]
async fn startup_timeout_is_503_then_retried_after_cooldown() {
    let pc = pc(Power::Asleep(None)).await;
    let mut cfg = config(&pc, true);
    let wol = cfg.wol.as_mut().unwrap();
    wol.startup_timeout = Duration::from_millis(400);
    wol.retry_cooldown = Duration::from_millis(600);
    let gw = gateway(cfg).await;

    let start = Instant::now();
    assert_unavailable(gw.chat(false).await).await;
    assert!(start.elapsed() >= Duration::from_millis(400));
    assert_eq!(pc.counters.packets(), 1);

    // Within the cooldown: fail fast, no new packet.
    let start = Instant::now();
    assert_unavailable(gw.chat(true).await).await;
    assert!(
        start.elapsed() < Duration::from_millis(350),
        "should fail fast"
    );
    assert_eq!(pc.counters.packets(), 1);

    // After the cooldown, a new request makes a fresh attempt.
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_unavailable(gw.chat(false).await).await;
    assert_eq!(pc.counters.packets(), 2);
}

#[tokio::test]
async fn failed_attempt_is_retried_by_a_later_request() {
    let pc = pc(Power::LosesFirstPacket(Duration::from_millis(100))).await;
    let mut cfg = config(&pc, true);
    let wol = cfg.wol.as_mut().unwrap();
    wol.startup_timeout = Duration::from_millis(300);
    wol.retry_cooldown = Duration::from_millis(200);
    let gw = gateway(cfg).await;

    assert_unavailable(gw.chat(false).await).await;
    assert_eq!(pc.counters.packets(), 1);

    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_ok(gw.chat(false).await, false).await;
    assert_eq!(pc.counters.packets(), 2);
}

#[tokio::test]
async fn concurrent_requests_share_one_wake() {
    let pc = pc(Power::Asleep(Some(Duration::from_millis(400)))).await;
    let mut cfg = config(&pc, true);
    cfg.max_concurrent_requests = 8;
    let gw = Arc::new(gateway(cfg).await);

    let requests: Vec<_> = (0..6)
        .map(|i| {
            let gw = gw.clone();
            tokio::spawn(async move {
                let stream = i % 2 == 0;
                assert_ok(gw.chat(stream).await, stream).await;
            })
        })
        .collect();
    for r in requests {
        r.await.unwrap();
    }
    assert_eq!(pc.counters.packets(), 1, "one packet for the whole burst");
    assert_eq!(pc.counters.chats(), 6, "every request was forwarded");
}

#[tokio::test]
async fn cancelled_request_frees_its_slot_and_the_wake_continues() {
    let pc = pc(Power::Asleep(Some(Duration::from_millis(800)))).await;
    let mut cfg = config(&pc, true);
    cfg.max_concurrent_requests = 1;
    let gw = gateway(cfg).await;

    // A client that gives up while the PC is booting.
    let impatient = gw
        .http
        .post(format!("{}/v1/chat/completions", gw.base))
        .bearer_auth(KEY)
        .json(&hello(false))
        .timeout(Duration::from_millis(250))
        .send()
        .await;
    assert!(impatient.unwrap_err().is_timeout());
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Its concurrency slot was released (else this would be gateway_overloaded),
    // and it joins the wake already under way instead of sending another packet.
    assert_ok(gw.chat(false).await, false).await;
    assert_eq!(pc.counters.packets(), 1);
    assert_eq!(
        pc.counters.chats(),
        1,
        "the cancelled request never reached Ollama"
    );
}

#[tokio::test]
async fn wol_disabled_never_sends_packets() {
    let pc = pc(Power::Asleep(Some(Duration::ZERO))).await;
    let cfg = config(&pc, false);
    assert!(cfg.wol.is_none());
    let gw = gateway(cfg).await;

    let start = Instant::now();
    assert_unavailable(gw.chat(false).await).await;
    assert!(start.elapsed() < Duration::from_secs(1), "no waiting");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(pc.counters.packets(), 0);
    assert_eq!(pc.counters.tags(), 0);
}

#[tokio::test]
async fn only_authenticated_inference_requests_wake() {
    let pc = pc(Power::Asleep(Some(Duration::ZERO))).await;
    let gw = gateway(config(&pc, true)).await;
    let chat = format!("{}/v1/chat/completions", gw.base);

    // No key, wrong key.
    let res = gw
        .http
        .post(&chat)
        .json(&hello(false))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    let res = gw
        .http
        .post(&chat)
        .bearer_auth("wrong-key")
        .json(&hello(false))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    // Unauthenticated health probe.
    let res = gw
        .http
        .get(format!("{}/health/ready", gw.base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 503);
    // Authenticated but not inference, or invalid.
    let res = gw
        .http
        .get(format!("{}/v1/models", gw.base))
        .bearer_auth(KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let res = gw
        .http
        .post(&chat)
        .bearer_auth(KEY)
        .json(&serde_json::json!({ "model": "gpt-4", "messages": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(pc.counters.packets(), 0);

    // The same gateway does wake for a real request.
    assert_ok(gw.chat(false).await, false).await;
    assert_eq!(pc.counters.packets(), 1);
}

#[tokio::test]
async fn invalid_wol_configuration_is_rejected() {
    let pc = pc(Power::On).await;
    for (name, value) in [
        ("WOL_MAC_ADDRESS", "AA:BB:CC:DD:EE"),
        ("WOL_MAC_ADDRESS", "zz:zz:zz:zz:zz:zz"),
        ("WOL_MAC_ADDRESS", "FF:FF:FF:FF:FF:FF"),
        ("WOL_BROADCAST_ADDRESS", "192.168.1"),
        ("WOL_BROADCAST_ADDRESS", "0.0.0.0"),
        ("WOL_PORT", "0"),
        ("WOL_POLL_INTERVAL_SECONDS", "0"),
        ("WOL_STARTUP_TIMEOUT_SECONDS", "abc"),
    ] {
        let result = Config::from_lookup(|n| match n {
            n if n == name => Some(value.into()),
            "OPENGATE_API_KEY" => Some(KEY.into()),
            "OLLAMA_BASE_URL" => Some(format!("http://{}", pc.ollama)),
            "OLLAMA_MODEL" => Some("llama3.1:8b".into()),
            "WOL_ENABLED" => Some("true".into()),
            "WOL_MAC_ADDRESS" => Some(MAC.into()),
            "WOL_BROADCAST_ADDRESS" => Some("127.0.0.1".into()),
            _ => None,
        });
        let err = result
            .err()
            .unwrap_or_else(|| panic!("{name}={value} accepted"));
        assert!(err.contains(name), "error names the variable: {err}");
    }
}

// ---- host-side relay ------------------------------------------------------

async fn relay_to(pc: &Pc) -> std::net::SocketAddr {
    let cfg = WolRelayConfig::from_lookup(|n| match n {
        "WOL_MAC_ADDRESS" => Some(MAC.into()),
        "WOL_BROADCAST_ADDRESS" => Some(pc.nic.ip().to_string()),
        "WOL_PORT" => Some(pc.nic.port().to_string()),
        "WOL_RELAY_LISTEN" => Some("127.0.0.1:0".into()),
        _ => None,
    })
    .unwrap();
    let relay = WolRelay::bind(cfg).await.unwrap();
    let addr = relay.local_addr().unwrap();
    tokio::spawn(relay.run());
    addr
}

#[tokio::test]
async fn relay_only_forwards_its_own_magic_packet() {
    let pc = pc(Power::Asleep(None)).await;
    let relay = relay_to(&pc).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

    client.send_to(b"not a magic packet", relay).await.unwrap();
    let mut other = vec![0xFF; 6];
    for _ in 0..16 {
        other.extend_from_slice(&[0x02, 0, 0, 0, 0, 0x99]);
    }
    client.send_to(&other, relay).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(pc.counters.packets(), 0, "foreign payloads are not relayed");

    client
        .send_to(&common::magic_packet(), relay)
        .await
        .unwrap();
    client
        .send_to(&common::magic_packet(), relay)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        pc.counters.packets(),
        1,
        "relayed once; the repeat is dropped"
    );
}

#[tokio::test]
async fn gateway_wakes_through_the_relay() {
    let pc = pc(Power::Asleep(Some(Duration::from_millis(100)))).await;
    let relay = relay_to(&pc).await;
    let mut cfg = config(&pc, true);
    // A hostname, as with host.docker.internal in Docker.
    cfg.wol.as_mut().unwrap().target = WolTarget::Relay(format!("localhost:{}", relay.port()));
    let gw = gateway(cfg).await;

    assert_ok(gw.chat(false).await, false).await;
    assert_eq!(pc.counters.packets(), 1);
}
