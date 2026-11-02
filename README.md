# OpenGate

A small, authenticated, OpenAI-compatible gateway, **local first**. Every
request goes to one local Ollama model, waking the Windows PC with
[Wake-on-LAN](#wake-on-lan) if it is asleep. Only if local inference is
unavailable can OpenGate fall back to **your ChatGPT plan**, through the
official [Sign in with ChatGPT](#chatgpt-plan-fallback) flow. OpenGate has no
OpenAI API-key or API-billing path at all.

```
App / AI agent ──► OpenGate (Coolify VM 104, :8100) ── authenticate client
                        │
                        ├─► Ollama on 192.168.1.50 (llama3.1:8b) ── available ──► local answer
                        │      └─ unreachable ─► Wake-on-LAN ─► wait ─► up ──► local answer
                        │                                        └─ timeout
                        │                                              ▼
                        └─► ChatGPT plan fallback (optional, off by default)
                               OAuth token from Sign in with ChatGPT
                               ──► POST https://api.openai.com/v1/responses (store:false, stream:true)
                               └─ not signed in / limit reached / down ──► clear error, no other billing
```

- **Local first, always.** Ollama is tried first for every request, and the
  existing Wake-on-LAN behaviour is unchanged.
- **Fallback only on unavailability.** OpenGate falls back when Ollama is
  unreachable, the wake times out, the model is missing, Ollama is
  overloaded, times out or returns a 5xx. It never falls back because a prompt
  looks hard, after Ollama refused a request (4xx), or once Ollama has started
  answering.
- **No silent billing.** The ChatGPT plan's own limits apply. When they are
  reached the client gets `subscription_limit_reached`, and nothing is retried
  elsewhere.
- With `CHATGPT_FALLBACK_ENABLED=false` (the default), OpenGate behaves exactly
  as before: if Ollama is down, the client gets a clear 503/504.

## API

| Endpoint | Auth | Purpose |
|---|---|---|
| `GET /health/live` | none | Process is up. No upstream call. Used by the container healthcheck. |
| `GET /health/ready` | none | Bounded check that Ollama is reachable and the model is installed. `200 {"status":"ready"}` or `503 {"status":"not_ready","reason":…}`. Reveals no addresses or secrets. |
| `GET /health/providers` | none | Separate status for the gateway, Ollama and the ChatGPT plan (see [Health](#health)). Always 200. Never wakes the PC or spends plan usage. |
| `GET /v1/models` | Bearer | Lists the one virtual model, `opengate/local`. |
| `POST /v1/chat/completions` | Bearer | OpenAI-style chat completion, streaming or not. With WOL enabled, may first wait up to `WOL_STARTUP_TIMEOUT_SECONDS` for the PC to wake. |

`/v1/*` requires `Authorization: Bearer <OPENGATE_API_KEY>` (constant-time check).
Unauthenticated requests never reach Wake-on-LAN or the ChatGPT plan.

Every chat response carries `x-opengate-provider: ollama` or `chatgpt`.
The `model` field is always the virtual model, whichever provider answered.

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

With the ChatGPT fallback enabled, a local failure in the 502–504 rows above
(other than a 4xx from Ollama) triggers the fallback instead of being returned
directly. If the fallback fails too, the client gets one combined error, with
both outcomes in `error.providers`:

```json
{"error":{"code":"all_providers_unavailable","type":"server_error","param":null,
  "message":"local model unavailable (wol_timeout: …); ChatGPT plan fallback failed (subscription_not_authorized: …)",
  "providers":{"ollama":"wol_timeout","chatgpt":"subscription_not_authorized"}}}
```

| Status | `code` | When |
|---|---|---|
| 503 | `all_providers_unavailable` | Ollama unavailable and the ChatGPT plan could not serve the request. See `error.providers`. |
| 429 | `subscription_limit_reached` | Ollama unavailable and the ChatGPT plan (or OpenGate's app limit) is used up. No other billing is tried. |

`error.providers.ollama` is one of:

| Value | Meaning |
|---|---|
| `wol_timeout` | The PC did not answer within `WOL_STARTUP_TIMEOUT_SECONDS` after the magic packet, or a wake failed within the cooldown. |
| `ollama_unavailable` | Unreachable (WOL off), model missing, busy, timed out, or a 5xx/garbled reply. |

`error.providers.chatgpt` (and the `code` of a mid-stream error frame from the ChatGPT plan) is one of:

| Value | Meaning | What to do |
|---|---|---|
| `subscription_not_configured` | No credential file: never signed in. | `opengate chatgpt login` |
| `subscription_not_authorized` | Signed out; plan usage not granted at consent; session revoked or app disconnected in ChatGPT settings; account/workspace not eligible; credentials unreadable (wrong `CHATGPT_CREDENTIALS_KEY`). The message says which. | Sign in again (`--reconsent` if plan usage was declined). |
| `subscription_expired` | The refresh token expired (30 days unused). | `opengate chatgpt login` |
| `subscription_limit_reached` | ChatGPT plan or app usage limit. Fallback pauses for `CHATGPT_LIMIT_PAUSE_SECONDS`. | Wait, or review limits at [chatgpt.com/settings/usage](https://chatgpt.com/settings/usage). |
| `subscription_provider_unavailable` | OpenAI or the network is unavailable. Credentials are kept. | Retry later. |
| `unsupported_request` | The request uses something the ChatGPT plan route cannot do, e.g. `temperature` or `max_tokens` (named in `param`). | See [Supported requests](#supported-requests-on-the-chatgpt-plan). |

### Streaming

Frames are relayed one at a time as they arrive (`data: {...}\n\n`), with
`model` rewritten to the virtual id. `data: [DONE]` is sent **only** if Ollama
sent it. If the stream fails after it has started (upstream error, idle timeout,
overall timeout, or EOF without `[DONE]`), OpenGate sends one error frame,
`data: {"error":{…}}`, and ends the stream **without** `[DONE]`. If the client
disconnects, the upstream request is dropped and Ollama stops generating.

The same rules apply to a ChatGPT plan stream. OpenAI's events are translated
to `chat.completion.chunk` frames, and `[DONE]` follows only
`response.completed` (or `response.incomplete`, sent with `finish_reason:
"length"`). A usage limit or error mid-stream ends the stream with one error
frame and no `[DONE]`. Once any output has been sent, OpenGate never retries
on another provider.

## Configuration

| Variable | Default | Purpose |
|---|---|---|
| `OPENGATE_API_KEY` | **required** | ≥32 chars, no whitespace. Generate with `openssl rand -base64 48 \| tr -d '\n=+/'`. |
| `OLLAMA_BASE_URL` | **required** | e.g. `http://192.168.1.50:11434`. Plain http(s), no credentials or query. |
| `OLLAMA_MODEL` | **required** | Installed Ollama model, e.g. `llama3.1:8b`. |
| `OPENGATE_VIRTUAL_MODEL` | `opengate/local` | The only model id clients may send. |
| `OPENGATE_HOST` | `0.0.0.0` | Listen address. |
| `OPENGATE_PORT` | `8000` | Listen port. |
| `OPENGATE_PUBLISH_ADDRESS`, `OPENGATE_PUBLISH_PORT` | `127.0.0.1`, `8100` | Docker only: host address and port compose publishes the container on. Never `8000` on the Coolify server (its dashboard uses it). |
| `UPSTREAM_CONNECT_TIMEOUT_SECONDS` | `3` | TCP connect to Ollama. |
| `UPSTREAM_REQUEST_TIMEOUT_SECONDS` | `120` | Overall cap per request, **including a whole stream**. Raise it for very long generations. |
| `UPSTREAM_STREAM_IDLE_TIMEOUT_SECONDS` | `60` | Max gap between stream chunks. |
| `MAX_CONCURRENT_REQUESTS` | `2` | 1–64. Extra requests get an immediate 503. |
| `RUST_LOG` | `info` | Log filter. `info` logs method, path, status, latency and failure codes only. |
| `LOG_PROMPT_CONTENT` | `false` | `true` logs request messages (never responses). Debugging only; warns at startup. |
| `WOL_ENABLED` | `false` | `true` turns on [Wake-on-LAN](#wake-on-lan). |
| `WOL_MAC_ADDRESS` | — | MAC of the PC's wired NIC, `AA:BB:CC:DD:EE:64` or `AA-BB-CC-DD-EE-64`. Must be a unicast address. |
| `WOL_BROADCAST_ADDRESS` | — | LAN broadcast address, e.g. `192.168.1.255`. Used when OpenGate sends the packet itself. |
| `WOL_PORT` | `9` | UDP port for the magic packet (9 or 7 are conventional). |
| `WOL_STARTUP_TIMEOUT_SECONDS` | `90` | How long to wait for Ollama after waking (1–3600). |
| `WOL_POLL_INTERVAL_SECONDS` | `3` | How often to check Ollama while waiting. Must not exceed the timeout. |
| `WOL_RETRY_COOLDOWN_SECONDS` | `30` | After a failed wake, requests get an immediate 503 for this long; the next request after it makes a fresh attempt. |
| `WOL_RELAY_ADDRESS` | — | `host:port` of the [host-side relay](#wake-on-lan-from-docker), e.g. `host.docker.internal:9009`. When set, it is used instead of `WOL_BROADCAST_ADDRESS`. Needed in Docker. |
| `WOL_RELAY_LISTEN` | — | Relay only (`opengate wol-relay`): `IP:port` to listen on, e.g. `172.17.0.1:9009`. |
| `CHATGPT_FALLBACK_ENABLED` | `false` | `true` turns on the [ChatGPT plan fallback](#chatgpt-plan-fallback). |
| `CHATGPT_CREDENTIALS_KEY` | required if enabled | 32 random bytes, base64 (`openssl rand -base64 32`). Encrypts the stored OAuth tokens. Keep it as a Coolify secret. |
| `CHATGPT_CREDENTIALS_PATH` | `/data/chatgpt/credentials.json` | Encrypted credential file. Must be on a persistent volume. |
| `CHATGPT_AUTH_METHOD` | `official_oauth` | The only accepted value. |
| `CHATGPT_PROVIDER` | `responses` | The only accepted value. `codex_app_server` is refused at startup ([why](#why-not-codex-app-server)). |
| `CHATGPT_MODEL` | `auto` | `auto` uses the first model your account lists (re-read hourly), or set a slug from that list. |
| `CHATGPT_REQUEST_TIMEOUT_SECONDS` | `300` | Overall cap per fallback request, including the stream. |
| `CHATGPT_STREAM_IDLE_TIMEOUT_SECONDS` | `120` | Max gap between ChatGPT stream events. |
| `CHATGPT_LIMIT_PAUSE_SECONDS` | `300` | After a usage-limit error, fallback requests fail fast for this long without calling OpenAI. |
| `CHATGPT_DROP_UNSUPPORTED_SAMPLING` | `false` | `true` drops `temperature`, `top_p`, `seed` and the penalties on fallback instead of rejecting the request. |
| `OPENAI_API_BILLING_ENABLED` | `false` | Must be `false`. `true` makes OpenGate refuse to start. `OPENAI_API_KEY` is never read; if it is set, OpenGate logs a warning. |

The process refuses to start if a required value is missing or invalid. Error
messages name the variable, never its value. Blank values count as unset.
Commit only `.env.example`; `.env` is git- and docker-ignored.

Wake-on-LAN values are only read when `WOL_ENABLED=true`. Then any WOL value
that is set must be valid, or OpenGate refuses to start. If the MAC, or both
the broadcast and relay addresses, are missing, OpenGate starts with WOL
**disabled** and logs a warning. The MAC and network settings live only in the
environment, never in the source.

ChatGPT values are only read when `CHATGPT_FALLBACK_ENABLED=true`, and the
`opengate chatgpt …` commands always read them. Then they must be valid, or
OpenGate refuses to start. A missing or revoked *sign-in* never stops the
gateway, though: local inference keeps working, and fallback requests fail
closed with `subscription_not_configured` or `subscription_not_authorized`.

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

## ChatGPT plan fallback

Off by default. When enabled, a request that local Ollama cannot serve
(after Wake-on-LAN, if enabled) is sent to **your ChatGPT plan**, using
OpenAI's documented *Sign in with ChatGPT → ChatGPT plan usage* flow for
open-source clients:

- [Overview](https://developers.openai.com/siwc/token-sharing-open-source)
- [Registration and sign-in](https://developers.openai.com/siwc/token-sharing-open-source/sign-in)
- [Accounts and sessions](https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions)
- [Models and inference](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference)
- [Self-hosted VMs](https://developers.openai.com/siwc/token-sharing-open-source/self-hosted-vms)
- [Errors and recovery](https://developers.openai.com/siwc/token-sharing-open-source/errors-and-recovery)
- [Preview limitations](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations)

How it works:

1. You sign in once, interactively. OpenGate registers itself with
   `client_id=dynamic_agent_client`, `agent_name_hint=OpenGate` and a stable
   per-host `ext_agent_host_id`. It uses PKCE and a `127.0.0.1` loopback
   callback, and you approve the requested scopes in your browser. OpenAI
   returns an issued `client_id` (`oaiapp_…`), which is saved and reused.
2. OpenGate validates the ID token against OpenAI's JWKS (issuer, audience,
   expiry, nonce). It enables plan usage **only if** the granted scopes include
   `chatgpt.tokens.use.direct`: signing in alone does not grant inference.
3. On fallback, OpenGate sends the OAuth access token as a bearer token to
   `POST https://api.openai.com/v1/responses` with `store: false` and
   `stream: true`, and translates the result back into a Chat Completion.
   Usage counts against your ChatGPT plan. No client secret, no partner key and
   no API key exist anywhere in this flow.
4. Tokens are refreshed automatically before they expire, and are revoked when
   you sign out.

### Requirements and eligibility

What the official docs say, as of 2026-11-02:

- **Who can use it:** "Eligible ChatGPT **Plus and Pro** users". A workspace or
  policy can still make an account ineligible (`subscription_not_authorized`,
  upstream code `subscription_sharing_user_not_eligible`).
- **Which apps:** "ChatGPT plan usage is available to all open-source partners
  and selected private clients." The open-source flow uses **self-service
  dynamic registration**: there is no client ID to apply for. The "Request a
  client ID" waitlist is for the commercial website and plugin sign-in, which
  OpenGate does not use. The docs scope this flow to "open-source and locally
  hosted apps". A *paid or remotely hosted* app needs OpenAI's
  [interest form](https://openai.com/form/sign-in-with-chatgpt-interest/)
  instead.
- **How OpenGate fits:** it is a self-hosted tool you run for yourself, on your
  own VM. The docs' *Self-hosted VMs* page covers exactly that case. Two things
  are your call, and the docs do not settle them:
  - **The repository is currently private and unlicensed.** If you want
    OpenGate clearly inside the "open-source" framing, publish it under an
    open-source license.
  - **OpenGate re-exposes your plan to anything holding `OPENGATE_API_KEY`.**
    Keep that key to your own apps and agents. Giving it to other people would
    mean sharing your ChatGPT plan.
- **Preview:** OpenAI labels this flow a preview, with its own limitations (see
  [Supported requests](#supported-requests-on-the-chatgpt-plan)). Behaviour may
  change.
- OpenAI's help-center article
  [Using your ChatGPT plan in other apps and sites](https://help.openai.com/en/articles/20001542-using-your-chatgpt-plan-in-other-apps-and-sites)
  blocks automated fetching (HTTP 403), so it was not reviewed for this
  implementation. Read it before enabling the fallback.

**Status:** the code follows the documented flow and passes all tests against
mocks of OpenAI's services. It has **not yet been verified against a real
ChatGPT account.** It counts as working only once
[`opengate chatgpt test`](#end-to-end-fallback-test) succeeds for your account.

### Why not Codex app-server

The docs also describe driving `codex app-server` with the same OAuth token.
OpenGate calls the Responses API directly instead, because that is the
documented core of this flow and it fits a chat gateway better:

- Codex app-server is marked `[experimental]` in the CLI. It is a coding-agent
  harness: it wraps every turn in its own agent instructions, and it can run
  shell and MCP tools and child agents, all of which would have to be disabled
  and kept disabled.
- It must be **restarted on every hourly token renewal**, then `thread/resume`d.
- It would add the Codex runtime to the image.

The direct route has none of these: no agent and no tools, nothing to sandbox,
in-process token refresh with no restarts, and a small image.
`CHATGPT_PROVIDER=codex_app_server` is refused at startup rather than ignored.

### Supported requests on the ChatGPT plan

OpenGate's normal validation applies first: tools, images, `response_format`
and similar are already rejected for every provider. On fallback:

| Request feature | On the ChatGPT plan |
|---|---|
| `messages` (system, developer, user, assistant; text) | Supported. System prompts are sent as `developer` messages, since this route rejects system items. The full history is sent on every request (`store: false`, no `previous_response_id`). |
| `stream: true` / `false` | Supported. OpenAI always streams; for `stream: false`, OpenGate collects the stream and returns one `chat.completion`. Partial text is never returned as a success. |
| `stream_options.include_usage` | Supported (`input_tokens`/`output_tokens` mapped to `prompt_tokens`/`completion_tokens`). |
| `n: 1`, `user`, `tools: []`, `tool_choice: "none"`, `parallel_tool_calls` | Accepted no-ops, as for Ollama. |
| `temperature`, `top_p`, `presence_penalty`, `frequency_penalty`, `seed` | **Rejected** with `unsupported_request` (this route does not accept them), unless `CHATGPT_DROP_UNSUPPORTED_SAMPLING=true`, which drops them. |
| `max_tokens` / `max_completion_tokens`, `stop` | **Always rejected** on fallback. The route has no `max_output_tokens` and no stop sequences, and dropping them would silently change the output. |
| Response `id` | OpenAI's response ID (`resp_…`), preserved. |
| `model` in responses | Always the virtual model (`opengate/local`). The `x-opengate-provider` header says who answered. |
| `finish_reason` | `stop` (`response.completed`), `length`/`content_filter` (`response.incomplete`). Failures are errors, never a finish reason. |
| Refusals | Returned in `message.refusal` / `delta.refusal`. |

These parameters are rejected only on fallback: with Ollama up, they work as
before.

### Sign in (first time, headless Coolify, from your Mac)

Sign-in runs only as a command inside the OpenGate container. OpenGate's HTTP
port has **no** sign-in endpoints, so only someone with shell access to the
container (Coolify login or SSH to VM 104) can connect or disconnect an
account.

1. Deploy with `CHATGPT_FALLBACK_ENABLED=true` and `CHATGPT_CREDENTIALS_KEY` set
   (see [Deploy on Coolify](#deploy-on-coolify-vm-104)).
2. Open a shell in the container. In Coolify use the resource's **Terminal**
   tab, or from VM 104:
   `docker exec -it <opengate-container> sh`.
3. Choose how the browser's callback gets back to the container.

   **A. Paste the URL (no tunnel).** In the container:

   ```sh
   opengate chatgpt login
   ```

   Open the printed URL in your Mac's browser, sign in, and approve
   **using your ChatGPT plan**. The browser is then sent to
   `http://127.0.0.1:1455/auth/callback?code=…`, which will not load on the
   Mac. That is expected. Copy the **whole URL** from the address bar, paste it
   into the container terminal and press Enter. The code is single-use, bound
   to this attempt's PKCE verifier, and expires quickly, so paste it right away.

   **B. SSH tunnel (callback completes by itself).** On VM 104, find the
   container's IP:

   ```sh
   docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}' <opengate-container>
   ```

   On the Mac, forward the callback port to it, and leave this running:

   ```sh
   ssh -N -L 1455:<container-ip>:1455 <user>@<vm-104>
   ```

   In the container:

   ```sh
   opengate chatgpt login --listen 0.0.0.0:1455
   ```

   Open the URL on the Mac. After you approve, the browser reaches the
   container through the tunnel and the terminal finishes by itself. Nothing
   else should be listening on port 1455 on the Mac (a running Codex CLI login
   would be).

4. The command checks the granted scopes. If you declined plan usage, it says
   so and the fallback stays off. Run `opengate chatgpt login --reconsent` to
   grant it.
5. Run `opengate chatgpt test`. This is the proof that it works (see below).
   The gateway picks up the new credentials **without a restart**.

Other commands: `opengate chatgpt status` (account, scopes, expiry; never
tokens), `opengate chatgpt logout`, `opengate chatgpt init` (creates the file
and host ID without signing in), and `login --new-account` (replaces the saved
account with a different one).

Signing in runs *inside* the container, with the container's own host ID, so
no credential file is ever copied between machines. That avoids the docs'
caveat about transferred sessions. Never copy a Codex CLI `auth.json` or
browser cookies into OpenGate: they will not work and are not authorized.

### Usage limits

- Requests count against your ChatGPT plan. For **Plus**, the five-hour usage
  limit is *shared* across every app that uses your plan; **Pro** has no
  five-hour limit. In
  [ChatGPT Settings → Usage](https://chatgpt.com/settings/usage) you can set
  OpenGate's own weekly limit (10–100%) and whether it may use **ChatGPT
  credits** after the plan limit is reached. That setting lives at OpenAI, and
  OpenGate can neither see nor change it. For a hard stop, leave credits off
  for OpenGate. (These are ChatGPT credits, not API billing.)
- When a limit is reached (HTTP 429 or mid-stream
  `subscription_sharing_usage_limit_exceeded`), OpenGate returns 429
  `subscription_limit_reached`. It pauses all fallback requests for
  `CHATGPT_LIMIT_PAUSE_SECONDS` without contacting OpenAI, and never retries
  on anything else. It does not guess a reset time, because OpenAI's code does
  not say whether the plan limit or the app limit was hit.
- Local inference is unaffected. As soon as Ollama is up, requests are served
  locally again.

### Credential storage and security

- **Storage:** `/data/chatgpt/credentials.json` on the named volume
  `opengate-data`. The file is sealed with XChaCha20-Poly1305 under
  `CHATGPT_CREDENTIALS_KEY`, written atomically, mode `0600` in a `0700`
  directory, owned by uid 10001. The container keeps its read-only root
  filesystem, no capabilities and `no-new-privileges`. It holds the issued
  client ID, the verified account (`sub`, email), the granted scopes, the
  access/refresh/ID tokens, and this host's ID.
- **Nothing in the image or Git:** the image contains no credentials, and the
  key is a Coolify secret. A copy of the volume alone is useless. A wrong key
  is reported as `subscription_not_authorized` ("cannot be read"), and the file
  is left untouched.
- **Never exposed:** tokens are not logged (a test proves this at TRACE),
  never returned to API callers, never put in URLs. The ID token is not sent
  as `id_token_hint`, because the authorization URL is printed to a terminal.
  HTTP redirects are refused, and proxies are ignored.
- **Restarts and redeploys** keep the volume, so they keep the sign-in.
  Changing `CHATGPT_CREDENTIALS_KEY` means signing in again: run
  `opengate chatgpt logout` first, delete the file, then sign in with the new
  key.
- **If credentials are compromised:** disconnect OpenGate in ChatGPT
  Settings, run `opengate chatgpt logout`, rotate `CHATGPT_CREDENTIALS_KEY`,
  and sign in again.

**Token lifecycle:**

- Access tokens last one hour and are refreshed five minutes before expiry.
  Refreshes are serialized, so one burst means one refresh, and the rotated
  refresh token is saved at once.
- A 401 triggers one refresh and one retry, safe because nothing has been
  streamed yet.
- Refresh tokens last 30 days and renew with every refresh.
- A temporary outage of OpenAI's auth service keeps the still-valid token and
  retries after 30 s.
- An expired refresh token becomes `subscription_expired`. A revoked or reused
  one, or a disconnect in ChatGPT settings (OpenAI does not notify apps of
  this), becomes `subscription_not_authorized`. In both cases the tokens are
  cleared and the client ID kept, so `opengate chatgpt login` reconnects the
  same registration. All of this happens without restarting the gateway.

**Sign out:** `opengate chatgpt logout` revokes the refresh token at OpenAI's
`revocation_endpoint`, with retries. It then deletes the access, refresh and
ID tokens, and keeps the client ID and host ID for a later sign-in. If
revocation cannot be confirmed, it says so: then disconnect OpenGate in
ChatGPT Settings yourself.

### Health

`GET /health/providers` (no auth) reports each provider separately:

```json
{"gateway":{"status":"ok"},
 "ollama":{"status":"ready","wake_on_lan":true},
 "chatgpt":{"status":"authorized","fallback_enabled":true}}
```

- **`ollama`** is `ready`, `upstream_unreachable`, `upstream_timeout`,
  `model_not_installed` or `upstream_error`. The probe never wakes the PC.
- **`chatgpt`** is `disabled`, `authorized`, `subscription_not_configured`,
  `subscription_not_authorized`, `subscription_expired`,
  `subscription_limit_reached` (while paused) or
  `subscription_provider_unavailable` (failed within the last minute). It is
  computed from the stored session: it never calls OpenAI, so it never spends
  plan usage. That also means `authorized` is a statement about the stored
  session, not proof that inference works. Only `opengate chatgpt test` proves
  that.

The container healthcheck still uses `/health/live`, so a missing sign-in or
a sleeping PC never makes Coolify restart the gateway.

### End-to-end fallback test

1. **Real inference through your plan.** In the container:

   ```sh
   opengate chatgpt test
   ```

   Expect `OK: response resp_… finished with "stop"` and the reply. This uses a
   little plan usage. If it fails, it prints the state code (table above).
   Then check [ChatGPT Settings → Usage](https://chatgpt.com/settings/usage):
   OpenGate should appear there.
2. **Local first.** With the PC awake, send a [smoke test](#smoke-tests)
   request with `-i`. Expect `x-opengate-provider: ollama`, and no `ChatGPT`
   lines in the logs.
3. **Wake still preferred.** Put the PC to sleep and send a request. The logs
   should show `sent Wake-on-LAN magic packet`, and if the PC boots within
   `WOL_STARTUP_TIMEOUT_SECONDS`, the reply has `x-opengate-provider: ollama`.
4. **Fallback.** Keep the PC from waking (unplug its network, or temporarily
   set `WOL_STARTUP_TIMEOUT_SECONDS=10`). Send a request. Expect the wake
   attempt, then `local inference unavailable; trying the ChatGPT plan
   fallback`, `serving request from the ChatGPT plan`, and
   `x-opengate-provider: chatgpt`. Try `"stream": true` too.
5. **Back to local.** Wake the PC. The next request is `ollama` again.
6. **Revocation.** Disconnect OpenGate in ChatGPT Settings and repeat step 4.
   Expect 503 `all_providers_unavailable` with
   `providers.chatgpt = subscription_not_authorized`. Sign in again to restore
   it.

```bash
docker logs -f <opengate-container> 2>&1 | grep -E 'wol|Wake|ChatGPT|chat/completions'
```

### Troubleshooting

| Symptom | Cause and fix |
|---|---|
| Startup error naming `CHATGPT_CREDENTIALS_KEY` | Missing or not 32 bytes of base64. Generate with `openssl rand -base64 32`. |
| `subscription_not_configured` | Never signed in on this volume. Run `opengate chatgpt login`. |
| `subscription_not_authorized … cannot be read` | The key changed, or the file was modified. Restore the old key, or log out, delete the file and sign in again. |
| `subscription_not_authorized … not granted` | Plan usage was declined at consent. Run `opengate chatgpt login --reconsent`. |
| `subscription_not_authorized … revoked or disconnected` | Disconnected in ChatGPT settings, or the session was revoked. Sign in again. |
| `subscription_not_authorized (subscription_sharing_user_not_eligible …)` | The account, plan or workspace is not eligible (Plus/Pro only; workspace policy). Signing in again will not help. |
| `subscription_expired` | Unused for 30 days. Sign in again. |
| `subscription_limit_reached` | Plan or app limit. See [Usage limits](#usage-limits). |
| `unsupported_request`, `param: temperature` | The client sends sampling parameters. Remove them, or set `CHATGPT_DROP_UNSUPPORTED_SAMPLING=true`. |
| `subscription_provider_unavailable` | OpenAI or the network is down. From VM 104, check `docker exec <c> opengate chatgpt status` and outbound HTTPS to `auth.openai.com` / `api.openai.com`. |
| `opengate chatgpt login`: `cannot listen on …` | Port 1455 is in use in the container (a previous login is still running). Use `--port 1456` (the redirect may use any port), or stop the other one. |
| Browser shows "can't connect to 127.0.0.1:1455" | Expected without a tunnel. Paste the full URL into the terminal (method A). |
| `callback state does not match` | The pasted URL is from an older attempt. Start `login` again and use the newest URL. |
| `CHATGPT_MODEL=auto` picks a model you don't want | Set `CHATGPT_MODEL` to a slug from your account's list. A wrong slug fails with `unsupported_request` or `subscription_provider_unavailable`. |


## Development (Ubuntu VM)

### Secret guard (do this first in every clone)

```bash
git config core.hooksPath .githooks
```

This turns on `.githooks/pre-commit` and `.githooks/pre-push`. Both run
`.githooks/scan-secrets.sh` and refuse the commit or push if it adds:

- a secret-bearing file: `.env` or `.env.*` (anything except `.env.example`),
  `*.pem`/`*.key`/`*.p12`, SSH keys, `credentials*.json`;
- any value from your local `.env`, including the host of URL values and the
  first octets of `WOL_MAC_ADDRESS`. Values that also appear in `.env.example`
  are public defaults and are allowed;
- a known credential format: private keys, OpenAI/Anthropic/GitHub/AWS/Slack/Google
  tokens, JWTs, URLs with passwords, long literal secrets assigned to
  key/token/password names.

The pre-push hook rescans every commit being pushed, so a `git commit --no-verify`
is still caught. Findings show the file, line and kind, never the value. Real
LAN details (IPs, MACs, hostnames) belong in `.env`, never in docs or code; use
`192.168.1.50`/`AA:BB:CC:DD:EE:64`-style placeholders. For a reviewed false
positive, put `opengate:allow-secret` on that line or add it to
`.githooks/secret-allowlist`. Scan by hand with `.githooks/scan-secrets.sh staged`
or `.githooks/scan-secrets.sh range origin/main..HEAD`.

### Build and test

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

`tests/fallback.rs` puts a real OpenGate in front of the mock PC and a mock of
OpenAI's sign-in and Responses services. It records every bearer token the
mock receives. It covers:
- Ollama online: no fallback, and OpenAI is never contacted
- Ollama offline: WOL is attempted, and a successful wake uses Ollama
- WOL timeout: fallback, with the request contract checked (`store:false`,
  `stream:true`, `developer` role, no extra fields)
- streaming translation, including usage
- missing, signed-out and scope-less credentials
- revoked sessions: tokens cleared, and the error sticks
- expired sessions
- a 401 → one refresh → success
- a temporary refresh outage keeping the still-valid token
- usage limit over HTTP and mid-stream: 429, a pause, no retry, no other
  credential ever sent
- `OPENAI_API_KEY` in the environment never used
- unsupported parameters
- invalid client keys triggering neither WOL nor OpenAI
- six concurrent fallbacks sharing one wake and one refresh
- client cancellation stopping the ChatGPT stream
- persisted credentials surviving a restart, and the wrong key failing safely
- the cloud being down while local keeps working
- Ollama 4xx and mid-stream failures never falling back
- credentials written by `login` picked up without a restart

`tests/chatgpt_login.rs` runs the sign-in flow against the mock authorization
server: dynamic registration, PKCE, ID-token signature, issuer, audience and
nonce, declined consent, a declined plan scope, and revocation.
`tests/logging.rs` also proves that no token, prompt or reply is ever logged
on the ChatGPT path.

Before committing:

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test && cargo build --release
docker build -t opengate:local .
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
cp .env.example .env   # then set OPENGATE_API_KEY and OLLAMA_BASE_URL
docker compose up --build -d
docker compose logs -f opengate
```

By default it publishes on `127.0.0.1:8100` only (`OPENGATE_PUBLISH_ADDRESS`,
`OPENGATE_PUBLISH_PORT`); inside the container it listens on 8000. The container runs as
uid 10001 with a read-only root filesystem, all capabilities dropped and
`no-new-privileges`. Test the **container's** path to Ollama, not just the host's:

```bash
curl -s --max-time 10 http://127.0.0.1:8100/health/ready   # probe runs from inside the container
```

For Wake-on-LAN in Docker, also start the relay, and set `WOL_ENABLED=true` and
`WOL_RELAY_ADDRESS=host.docker.internal:9009` in `.env`:

```bash
docker compose -f compose.wol-relay.yaml up -d --build
docker compose up --build -d
```

### Smoke tests

```bash
set -a; . ./.env; set +a
B=http://127.0.0.1:8100        # Docker; use :8000 for `cargo run`, or the Coolify URL

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

   **Ports.** Coolify's own dashboard already uses host port **8000** on this
   server, so OpenGate must never be published there. The container listens
   on 8000 internally, and compose publishes it on host port
   `OPENGATE_PUBLISH_PORT` (default `8100`). To reach it straight from the LAN
   (e.g. from your Mac), also set:

   ```
   OPENGATE_PUBLISH_ADDRESS=<VM 104's LAN IP>
   OPENGATE_PUBLISH_PORT=8100
   ```
   Left unset, it is published on the server's loopback only, and only the
   domain from step 3 reaches it.

   For Wake-on-LAN, also set:

   ```
   WOL_ENABLED=true
   WOL_MAC_ADDRESS=AA:BB:CC:DD:EE:64
   WOL_BROADCAST_ADDRESS=192.168.1.255
   WOL_RELAY_ADDRESS=host.docker.internal:9009
   ```
   For the ChatGPT plan fallback, also set (`CHATGPT_CREDENTIALS_KEY` as a
   **secret**, generated once with `openssl rand -base64 32` and kept in your
   password manager):

   ```
   CHATGPT_FALLBACK_ENABLED=true
   CHATGPT_CREDENTIALS_KEY=<secret>
   CHATGPT_MODEL=auto
   ```
   `compose.yaml` mounts the named volume `opengate-data` at `/data`, where the
   encrypted credentials live. Coolify keeps named volumes across restarts and
   redeploys. Check that it appears under the resource's **Storages**, and never
   delete it, or you will have to sign in again.
3. Optional. Under **Domains**, add a domain for service `opengate` with the internal
   port suffix, e.g. `https://opengate.<your-domain>:8000`. Here `:8000` only
   tells Coolify's proxy which **container** port to forward to; clients use
   `https://opengate.<your-domain>` on 443, and it does not touch the dashboard's
   host port 8000. The name must resolve to VM 104 (DNS or `/etc/hosts`). Keep it reachable
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
   - With the fallback enabled: the startup log shows `chatgpt: Some(ChatGptConfig { … credentials_key: <redacted> … })`
     and `/health/providers` shows `"chatgpt":{"status":"subscription_not_configured"…}`
     until you [sign in](#sign-in-first-time-headless-coolify-from-your-mac), then `authorized`.
6. **Sign in** once, then run `opengate chatgpt test`. See
   [End-to-end fallback test](#end-to-end-fallback-test).

### Test from your Mac

Run the [smoke tests](#smoke-tests) from the Mac with `B` pointing at the
deployment and the **Coolify** API key (not your dev one):

```bash
export OPENGATE_API_KEY='<Coolify value>'
B=http://<VM 104's LAN IP>:8100      # direct, needs OPENGATE_PUBLISH_ADDRESS
# B=https://opengate.<your-domain>   # via Coolify's proxy, if you added a domain
curl -i $B/health/providers
```

`/health/ready` must return 200 (Coolify's container can reach Ollama), and a
chat request should come back with `x-opengate-provider: ollama`. If the
connection is refused, check `docker ps` on VM 104 shows
`<VM 104's LAN IP>:8100->8000/tcp` for the OpenGate container; if it hangs, a
firewall on VM 104 (ufw) or between subnets is blocking 8100.

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

## Using OpenGate from OpenClaw (optional)

OpenClaw is **not required**: OpenGate now does the ChatGPT plan fallback
itself, so any OpenAI-compatible client gets it. If you use OpenClaw as a
client, verify each step against the **installed** OpenClaw release; these are
not drop-in values.

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
           // No `fallbacks` needed: OpenGate falls back to your ChatGPT plan itself.
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

3. **Fallback.** Leave OpenClaw's own fallback out, or two layers will each
   try to recover from the same failure. OpenGate's fallback rejects
   `temperature`, `top_p` and `max_tokens` by default. If OpenClaw always sends
   them, either configure the model without them, or set
   `CHATGPT_DROP_UNSUPPORTED_SAMPLING=true` (this drops the sampling hints
   only; `max_tokens` is still rejected).

## Out of scope

No tev1 or other routing model, no complexity scoring or prompt-based
escalation. No OpenAI API keys, Platform credits or API billing. No Codex
app-server, browser cookies, scraped ChatGPT endpoints or copied Codex `auth.json`.
No database, no dashboard, one local model only.
