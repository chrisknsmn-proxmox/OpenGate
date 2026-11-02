# OpenGate

A small, authenticated, OpenAI-compatible gateway in front of **one local Ollama
model**. OpenGate never calls a cloud provider and holds no cloud credentials.

```
OpenClaw (LXC 103) ──► OpenGate (Coolify VM 104) ──► Ollama on 192.168.1.50 (llama3.1:8b)
      │
      └─ on an eligible failure, OpenClaw's own fallback ──► OpenAI model via
         OpenClaw's supported ChatGPT/Codex sign-in (never passes through OpenGate)
```

- **OpenGate**: auth, request validation, proxying to Ollama, streaming,
  timeouts, a concurrency limit, health, logs that never contain prompts, and
  [Wake-on-LAN](#wake-on-lan) for the Windows PC when it is asleep.
- **OpenClaw**: agents, tools, ChatGPT/Codex sign-in, primary/fallback choice.
- **Ollama**: runs the model.

Clients calling OpenGate directly get **no cloud fallback**: if Ollama is down
they get a clear 503/504. Fallback only happens in OpenClaw, and only for the
failures OpenClaw's [failover policy](https://docs.openclaw.ai/model-failover)
recognises. A slow or mediocre-but-successful answer never triggers it.

## API

| Endpoint | Auth | Purpose |
|---|---|---|
| `GET /health/live` | none | Process is up. No upstream call. Used by the container healthcheck. |
| `GET /health/ready` | none | Bounded check that Ollama is reachable and the model is installed. `200 {"status":"ready"}` or `503 {"status":"not_ready","reason":…}`. Reveals no addresses or secrets. |
| `GET /v1/models` | Bearer | Lists the one virtual model, `opengate/local`. |
| `POST /v1/chat/completions` | Bearer | OpenAI-style chat completion, streaming or not. With WOL enabled, may first wait up to `WOL_STARTUP_TIMEOUT_SECONDS` for the PC to wake. |

`/v1/*` requires `Authorization: Bearer <OPENGATE_API_KEY>` (constant-time check).

### Supported request fields

| Field | Notes |
|---|---|
| `model` | Must equal `OPENGATE_VIRTUAL_MODEL` (default `opengate/local`). Mapped to `OLLAMA_MODEL` internally; the real name is never accepted from or shown to clients. |
| `messages` | Roles `system`, `developer` (sent as `system`), `user`, `assistant`. Content is a string or text parts (joined with newlines). Message `name` is accepted and dropped. |
| `stream` | `true` for SSE. |
| `stream_options` | Only `{"include_usage": bool}`. |
| `temperature` (0–2), `top_p` (0–1), `presence_penalty`, `frequency_penalty` (−2–2), `seed`, `stop` (string or ≤4 strings) | Passed to Ollama. |
| `max_tokens` / `max_completion_tokens` | Either one, sent to Ollama as `max_tokens`. |
| `n` | Only `1`. |
| `user` | Accepted and dropped. |
| `tools: []`, `tool_choice: "none"`, `parallel_tool_calls` | No-ops, accepted. |

Everything else is **rejected with 400**, naming the parameter. That covers
non-empty `tools`, `tool` messages, `tool_calls`, images, `response_format`,
`logprobs` and so on. Tool calling is not supported until it is proven against
OpenClaw.

### Errors

OpenAI-shaped: `{"error":{"message","type","code","param"}}`. Messages never
include prompt content.

| Status | `code` | When |
|---|---|---|
| 400 | `invalid_json`, `invalid_request`, `unsupported_parameter`, `model_not_supported`, `tools_not_supported` | Caller error. OpenClaw treats 400 as terminal (no fallback). |
| 401 | `invalid_api_key` | Missing or wrong key. |
| 413 | `request_too_large` | Body over 4 MiB. |
| 502 | `upstream_error`, `upstream_invalid_response` | Ollama returned an error or garbage. |
| 503 | `upstream_unavailable` | Ollama unreachable (with WOL: still unreachable after the wake timeout, or a wake failed within the last `WOL_RETRY_COOLDOWN_SECONDS`). |
| 503 | `upstream_model_unavailable` | Model not installed on Ollama. |
| 503 | `upstream_overloaded` | Ollama said it is busy. |
| 503 | `gateway_overloaded` | More than `MAX_CONCURRENT_REQUESTS` in flight (`Retry-After: 1`). No queue. |
| 504 | `upstream_timeout` | Connect, overall or stream-idle timeout. |

502, 503 and 504 are all failover-eligible in OpenClaw.

### Streaming

Frames are relayed one at a time as they arrive (`data: {...}\n\n`), with
`model` rewritten to the virtual id. `data: [DONE]` is sent **only** if Ollama
sent it. If the stream fails after it has started (upstream error, idle timeout,
overall timeout, or EOF without `[DONE]`), OpenGate sends one error frame,
`data: {"error":{…}}`, and ends the stream **without** `[DONE]`. If the client
disconnects, the upstream request is dropped and Ollama stops generating.

## Configuration

| Variable | Default | Purpose |
|---|---|---|
| `OPENGATE_API_KEY` | **required** | ≥32 chars, no whitespace. Generate with `openssl rand -base64 48 \| tr -d '\n=+/'`. |
| `OLLAMA_BASE_URL` | **required** | e.g. `http://192.168.1.50:11434`. Plain http(s), no credentials or query. |
| `OLLAMA_MODEL` | **required** | Installed Ollama model, e.g. `llama3.1:8b`. |
| `OPENGATE_VIRTUAL_MODEL` | `opengate/local` | The only model id clients may send. |
| `OPENGATE_HOST` | `0.0.0.0` | Listen address. |
| `OPENGATE_PORT` | `8000` | Listen port. |
| `UPSTREAM_CONNECT_TIMEOUT_SECONDS` | `3` | TCP connect to Ollama. |
| `UPSTREAM_REQUEST_TIMEOUT_SECONDS` | `120` | Overall cap per request, **including a whole stream**. Raise it for very long generations. |
| `UPSTREAM_STREAM_IDLE_TIMEOUT_SECONDS` | `60` | Max gap between stream chunks. |
| `MAX_CONCURRENT_REQUESTS` | `2` | 1–64. Extra requests get an immediate 503. |
| `RUST_LOG` | `info` | Log filter. `info` logs method, path, status, latency and failure codes only. |
| `LOG_PROMPT_CONTENT` | `false` | `true` logs request messages (never responses). Debugging only; warns at startup. |
| `WOL_ENABLED` | `false` | `true` turns on [Wake-on-LAN](#wake-on-lan). |
| `WOL_MAC_ADDRESS` | — | MAC of the PC's wired NIC, `AA:BB:CC:DD:EE:64` or `08-BF-…`. Must be a unicast address. |
| `WOL_BROADCAST_ADDRESS` | — | LAN broadcast address, e.g. `192.168.1.255`. Used when OpenGate sends the packet itself. |
| `WOL_PORT` | `9` | UDP port for the magic packet (9 or 7 are conventional). |
| `WOL_STARTUP_TIMEOUT_SECONDS` | `90` | How long to wait for Ollama after waking (1–3600). |
| `WOL_POLL_INTERVAL_SECONDS` | `3` | How often to check Ollama while waiting. Must not exceed the timeout. |
| `WOL_RETRY_COOLDOWN_SECONDS` | `30` | After a failed wake, requests get an immediate 503 for this long; the next request after it makes a fresh attempt. |
| `WOL_RELAY_ADDRESS` | — | `host:port` of the [host-side relay](#wake-on-lan-from-docker), e.g. `host.docker.internal:9009`. When set, it is used instead of `WOL_BROADCAST_ADDRESS`. Needed in Docker. |
| `WOL_RELAY_LISTEN` | — | Relay only (`opengate wol-relay`): `IP:port` to listen on, e.g. `172.17.0.1:9009`. |

The process refuses to start if a required value is missing or invalid. Error
messages name the variable, never its value. Blank values count as unset.
Commit only `.env.example`; `.env` is git- and docker-ignored.

Wake-on-LAN values are only read when `WOL_ENABLED=true`. Then any WOL value
that is set must be valid, or OpenGate refuses to start. If the MAC, or both
the broadcast and relay addresses, are missing, OpenGate starts with WOL
**disabled** and logs a warning. The MAC and network settings live only in the
environment, never in the source.

## Wake-on-LAN

With `WOL_ENABLED=true`, every authenticated, valid `POST /v1/chat/completions`
(streaming or not) runs this before anything is sent to Ollama:

1. `GET /api/tags` on Ollama, with a timeout of `UPSTREAM_CONNECT_TIMEOUT_SECONDS`
   (capped at the poll interval). **Any** HTTP answer means the PC is awake.
   The request is forwarded at once and no packet is sent.
2. Otherwise it sends **one** magic packet (6 × `FF` + 16 × MAC, UDP to
   `WOL_BROADCAST_ADDRESS:WOL_PORT`, or to the relay). It then polls `/api/tags`
   every `WOL_POLL_INTERVAL_SECONDS` for up to `WOL_STARTUP_TIMEOUT_SECONDS`.
3. When Ollama answers, the original request is forwarded unchanged.
4. If it never answers, the request gets 503 `upstream_unavailable`, the same
   error as before, so OpenClaw fails over exactly as it did without WOL.

Details:

- **One wake for many requests.** The wake runs as a single background task.
  Requests that arrive while it runs wait on it without probing or sending
  packets of their own, so a burst sends one packet. Waiting is async and
  never blocks a Tokio worker.
- **Cancellation.** If a waiting client disconnects, its request stops waiting
  and frees its concurrency slot. The wake continues for whoever comes next.
- **Retry after failure.** After a timeout or a send error, requests fail fast
  with 503 for `WOL_RETRY_COOLDOWN_SECONDS`. That avoids flooding packets and
  lets OpenClaw fail over quickly. The first request after the cooldown makes a
  fresh attempt.
- **Concurrency limit first.** A request takes its `MAX_CONCURRENT_REQUESTS`
  slot before it waits, so a burst beyond the limit is told
  `gateway_overloaded` immediately instead of after the wake.
- **Nothing else wakes the PC.** That includes unauthenticated requests, wrong
  keys, invalid bodies, `/v1/models` and `/health/*`. There is no WOL endpoint.
  Running streams are never interrupted; the check happens only before a
  request starts.
- **Ollama stopped on an awake PC** also looks unreachable. OpenGate then sends
  a (harmless) packet and waits the full timeout before 503. So in that case
  OpenClaw's fallback starts up to `WOL_STARTUP_TIMEOUT_SECONDS` later than
  without WOL.
- **Client timeouts.** A cold request can take the boot time plus the
  generation time. OpenClaw's request timeout for this provider must be longer
  than `WOL_STARTUP_TIMEOUT_SECONDS` plus a normal answer. Otherwise it gives up
  (and falls back) before the PC is up. The next turn then finds the PC awake.

Logs (`opengate::wol` target) cover:

| Event | Message |
|---|---|
| Connectivity checks | `Ollama connectivity check: online` (debug), `Ollama connectivity check: unreachable` (warn, with `reason`) |
| Packet sent | `sent Wake-on-LAN magic packet` with `mac`, `destination` and `via_relay` |
| Waiting | `waiting for the Ollama host to start`, `Ollama not reachable yet` (debug, per poll), `joining the wake already in progress` |
| Up | `Ollama became available after wake` with `elapsed_ms` and `polls` |
| Failures | `Ollama did not become available before the startup timeout`, `failed to send Wake-on-LAN magic packet`, `a recent wake attempt failed; not retrying…` |
| Resumed | `resuming request after wake` with `waited_ms` |

Set `RUST_LOG=info,opengate::wol=debug` to see each poll.

### Windows PC prerequisites

- In UEFI/BIOS, enable Wake-on-LAN (or "Power on by PCI-E").
- In Device Manager → NIC → **Power Management**, tick *Allow this device to
  wake the computer* and *Only allow a magic packet*. Under **Advanced**,
  enable *Wake on Magic Packet*.
- Use wired Ethernet. To wake from full shutdown (S5), disable Windows *Fast
  startup*. Waking from sleep usually works without that.
- Ollama must start by itself after boot: as a service, or at login with
  automatic sign-in. Keep `OLLAMA_HOST=0.0.0.0` as described below.
- Test from the Ubuntu VM first. Put the PC to sleep, then run
  `wakeonlan -i 192.168.1.255 AA:BB:CC:DD:EE:64` (or the in-container test below).

### Wake-on-LAN from Docker

**A broadcast sent from a container on a Docker bridge network does not reach
the LAN.** That covers the default bridge and Coolify's networks alike. This
was checked on the dev VM (Docker 29.9) by capturing `eth0` while sending UDP to
`192.168.1.255`:

| Sender | Seen on `eth0`? |
|---|---|
| The VM itself | yes, as an Ethernet broadcast |
| Container on the default bridge | **no**, consumed by the host |
| Container on a user-defined bridge | **no**, consumed by the host |
| Container sending to `255.255.255.255` | **no**, `Permission denied` |

The host treats the subnet broadcast address as its own. It delivers the packet
locally instead of forwarding it, because `net.ipv4.conf.*.bc_forwarding` is
`0` by default. So "WOL works on the VM" does not mean it works in the container.

Options considered:

- **Host-side relay (used).** `opengate wol-relay` is the same binary and
  image, run with host networking. It is unprivileged: uid 10001, read-only
  root filesystem, all capabilities dropped, no Docker socket. OpenGate sends
  its magic packet as plain unicast UDP to the relay, and the relay broadcasts
  it on the LAN. The relay only ever broadcasts the packet for its own
  `WOL_MAC_ADDRESS`, and only when it receives exactly that packet; it rate-limits
  repeats to one every 2 seconds. So the most anyone who can reach it could do
  is wake that one PC, which any device on the LAN can already do. It listens
  on the Docker bridge gateway (`172.17.0.1`), which containers on that host
  can reach and the LAN cannot. It has no HTTP endpoint. Inference requests
  remain the only way to trigger a wake through OpenGate's API.
- `network_mode: host` for OpenGate itself. Rejected: it takes OpenGate off
  Coolify's proxy network, and the API would listen on the VM's LAN address.
- macvlan/ipvlan network. Rejected: more network setup, and the container gets
  its own LAN address.
- Enabling `bc_forwarding` on the host. Rejected: a host-wide routing change,
  and it lets every container broadcast onto the LAN.
- Privileged container or `NET_RAW`/`NET_ADMIN`. Not needed and not used.

Verified on the dev VM, end to end: an unprivileged OpenGate container on a
user-defined bridge sent its packet to `host.docker.internal:9009`. The relay
then put a 102-byte magic packet on `eth0` as an Ethernet broadcast
(`ff:ff:ff:ff:ff:ff`, to `192.168.1.255`). The test used a dummy MAC and port.

## Development (Ubuntu VM)

The VM has Docker but no Rust toolchain. Either install Rust for your user:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal -c clippy,rustfmt
. ~/.cargo/env
cargo test && cargo build
```

or run cargo in the same pinned image the Dockerfile uses, with nothing installed on the host:

```bash
mkdir -p ~/.cache/opengate-cargo
docker run --rm -u "$(id -u):$(id -g)" -v "$PWD":/app -w /app \
  -v ~/.cache/opengate-cargo:/cargo -e CARGO_HOME=/cargo \
  rust:1.92-slim-trixie cargo test
```

The tests (`cargo test`) run a real OpenGate against a mock Ollama. They cover
auth, validation, nonstreaming, streaming (frames split across reads, single
`[DONE]`, incremental delivery), upstream down, missing model, upstream 500,
connect/overall/idle timeouts, failed streams never ending in `[DONE]`, the
concurrency limit, client-abort cancellation, and a log-privacy test at TRACE
level.

`tests/wol.rs` uses a mock PC. Its Ollama port is bound but not listening, so
connections are refused like a host that is off, until its UDP "NIC" receives
the exact magic packet. Those tests cover:
- Ollama already online (no packet)
- a wake followed by the resumed request, streaming and not
- the startup timeout, cooldown and retry
- a lost first packet being retried
- six concurrent requests sharing one packet
- a cancelled waiter freeing its slot
- WOL disabled
- invalid MAC, broadcast and timing values
- unauthorized, invalid and non-inference requests never waking
- the relay ignoring foreign payloads and repeats
- a wake through the relay by hostname

Before committing:

```bash
cargo fmt --check && cargo clippy -- -D warnings && cargo test && cargo build --release
```

### Test Wake-on-LAN locally

Run directly on the VM (not in Docker), so the broadcast can reach the LAN:

```bash
set -a; . ./.env; set +a
WOL_ENABLED=true WOL_RELAY_ADDRESS= RUST_LOG=info,opengate::wol=debug cargo run --release
```

Put the PC to sleep, wait until `curl -m 3 http://192.168.1.50:11434/api/tags`
fails, then send a chat request (see [Smoke tests](#smoke-tests)). The logs
should show `unreachable` → `sent Wake-on-LAN magic packet` → polls →
`Ollama became available after wake` → `resuming request after wake`, and the
request should return 200. Send a second request: it should log `online` and no
packet.

Check that the Windows Ollama host is reachable:

```bash
curl --connect-timeout 5 --max-time 10 http://192.168.1.50:11434/api/version
curl --connect-timeout 5 --max-time 10 http://192.168.1.50:11434/api/tags
```

Ollama on Windows must listen on the LAN (`OLLAMA_HOST=0.0.0.0`). Restrict TCP
11434 in Windows Firewall to the Ubuntu VM and Coolify VM 104 addresses, and
never forward it to the Internet.

### Run in Docker locally

```bash
cp .env.example .env   # then set OPENGATE_API_KEY
docker compose -f compose.yaml -f compose.dev.yaml up --build -d
```

`compose.dev.yaml` publishes on `127.0.0.1:8000` only. The container runs as
uid 10001 with a read-only root filesystem, all capabilities dropped and
`no-new-privileges`. Test the **container's** path to Ollama, not just the host's:

```bash
curl -s --max-time 10 http://127.0.0.1:8000/health/ready   # probe runs from inside the container
```

For Wake-on-LAN in Docker, also start the relay, and set `WOL_ENABLED=true` and
`WOL_RELAY_ADDRESS=host.docker.internal:9009` in `.env`:

```bash
docker compose -f compose.wol-relay.yaml up -d --build
docker compose -f compose.yaml -f compose.dev.yaml up --build -d
```

### Smoke tests

```bash
set -a; . ./.env; set +a
B=http://127.0.0.1:8000

curl -i $B/health/live
curl -i $B/v1/models -H "Authorization: Bearer $OPENGATE_API_KEY"

curl -sS $B/v1/chat/completions -H "Authorization: Bearer $OPENGATE_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"opengate/local","messages":[{"role":"user","content":"Say hello in one sentence."}],"stream":false}'

curl -N $B/v1/chat/completions -H "Authorization: Bearer $OPENGATE_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"opengate/local","messages":[{"role":"user","content":"Count to three."}],"stream":true}'

# Expected failures
curl -i $B/v1/models                                              # 401
curl -sS $B/v1/chat/completions -H "Authorization: Bearer $OPENGATE_API_KEY" \
  -d '{"model":"gpt-4","messages":[{"role":"user","content":"x"}]}'  # 400 model_not_supported
```

## Deploy on Coolify (VM 104)

1. **New Resource → Git-based → Docker Compose**. Pick
   `chrisknsmn-proxmox/OpenGate`, branch `main`, base directory `/`, compose
   file `compose.yaml`.
2. Under **Environment Variables**, set `OPENGATE_API_KEY` (as a secret; use a
   different value from your dev `.env`) and `OLLAMA_BASE_URL`. Coolify blocks
   deployment until both are filled in. The other variables have defaults.
   For Wake-on-LAN, also set:

   ```
   WOL_ENABLED=true
   WOL_MAC_ADDRESS=AA:BB:CC:DD:EE:64
   WOL_BROADCAST_ADDRESS=192.168.1.255
   WOL_RELAY_ADDRESS=host.docker.internal:9009
   ```
3. Under **Domains**, add a domain for service `opengate` with the internal
   port suffix, e.g. `https://opengate.<your-domain>:8000`. Keep it reachable
   only from trusted networks (LAN/VPN or proxy allowlist); it is the single
   protected endpoint. Never publish Ollama's port.
4. **Wake-on-LAN relay.** Add a second resource on the same server (VM 104):
   **New Resource → Git-based → Docker Compose**, same repository and branch,
   compose file `compose.wol-relay.yaml`. Set `WOL_MAC_ADDRESS=AA:BB:CC:DD:EE:64`
   and `WOL_BROADCAST_ADDRESS=192.168.1.255`. `WOL_PORT` defaults to `9` and
   `WOL_RELAY_LISTEN` to `172.17.0.1:9009`. It needs no domain.
   - It uses `network_mode: host`, and no privileges or capabilities. Check that
     Coolify kept host networking: `docker inspect -f '{{.HostConfig.NetworkMode}}' <relay-container>`
     should print `host`.
   - VM 104 must be on the same L2 segment as the PC (Proxmox bridge on the
     LAN), as the dev VM is.
   - `172.17.0.1` is the `docker0` gateway, which `host-gateway` maps to. If
     Docker's `bip`/`host-gateway-ip` is customised, use that address on both
     sides. If VM 104 runs a host firewall (ufw/firewalld), allow UDP 9009 in
     from the Docker networks.
   - Instead of Coolify, the relay can run as a plain container on VM 104:
     `docker run -d --name opengate-wol-relay --restart unless-stopped --network host --read-only --cap-drop ALL --security-opt no-new-privileges -e WOL_MAC_ADDRESS=… -e WOL_BROADCAST_ADDRESS=192.168.1.255 -e WOL_RELAY_LISTEN=172.17.0.1:9009 <opengate image> opengate wol-relay`.
5. Deploy, then verify from VM 104 / LXC 103:
   - `/health/live` returns 200 and the container shows *healthy*.
   - `/health/ready` returns 200. This proves the container can reach Ollama.
   - `/v1/models` returns 401 without the key and 200 with it.
   - A streaming request arrives incrementally and ends in `[DONE]`.
   - Logs show one line per request and no prompt text.
   - The OpenGate startup log shows `wol: Some(WolConfig { … target: Relay("host.docker.internal:9009") … })`.
     If it shows `wol: None`, a value is missing; look for the WOL warning.
   - The relay log shows `Wake-on-LAN relay listening`.

### Test Wake-on-LAN from inside the production container

Find the containers on VM 104 with `docker ps` (Coolify names them after the
resource; the images run `opengate` and `opengate wol-relay`).

1. **Container → Ollama path** (with the PC awake). Expect `open`:

   ```bash
   docker exec <opengate-container> bash -c 'timeout 5 bash -c "</dev/tcp/192.168.1.50/11434" && echo open'
   ```

   `No route to host` while the PC is asleep is normal.

2. **Packet path: container → relay → LAN.** Put the PC to sleep, then send
   the exact packet OpenGate would send, from inside the OpenGate container,
   using its own environment:

   ```bash
   docker exec -it <opengate-container> bash
   # inside the container:
   m=''; for b in ${WOL_MAC_ADDRESS//[:-]/ }; do m+="\\x$b"; done
   p='\xff\xff\xff\xff\xff\xff'; for i in {1..16}; do p+="$m"; done
   printf "$p" > "/dev/udp/${WOL_RELAY_ADDRESS%:*}/${WOL_RELAY_ADDRESS##*:}" && echo "magic packet sent"
   exit
   docker logs --tail 5 <relay-container>   # expect: relayed Wake-on-LAN magic packet from=…
   ```

   The PC should power on. If the relay logs nothing, the container cannot
   reach it; check `WOL_RELAY_LISTEN`, the host firewall, and
   `docker exec <opengate-container> getent hosts host.docker.internal`.
   If the relay logs the packet but the PC stays off, the problem is between
   VM 104 and the PC (L2 segment, NIC or BIOS settings). Check with
   `tcpdump -ni any udp port 9` on VM 104.

3. **End to end.** With the PC asleep, send a real chat request through the
   public endpoint, then follow the logs. Expect `unreachable` →
   `sent Wake-on-LAN magic packet … via_relay=true` → `Ollama became available
   after wake` → `resuming request after wake`, and a 200:

   ```bash
   docker logs -f <opengate-container> 2>&1 | grep -E 'wol|chat/completions'
   ```

   A second request right after should log no packet.

## OpenClaw integration

Verify each step against the **installed** OpenClaw release; these are not
drop-in values.

1. **Add OpenGate as a custom provider** ([custom providers](https://docs.openclaw.ai/gateway/config-tools/custom-providers)).
   This is a sketch to check against the installed schema (`apiKey` SecretRef
   syntax in particular):

   ```json5
   {
     models: {
       providers: {
         opengate: {
           baseUrl: "https://opengate.<your-domain>/v1",
           apiKey: "<SecretRef/env reference to OPENGATE_API_KEY>",
           api: "openai-completions",
           models: [{
             id: "opengate/local",
             name: "OpenGate local (llama3.1:8b)",
             input: ["text"],
             contextWindow: 32768,      // must match Ollama's real context length, see below
             maxTokens: 4096,
             compat: {
               supportsTools: false,         // OpenGate rejects tools; otherwise every agent turn is a terminal 400
               requiresStringContent: true,
               strictMessageKeys: true,      // OpenGate accepts only role/content/name
             },
           }],
         },
       },
     },
     agents: {
       defaults: {
         model: {
           primary: "opengate/opengate/local",   // confirm with `openclaw models list`
           fallbacks: ["openai/<verified-subscription-model>"],
         },
       },
     },
   }
   ```

   If the doubled `opengate/opengate/local` causes trouble, set
   `OPENGATE_VIRTUAL_MODEL=local` and use model id `local`, so the ref becomes `opengate/local`.

2. **Context length.** Ollama's OpenAI endpoint cannot set `num_ctx` per
   request and silently truncates long prompts. OpenClaw agent prompts are
   large. On the Windows host, set `OLLAMA_CONTEXT_LENGTH` (e.g. `32768`, as
   VRAM allows), restart Ollama, and set the same value as `contextWindow`.

3. **Subscription fallback.** Sign in with OpenClaw's supported flow (headless LXC):
   `openclaw models auth login --provider openai --method device-code`
   ([auth docs](https://docs.openclaw.ai/providers/openai/authentication)).
   Then pick a model your account actually exposes (`openclaw models list`) and
   put that exact ref in `fallbacks`. Never copy tokens out of OpenClaw, and
   never give OpenGate a ChatGPT credential.

4. **Prove it.** With Ollama up, confirm a turn is served locally (OpenGate
   logs a 200). Then stop Ollama: OpenGate returns 503 `upstream_unavailable`,
   and OpenClaw should log a fallback to the OpenAI model. Restore Ollama and
   confirm the next turn is local again. A 503 alone does not prove fallback
   works; check OpenClaw's logs. Note that 400s (bad request, tools) do **not**
   trigger OpenClaw fallback.

## Out of scope

No tev1 or other routing model, no complexity scoring, no cloud SDKs or
credentials, no database, no dashboard, one model only.
