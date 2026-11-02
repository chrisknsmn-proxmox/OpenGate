//! Wake-on-LAN for the Ollama host.
//!
//! Before an inference request is forwarded, OpenGate checks that Ollama
//! answers `/api/tags`. If it does, the request goes straight through. If not,
//! one background task sends a single magic packet and polls Ollama until it
//! answers or the startup timeout passes. Concurrent requests wait on that same
//! task, so a burst of requests sends one packet. A request whose client goes
//! away just stops waiting; the wake carries on for whoever comes next. After
//! a failed wake, requests fail fast for the retry cooldown, then the next
//! request starts a fresh attempt.
//!
//! [`WolRelay`] is the host-side half used when OpenGate runs on a Docker
//! bridge network, where broadcasts cannot reach the LAN (see README).

use std::{
    io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{
    net::UdpSocket,
    sync::watch,
    time::{self, Instant, MissedTickBehavior},
};
use tracing::{debug, error, info, warn};

use crate::{
    config::{MacAddress, WolConfig, WolRelayConfig, WolTarget},
    error::ApiError,
    upstream::Ollama,
};

const MAGIC_LEN: usize = 6 + 16 * 6;
/// The relay ignores repeats closer together than this. OpenGate itself sends
/// one packet per attempt, and attempts are at least the retry cooldown apart.
const RELAY_MIN_GAP: Duration = Duration::from_secs(2);

/// Six `0xFF` bytes followed by the MAC sixteen times.
pub(crate) fn magic_packet(mac: MacAddress) -> [u8; MAGIC_LEN] {
    let mut packet = [0xFF; MAGIC_LEN];
    for chunk in packet[6..].as_chunks_mut::<6>().0 {
        *chunk = mac.0;
    }
    packet
}

async fn send_packet(packet: &[u8], dest: SocketAddr) -> io::Result<()> {
    let local: SocketAddr = if dest.is_ipv4() {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    } else {
        (Ipv6Addr::UNSPECIFIED, 0).into()
    };
    let socket = UdpSocket::bind(local).await?;
    if dest.is_ipv4() {
        // Needs no capability; without it the kernel refuses broadcast destinations.
        socket.set_broadcast(true)?;
    }
    let sent = socket.send_to(packet, dest).await?;
    if sent != packet.len() {
        return Err(io::Error::other("magic packet was truncated"));
    }
    Ok(())
}

/// Outcome of a wake attempt: `None` while running, then `Some(succeeded)`.
type Outcome = watch::Receiver<Option<bool>>;

#[derive(Clone)]
pub(crate) struct Wol(Arc<Inner>);

struct Inner {
    config: WolConfig,
    ollama: Ollama,
    probe_timeout: Duration,
    state: Mutex<State>,
}

enum State {
    /// No wake running. `woke_at` is when the last wake succeeded.
    Idle {
        woke_at: Option<Instant>,
    },
    Waking(Outcome),
    Failed {
        at: Instant,
    },
}

enum Next {
    Ready,
    Wait(Outcome),
    CoolingDown,
}

impl Wol {
    pub fn new(config: WolConfig, ollama: Ollama) -> Self {
        // Keep each probe within one poll interval so polling keeps its cadence.
        let probe_timeout = ollama.probe_timeout().min(config.poll_interval);
        Wol(Arc::new(Inner {
            config,
            ollama,
            probe_timeout,
            state: Mutex::new(State::Idle { woke_at: None }),
        }))
    }

    /// Returns once Ollama answers, waking its host first if needed.
    /// Fails with `upstream_unavailable` (fallback reason `wol_timeout`) if it
    /// does not come up in time.
    pub async fn ensure_ready(&self) -> Result<(), ApiError> {
        let started = Instant::now();
        let next = match self.in_progress() {
            Some(outcome) => {
                info!("Ollama host is starting; joining the wake already in progress");
                Next::Wait(outcome)
            }
            None => match self.0.ollama.reachable(self.0.probe_timeout).await {
                Ok(()) => {
                    debug!(
                        latency_ms = started.elapsed().as_millis(),
                        "Ollama connectivity check: online"
                    );
                    return Ok(());
                }
                Err(reason) => {
                    warn!(
                        reason,
                        latency_ms = started.elapsed().as_millis(),
                        "Ollama connectivity check: unreachable"
                    );
                    self.join_or_start(started)
                }
            },
        };

        match next {
            Next::Ready => Ok(()),
            Next::CoolingDown => {
                warn!(
                    cooldown_ms = self.0.config.retry_cooldown.as_millis(),
                    "a recent wake attempt failed; not retrying until the cooldown ends"
                );
                Err(ApiError::wol_timeout())
            }
            Next::Wait(mut outcome) => {
                // Dropping this future (client gone) just stops waiting.
                let woke = outcome
                    .wait_for(Option::is_some)
                    .await
                    .is_ok_and(|v| *v == Some(true));
                if woke {
                    info!(
                        waited_ms = started.elapsed().as_millis(),
                        "resuming request after wake"
                    );
                    Ok(())
                } else {
                    Err(ApiError::wol_timeout())
                }
            }
        }
    }

    fn in_progress(&self) -> Option<Outcome> {
        match &*self.0.state.lock().expect("wake state lock") {
            State::Waking(outcome) if outcome.has_changed().is_ok() => Some(outcome.clone()),
            _ => None,
        }
    }

    fn join_or_start(&self, probe_started: Instant) -> Next {
        let mut state = self.0.state.lock().expect("wake state lock");
        match &*state {
            // `has_changed` errs only if the wake task died without reporting.
            State::Waking(outcome) if outcome.has_changed().is_ok() => {
                return Next::Wait(outcome.clone())
            }
            // A wake finished while this request was probing.
            State::Idle { woke_at: Some(at) } if *at >= probe_started => return Next::Ready,
            State::Failed { at } if at.elapsed() < self.0.config.retry_cooldown => {
                return Next::CoolingDown
            }
            _ => {}
        }
        let (tx, outcome) = watch::channel(None);
        *state = State::Waking(outcome.clone());
        drop(state);
        // A task, not part of any request, so it survives client cancellation
        // and never blocks a worker thread.
        tokio::spawn(self.clone().wake(tx));
        Next::Wait(outcome)
    }

    async fn wake(self, tx: watch::Sender<Option<bool>>) {
        let ok = self.send_and_wait().await;
        let now = Instant::now();
        *self.0.state.lock().expect("wake state lock") = if ok {
            State::Idle { woke_at: Some(now) }
        } else {
            State::Failed { at: now }
        };
        tx.send_replace(Some(ok));
    }

    async fn send_and_wait(&self) -> bool {
        let c = &self.0.config;
        let started = Instant::now();
        let dest = match &c.target {
            WolTarget::Broadcast(addr) => Ok(SocketAddr::V4(*addr)),
            WolTarget::Relay(addr) => resolve(addr).await,
        };
        let sent = match dest {
            Ok(dest) => send_packet(&magic_packet(c.mac), dest).await.map(|()| dest),
            Err(e) => Err(e),
        };
        match sent {
            Ok(dest) => info!(
                mac = %c.mac,
                destination = %dest,
                via_relay = matches!(c.target, WolTarget::Relay(_)),
                "sent Wake-on-LAN magic packet"
            ),
            Err(e) => {
                error!(mac = %c.mac, error = %e, "failed to send Wake-on-LAN magic packet");
                return false;
            }
        }

        info!(
            timeout_ms = c.startup_timeout.as_millis(),
            poll_interval_ms = c.poll_interval.as_millis(),
            "waiting for the Ollama host to start"
        );
        let deadline = started + c.startup_timeout;
        let mut ticks = time::interval_at(started + c.poll_interval, c.poll_interval);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut polls = 0u32;
        while time::timeout_at(deadline, ticks.tick()).await.is_ok() {
            polls += 1;
            let timeout = self
                .0
                .probe_timeout
                .min(deadline.saturating_duration_since(Instant::now()));
            if timeout.is_zero() {
                break;
            }
            match self.0.ollama.reachable(timeout).await {
                Ok(()) => {
                    info!(
                        elapsed_ms = started.elapsed().as_millis(),
                        polls, "Ollama became available after wake"
                    );
                    return true;
                }
                Err(reason) => debug!(
                    reason,
                    polls,
                    elapsed_ms = started.elapsed().as_millis(),
                    "Ollama not reachable yet"
                ),
            }
        }
        warn!(
            timeout_ms = c.startup_timeout.as_millis(),
            polls, "Ollama did not become available before the startup timeout"
        );
        false
    }
}

/// Prefers IPv4: the relay listens on a Docker bridge gateway address.
async fn resolve(addr: &str) -> io::Result<SocketAddr> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(addr).await?.collect();
    let v4 = addrs.iter().find(|a| a.is_ipv4());
    v4.or(addrs.first())
        .copied()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "WOL_RELAY_ADDRESS did not resolve"))
}

/// Host-side relay (`opengate wol-relay`). It listens for OpenGate's unicast
/// magic packet and re-broadcasts it on the LAN. It only ever broadcasts the
/// packet for its own configured MAC, so all it can do is wake that one
/// machine, which any LAN device can do anyway.
pub struct WolRelay {
    socket: UdpSocket,
    config: WolRelayConfig,
}

impl WolRelay {
    pub async fn bind(config: WolRelayConfig) -> io::Result<Self> {
        let socket = UdpSocket::bind(config.listen).await?;
        Ok(WolRelay { socket, config })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub async fn run(self) {
        let packet = magic_packet(self.config.mac);
        let dest = SocketAddr::V4(self.config.broadcast);
        info!(
            listen = %self.local_addr().map_or_else(|_| "?".into(), |a| a.to_string()),
            mac = %self.config.mac,
            destination = %dest,
            "Wake-on-LAN relay listening"
        );
        let mut last_sent: Option<Instant> = None;
        let mut buf = [0u8; 1500];
        loop {
            let (len, from) = match self.socket.recv_from(&mut buf).await {
                Ok(received) => received,
                Err(e) => {
                    warn!(error = %e, "relay receive failed");
                    time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            if buf[..len] != packet {
                debug!(%from, bytes = len, "relay ignored a datagram that is not the configured magic packet");
                continue;
            }
            if last_sent.is_some_and(|t| t.elapsed() < RELAY_MIN_GAP) {
                debug!(%from, "relay ignored a repeated wake request");
                continue;
            }
            last_sent = Some(Instant::now());
            match send_packet(&packet, dest).await {
                Ok(()) => {
                    info!(%from, mac = %self.config.mac, destination = %dest, "relayed Wake-on-LAN magic packet")
                }
                Err(e) => {
                    error!(%from, error = %e, "relay failed to broadcast Wake-on-LAN magic packet")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_packet_layout() {
        let mac: MacAddress = "02:00:5e:10:00:01".parse().unwrap();
        let p = magic_packet(mac);
        assert_eq!(p.len(), 102);
        assert_eq!(p[..6], [0xFF; 6]);
        for rep in p[6..].chunks(6) {
            assert_eq!(rep, [0x02, 0x00, 0x5e, 0x10, 0x00, 0x01]);
        }
    }
}
