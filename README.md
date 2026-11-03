# OpenGate

A small, authenticated, OpenAI-compatible gateway that is **local first**. Every
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
| `messages` | Roles `system`, `developer` (sent as `system`), `user`, `assistant`, `tool`. Content is a string or text parts (joined with newlines); assistant content may be `null` when it has `tool_calls`. Message `name` is accepted and dropped. An earlier answer's `reasoning_content`/`reasoning`/`refusal` is accepted and not forwarded. |
| `tools` | Function tools: `name` (`[A-Za-z0-9_-]{1,64}`, unique), `description`, `parameters` (a JSON Schema object; missing = no parameters). At most 128. `strict: true` is rejected (neither provider can guarantee schema-exact arguments); `strict: false` is accepted. |
| `tool_choice` | `auto` (default), `none` (tools are not sent to the model), `required`, or `{"type":"function","function":{"name":…}}`. See [Tool calling](#tool-calling). |
| `parallel_tool_calls` | `true`/`false`. See [Tool calling](#tool-calling). |
| assistant `tool_calls`, `tool` messages | Tool-call history: `{id, type:"function", function:{name, arguments}}` (arguments a JSON object as a string) and `{role:"tool", tool_call_id, content}`. Every tool call must be answered by a tool message with its ID before the next non-tool message, as OpenAI requires; IDs must be unique. |
| `stream` | `true` for SSE. |
| `stream_options` | `{"include_usage": bool}` (`include_obfuscation` is accepted and ignored). |
| `temperature` (0–2), `top_p` (0–1), `presence_penalty`, `frequency_penalty` (−2–2), `seed`, `stop` (string or ≤4 strings) | Passed to Ollama as `options`. |
| `max_tokens` / `max_completion_tokens` | Either one, sent to Ollama as `options.num_predict`. |
| `response_format` | `text` (no-op), `json_object` (Ollama `format: "json"`), `json_schema` (the schema as Ollama `format`). Not combinable with tools on Ollama (400). |
| `n` | Only `1`. |
| `user`, `safety_identifier`, `prompt_cache_key`, `metadata`, `store: false` | Accepted and dropped: they identify or cache requests and cannot change the answer. `store: true` is rejected (OpenGate never stores). |
| `functions` / `function_call` (deprecated) | Only `[]` / `"none"`; anything else is rejected — use `tools`. |

Everything else is **rejected with 400**, naming the parameter: images,
`logprobs`, `reasoning_effort`, `logit_bias`, `audio`, `modalities`, and so on.
Nothing that could change the answer is silently dropped.

### Tool calling

OpenGate **never executes tools**. It passes tool definitions to the model,
returns the model's tool calls to the client (OpenClaw runs them), and passes
the tool results back on the next request.

- **Responses.** A non-streaming answer has `message.tool_calls`
  (`content: null` when there is no text) and `finish_reason: "tool_calls"`.
  Arguments are always a JSON object serialized as a string. Several calls in
  one answer are supported.
- **IDs.** Ollama's own call ID is kept; when it does not send one, OpenGate
  generates `call_<24 hex>`. An ID never changes within a response, and
  history IDs are passed back to the model unchanged.
- **`tool_choice` on Ollama.** Ollama has no `tool_choice`, so OpenGate
  emulates it: `none` sends no tools; a forced function sends only that tool;
  `required`, a forced function and `parallel_tool_calls: false` add one short
  system instruction. The answer is then **checked**: a text-only answer to a
  forced choice, a call to another tool, a call when none were offered, or a
  second call with `parallel_tool_calls: false` is an error
  (`tool_choice_not_satisfied`), never passed off as compliant.
- **Malformed calls** (no function name, arguments that are not a JSON object)
  are an error (`upstream_invalid_tool_call`), never repaired or invented.
  A call to a tool name the request did not define is passed through as is;
  OpenClaw answers it with a "tool not found" result the model can recover from.
- **Models without tool support.** If `OLLAMA_MODEL` cannot do tool calling,
  Ollama refuses and the client gets `400 tools_not_supported`. Small models
  such as `llama3.1:8b` do support tools, but choose and fill them less
  reliably than large hosted models; for heavy agent use, set `OLLAMA_MODEL`
  to a stronger tools-capable model (no code change needed).

### Context window

OpenGate talks to Ollama's native `/api/chat` and sends `truncate: false` and
`shift: false` with every request, so a prompt that does not fit is
**refused** instead of having its history silently cut (which made an agent
answer a different conversation than the one it sent). The client gets:

```json
{"error":{"code":"context_length_exceeded","type":"invalid_request_error","param":null,
  "message":"input exceeds the 32768-token context window of the local model; shorten or compact the conversation, or raise OLLAMA_NUM_CTX if memory allows (Ollama: …)"}}
```

The wording matches OpenClaw's context-overflow detection, so OpenClaw can
compact and retry. It is a 400: the same request would not fit any better a
second time, and it never triggers the ChatGPT fallback.

The window is whatever Ollama actually runs with, not the model's advertised
maximum (llama3.1 advertises 128k):

- Set **`OLLAMA_NUM_CTX`** (e.g. `32768`) and OpenGate sends it as
  `options.num_ctx` with every request, independent of the PC's Ollama
  settings. Unset, Ollama decides (`OLLAMA_CONTEXT_LENGTH` on the PC, the
  Modelfile, or its default, which is small and varies by version and VRAM).
- A larger window needs more memory for the KV cache: for `llama3.1:8b`
  roughly 128 KiB per token at f16, so 32k tokens ≈ 4 GiB on top of the
  ~5 GiB of weights. If it does not fit in VRAM, Ollama splits the model with
  the CPU and gets much slower. Check after one request, on the PC:
  `ollama ps` (columns `CONTEXT` and `PROCESSOR`, which should say `100% GPU`),
  or remotely: `curl $B/health/providers` (`ollama.context_length` is what
  Ollama allocated for the loaded model, `ollama.num_ctx` what OpenGate asks for).
- Set OpenClaw's `contextWindow` to the **same** number. OpenClaw budgets its
  prompt and `max_completion_tokens` from it; if it believes in more room than
  Ollama has, requests fail with `context_length_exceeded`.
- `truncate`/`shift` need a recent Ollama. An older one ignores them and
  truncates silently as before; update Ollama on the PC if long conversations
  seem to "forget" their beginning without any error.

### Errors

OpenAI-shaped: `{"error":{"message","type","code","param"}}`. Messages never
include prompt content.

| Status | `code` | When |
|---|---|---|
| 400 | `invalid_json`, `invalid_request`, `unsupported_parameter`, `model_not_supported` | Caller error. OpenClaw treats 400 as terminal (no fallback). |
| 400 | `context_length_exceeded` | The prompt does not fit the local model's context window (see [Context window](#context-window)). |
| 400 | `tools_not_supported` | The request has tools but `OLLAMA_MODEL` cannot do tool calling. |
| 401 | `invalid_api_key` | Missing or wrong key. |
| 413 | `request_too_large` | Body over 4 MiB. |
| 502 | `upstream_error`, `upstream_invalid_response` | Ollama returned an error or garbage. |
| 502 | `upstream_invalid_tool_call` | The model produced a malformed tool call (no name, arguments not a JSON object). |
| 502 | `tool_choice_not_satisfied` | The model's answer broke `tool_choice` or `parallel_tool_calls: false` (see [Tool calling](#tool-calling)). |
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
| `unsupported_request` | The request uses something the ChatGPT plan route cannot do, e.g. `temperature` or `max_tokens` (named in `param`), or the route refused a tool (`subscription_sharing_unsupported_capability`). | See [Supported requests](#supported-requests-on-the-chatgpt-plan). |

### Streaming

Ollama's native stream is translated to standard `chat.completion.chunk`
frames (`data: {...}\n\n`) as it arrives, all with one `id` and `model` set to
the virtual id:

1. `{"role":"assistant","content":""}`, sent as soon as Ollama accepts the request;
2. text deltas (`content`), and `reasoning_content` for thinking models;
3. per tool call, an opening delta `{"index":i,"id":…,"type":"function","function":{"name":…,"arguments":""}}`
   then its arguments (`{"index":i,"function":{"arguments":"{…}"}}`).
   Concatenating the argument fragments of an index gives the JSON object the
   model produced. `index` counts from 0 within the response;
4. one final chunk with an empty delta and `finish_reason` (`stop`,
   `tool_calls` or `length`) — exactly one;
5. with `stream_options.include_usage`, a usage chunk (`choices: []`,
   `usage: {prompt_tokens, completion_tokens, total_tokens}` from Ollama's counts);
6. `data: [DONE]`.

`data: [DONE]` is sent **only** after Ollama's final chunk. If the stream fails
after it has started (upstream error, context overflow, idle timeout, overall
timeout, EOF before the end, malformed data, an answer that breaks
`tool_choice`), OpenGate sends one error frame, `data: {"error":{…}}`, and ends
the stream **without** `[DONE]`. If the client disconnects, the upstream request
is dropped and Ollama stops generating.

The same rules apply to a ChatGPT plan stream. OpenAI's events are translated
to `chat.completion.chunk` frames (function calls become the same `tool_calls`
deltas, with arguments streamed in OpenAI's fragments), and `[DONE]` follows only
`response.completed` (or `response.incomplete`, sent with `finish_reason:
"length"`). A usage limit or error mid-stream ends the stream with one error
frame and no `[DONE]`. Once any output has been sent, OpenGate never retries
on another provider.

## Configuration

| Variable | Default | Purpose |
|---|---|---|
| `OPENGATE_API_KEY` | **required** | ≥32 chars, no whitespace. Generate with `openssl rand -base64 48 \| tr -d '\n=+/'`. |
| `OLLAMA_BASE_URL` | **required** | e.g. `http://192.168.1.50:11434`. Plain http(s), no credentials or query. |
| `OLLAMA_MODEL` | **required** | Installed Ollama model, e.g. `llama3.1:8b`. Change it (and redeploy) to use another model; no code change. For tool calling it must be a tools-capable model. |
| `OLLAMA_NUM_CTX` | — (Ollama decides) | 512–1048576. Context window sent to Ollama with every request (`options.num_ctx`). See [Context window](#context-window). |
| `OPENGATE_VIRTUAL_MODEL` | `opengate/local` | The only model id clients may send. |
| `OPENGATE_HOST` | `0.0.0.0` | Listen address. |
| `OPENGATE_PORT` | `8000` | Listen port. |
| `OPENGATE_PUBLISH_ADDRESS`, `OPENGATE_PUBLISH_PORT` | **required** (compose), `8100` | Docker only: host address and port compose publishes the container on. Never `8000` on the Coolify server (its dashboard uses it). |
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
| `CHATGPT_DROP_MAX_TOKENS` | `false` | `true` drops `max_tokens`/`max_completion_tokens` on fallback instead of rejecting the request (the route has no output limit, so the answer may be longer). Needed for OpenClaw, which always sends a limit. |
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

OpenGate's normal validation applies first: images, `logprobs` and similar are
already rejected for every provider. On fallback:

| Request feature | On the ChatGPT plan |
|---|---|
| `messages` (system, developer, user, assistant; text) | Supported. System prompts are sent as `developer` messages, since this route rejects system items. The full history is sent on every request (`store: false`, no `previous_response_id`). |
| `stream: true` / `false` | Supported. OpenAI always streams; for `stream: false`, OpenGate collects the stream and returns one `chat.completion`. Partial text is never returned as a success. |
| `stream_options.include_usage` | Supported (`input_tokens`/`output_tokens` mapped to `prompt_tokens`/`completion_tokens`). |
| `tools`, `tool_choice`, `parallel_tool_calls` | Sent as Responses function tools, grouped in one namespace (`opengate`) as the Sign in with ChatGPT docs require for this route, with `strict: false`. `tool_choice` maps to `auto`/`none`/`required`/`{"type":"function","name":…}`. Function calls come back as `tool_calls` (IDs are OpenAI's `call_…`), `finish_reason: "tool_calls"`. **Not yet verified against the live route** (only against a mock): if the plan route refuses a tool request, the client gets `unsupported_request`, never a fake success. |
| Tool history (assistant `tool_calls`, `tool` messages) | Sent as `function_call` / `function_call_output` input items with the client's call IDs, so a conversation started on Ollama continues here. Nothing has been sent to the client before the switch, so no tool call is duplicated. |
| `response_format` | Sent as `text.format` (`json_object` or `json_schema`). |
| `n: 1`, `user`, `prompt_cache_key`, `store: false` | Accepted no-ops, as for Ollama. |
| `temperature`, `top_p`, `presence_penalty`, `frequency_penalty`, `seed` | **Rejected** with `unsupported_request` (this route does not accept them), unless `CHATGPT_DROP_UNSUPPORTED_SAMPLING=true`, which drops them. |
| `max_tokens` / `max_completion_tokens` | **Rejected** on fallback: the route has no `max_output_tokens`, and dropping the limit would silently change the output — unless you opt in with `CHATGPT_DROP_MAX_TOKENS=true`. |
| `stop` | **Always rejected** on fallback (no stop sequences on this route). |
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
 "ollama":{"status":"ready","wake_on_lan":true,"num_ctx":32768,"context_length":32768},
 "chatgpt":{"status":"authorized","fallback_enabled":true}}
```

- **`ollama`** is `ready`, `upstream_unreachable`, `upstream_timeout`,
  `model_not_installed` or `upstream_error`. The probe never wakes the PC.
  `num_ctx` is `OLLAMA_NUM_CTX` (null if unset); `context_length` is what
  Ollama allocated for the model if it is loaded right now (from `/api/ps`;
  null when unloaded or unreachable).
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

The tests (`cargo test`) run a real OpenGate against a mock of Ollama's native
API (`/api/chat`, NDJSON streams). `tests/gateway.rs` covers
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

`tests/openclaw.rs` sends the exact request shapes OpenClaw 2026.10.5's
`openai-completions` transport sends to a custom endpoint, through a real
OpenGate in front of a scripted mock of Ollama's native API, and checks what
reaches Ollama as well as what the client gets. It covers:
- a streamed agent turn with two tool calls: role chunk, opening deltas,
  argument reconstruction into valid JSON, kept and generated IDs, one
  `tool_calls` finish, one `[DONE]`; `truncate:false`, `num_ctx`, `num_predict`
- streamed text with the usage chunk; `length` finishes
- a non-streaming tool call (`content: null`, string arguments)
- tool results passed back (`tool_name`, `tool_call_id`) and a final answer,
  then a third multi-turn round
- out-of-order tool results rejected before Ollama is called
- forced `tool_choice` (only that tool sent; ignoring it is an error, streaming
  and not) and `tool_choice: "none"`
- malformed tool arguments and malformed stream lines from the model
- context overflow as a 400 (HTTP and mid-stream), and never falling back
- a model without tool support; Ollama unavailable with tools
- unsupported fields rejected before calling Ollama; harmless ones accepted
- auth required for tool requests; `/health/providers` context reporting
- ChatGPT plan fallback with tools: streamed and collected function calls,
  namespaced tool definitions, tool history as `function_call` items sent
  once, `max_tokens` refused unless `CHATGPT_DROP_MAX_TOKENS=true`

`tests/smoke.rs` holds optional smoke tests against a **real** deployment
(chat, stream, a tool round trip). They are `#[ignore]`d, need no credentials
in CI, and run only on request; they spend real inference and may wake the PC:

```bash
OPENGATE_SMOKE_BASE_URL=http://<VM-104-LAN-IP>:8100 OPENGATE_SMOKE_API_KEY="$OPENGATE_API_KEY" \
  cargo test --test smoke -- --ignored --test-threads=1 --nocapture
```

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
                       # (OPENGATE_PUBLISH_ADDRESS=127.0.0.1 is required for compose)
docker compose up --build -d
docker compose logs -f opengate
```

With `OPENGATE_PUBLISH_ADDRESS=127.0.0.1` it publishes on `127.0.0.1:8100`
only (`OPENGATE_PUBLISH_PORT`); inside the container it listens on 8000. The container runs as
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

# Tool calling: expect finish_reason "tool_calls" and message.tool_calls
curl -sS $B/v1/chat/completions -H "Authorization: Bearer $OPENGATE_API_KEY" \
  -H 'Content-Type: application/json' -d '{"model":"opengate/local",
  "messages":[{"role":"user","content":"What is the weather in Paris? Use the tool."}],
  "tools":[{"type":"function","function":{"name":"get_weather","description":"Current weather for a city",
    "parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}],
  "tool_choice":"auto"}'

# Streaming tool call: expect tool_calls deltas, one finish_reason, then [DONE]
curl -N $B/v1/chat/completions -H "Authorization: Bearer $OPENGATE_API_KEY" \
  -H 'Content-Type: application/json' -d '{"model":"opengate/local","stream":true,
  "stream_options":{"include_usage":true},
  "messages":[{"role":"user","content":"What is the weather in Paris? Use the tool."}],
  "tools":[{"type":"function","function":{"name":"get_weather",
    "parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}]}'

# Tool result round trip: the client (you) answers the call
curl -sS $B/v1/chat/completions -H "Authorization: Bearer $OPENGATE_API_KEY" \
  -H 'Content-Type: application/json' -d '{"model":"opengate/local","messages":[
  {"role":"user","content":"What is the weather in Paris?"},
  {"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function",
    "function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}]},
  {"role":"tool","tool_call_id":"call_1","content":"18C and sunny"}],
  "tools":[{"type":"function","function":{"name":"get_weather",
    "parameters":{"type":"object","properties":{"city":{"type":"string"}}}}}]}'

curl -sS $B/health/providers     # ollama.context_length: the real window, once the model is loaded

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
   `OPENGATE_PUBLISH_PORT` (default `8100`) at `OPENGATE_PUBLISH_ADDRESS`,
   which is **required**: Coolify will not deploy without it, so a redeploy
   can never silently move the API off the LAN. Set:

   ```
   OPENGATE_PUBLISH_ADDRESS=<VM 104's LAN IP>
   OPENGATE_PUBLISH_PORT=8100
   ```

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
   For OpenClaw (see [Using OpenGate from OpenClaw](#using-opengate-from-openclaw)),
   also set:

   ```
   OLLAMA_NUM_CTX=32768
   CHATGPT_DROP_MAX_TOKENS=true    # only if OpenClaw turns should use the fallback
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
   - A tool request returns `finish_reason: "tool_calls"` (see
     [Smoke tests](#smoke-tests)), and `/health/providers` shows
     `ollama.context_length` equal to `OLLAMA_NUM_CTX` after it.
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

## Automatic deployment

Every push to `main` deploys to Coolify without touching the dashboard:

```
push to main ──► GitHub
                   ▲  outbound HTTPS, once a minute: "what is main now?"
VM 104 ── opengate-autodeploy.timer
   │   new commit?
   └─► POST <Coolify Deploy Webhook> (LAN, Bearer token with `deploy` only)
          └─► Coolify: pull main ─► docker compose build ─► recreate opengate
                                                       └─► healthcheck /health/live
```

**Why not Coolify's GitHub webhook?** Both of Coolify's push mechanisms (the
GitHub App, and a manual repository webhook) need GitHub's servers to reach
Coolify over the Internet. Coolify here is on a private LAN, so that would
mean publishing the dashboard host, or a tunnel. Instead a small systemd timer
on VM 104 asks GitHub for the tip of `main` (outbound only; no API token, no
rate limit) and, when it changes, calls Coolify's own **Deploy Webhook** on the
LAN. Nothing is exposed, GitHub holds no secrets, and it works whatever source
the Coolify application uses (GitHub App, public URL or deploy key). Deploys
start within about a minute of the push. A self-hosted GitHub Actions runner
was ruled out because this repository is public, and GitHub advises against
self-hosted runners on public repositories (a fork's pull request could run
code on your LAN).

### One-time setup

**In Coolify** (dashboard on VM 104, port 8000):

1. Open the OpenGate application. On its **General** page, check **Source**:
   GitHub App, public repository, or deploy key all work, as long as
   **Branch** is `main`, **Build Pack** is Docker Compose and the compose file
   is `/compose.yaml`.
2. **Environment Variables**: confirm `OPENGATE_PUBLISH_ADDRESS` (VM 104's LAN
   IP) and `OPENGATE_PUBLISH_PORT=8100` are set, next to `OPENGATE_API_KEY`,
   the Ollama, WOL and ChatGPT values. They are stored in Coolify, not Git, so
   every deployment reuses them.
3. Enable the API if it is off: **Settings → Advanced → API Access** (the label
   varies by version; a disabled API answers `API is disabled`).
4. **Keys & Tokens → API tokens → Create**: name `opengate-autodeploy`, the
   **deploy** permission only, and the team that owns OpenGate. Copy the token;
   it is shown once.
5. Back in the application: **Webhooks → Deploy Webhook**. Copy that URL
   (`http://<coolify>/api/v1/deploy?uuid=<application uuid>&force=false`).
6. Optional: **Advanced → Deployment → Auto deploy → Manual deployments only**.
   The Deploy Webhook works regardless, and this avoids a double deployment if
   a GitHub webhook is ever added later.

**On VM 104** (needs `curl` and systemd):

```bash
curl -fsSL https://github.com/chrisknsmn-proxmox/OpenGate/archive/refs/heads/main.tar.gz | tar xz
cd OpenGate-main/deploy/autodeploy && sudo ./install.sh
# paste the Deploy Webhook URL, then the token (hidden)
```

It installs `/usr/local/bin/opengate-autodeploy`, the service and timer, and
`/etc/opengate-autodeploy/{config,coolify-token}` (root, `0600`). The service
runs as a throwaway unprivileged user that receives the token as a systemd
credential and can write only `/var/lib/opengate-autodeploy`. The first run
records the current commit as a baseline; it does not redeploy.

### Verify it with a real push

```bash
# dev VM
git commit --allow-empty -m "Test automatic deployment" && git push origin main
```

```bash
# VM 104: within a minute
journalctl -u opengate-autodeploy -f
#   new commit on main: 6191409 -> 1a2b3c4; requesting a Coolify deployment
#   Coolify accepted the deployment of 1a2b3c4: {"deployments":[...]}
```

Then in Coolify, **Deployments** shows a new deployment for that commit with
the Rust build in its log, and the container ends **healthy**. From any LAN
machine, `curl http://<VM 104's LAN IP>:8100/health/live` returns 200 and
`/v1/models` with your key returns the model. Only after that has worked is
automatic deployment confirmed.

### Health, failures and what survives a deployment

- The container healthcheck is `opengate healthcheck`, which calls
  `/health/live`: it does not depend on Ollama, so a sleeping PC never fails a
  deployment or restarts the container. Use `/health/ready` or
  `/health/providers` to check Ollama.
- Coolify builds the new image before recreating the container, so a failing
  `cargo build` should leave the running version in place; the deployment shows
  **Failed**. Confirm that on your Coolify version before relying on it, and
  run the [Before committing](#build-and-test) checks before you push.
- Kept across every deployment: all Coolify environment variables (API key,
  Ollama, WOL, ChatGPT, publish address), the `opengate-data` volume with the
  encrypted ChatGPT credentials, and the networks. Nothing in Git overrides
  them. Never delete the volume under **Storages**.
- If a deployment request fails (Coolify down, token revoked), the timer logs
  it and retries every minute; it records a commit only once Coolify accepts it.

### Roll back

- **Preferred:** revert in Git and push. That deploys the previous code the
  same way, and history shows what happened:
  ```bash
  git revert <bad commit> && git push origin main
  ```
- **Immediately, without Git:** if your Coolify version offers **Rollback** for
  this application, pick the previous image there. Then pause automatic
  deployment (below) until `main` is fixed, or the next poll will redeploy the
  tip of `main` only when it changes again.

### Pause or remove

- Pause: `sudo systemctl disable --now opengate-autodeploy.timer`
  (resume: `sudo systemctl enable --now opengate-autodeploy.timer`).
- Remove completely: `sudo ./install.sh --uninstall`, then delete the
  `opengate-autodeploy` token in Coolify.
- Manual deployment from the Coolify dashboard always keeps working.

### Secrets

The only new secret is the Coolify API token. It lives on VM 104 only
(`/etc/opengate-autodeploy/coolify-token`), has the `deploy` permission only,
and is passed to `curl` on stdin, never on the command line or in logs. GitHub
stores nothing. Rotate it by creating a new token, writing it to that file and
deleting the old one in Coolify.

### Troubleshooting

| Symptom (`journalctl -u opengate-autodeploy`) | Cause and fix |
|---|---|
| `could not read … from GitHub` | VM 104 has no outbound HTTPS or DNS, or `BRANCH`/`REPO` in `/etc/opengate-autodeploy/config` is wrong. |
| `Coolify answered HTTP 401` / `400` | Token wrong, expired or deleted. Create a new one with **deploy** permission. |
| `Coolify answered HTTP 403` / `API is disabled` | Enable API access in Coolify settings, or the token's team does not own the app. |
| `Coolify answered HTTP 404` | Wrong application uuid; copy the Deploy Webhook URL again. |
| `cannot reach Coolify` | The URL uses a hostname/IP not reachable from VM 104 itself; use `http://127.0.0.1:8000/api/v1/deploy?…`. |
| Nothing logged after a push | `systemctl list-timers opengate-autodeploy.timer` should show the next run; the push may not be on `main`. |
| Deployment fails: `OPENGATE_PUBLISH_ADDRESS is required` | Set it in the application's environment variables, then deploy. |
| Deployed, but `:8100` refuses connections | `docker ps` on VM 104 should show `<LAN IP>:8100->8000/tcp`; fix `OPENGATE_PUBLISH_ADDRESS`. |

**Alternative, if you later want instant, native webhooks:** expose only
Coolify's webhook path (`/webhooks/source/github/events/manual`) through an
authenticated tunnel such as Cloudflare Tunnel with a path-restricted ingress
rule, add a GitHub repository webhook with a long random secret (Coolify →
**Webhooks → Manual Git webhooks → GitHub**), and set **Auto deploy** to
**Deploy on push (webhooks)**. Never forward the dashboard or OpenGate's port.

## Using OpenGate from OpenClaw

OpenGate works as an OpenClaw **custom provider** with `api:
"openai-completions"`, including agent turns with tools, streaming and
multi-turn tool conversations. The behaviour below was checked against the
request builder of OpenClaw **2026.10.5** (`@openclaw/ai`), which treats a LAN
URL such as `http://<VM-104-LAN-IP>:8100/v1` as a *custom, proxy-like* endpoint:
it sends `tools` with an explicit `tool_choice: "auto"`, `max_completion_tokens`
(clamped to `contextWindow` minus its own input estimate), assistant messages
with `content: null` + `tool_calls`, and `tool` messages. It does **not** send
`store`, `stream_options`, `prompt_cache_key` or `reasoning_effort` to such an
endpoint unless a `compat` flag asks for them.

**Why it used to fail with HTTP 400:** OpenGate rejected exactly those fields:
non-empty `tools` and `tool_choice: "auto"` (`tools_not_supported`), and in
later turns `tool_calls`, `content: null` and `role: "tool"`. All are now
supported.

### OpenClaw configuration

Verify against the installed release; key names below are from the 2026.10.5
config schema.

```json5
{
  models: {
    providers: {
      opengate: {
        baseUrl: "http://<VM-104-LAN-IP>:8100/v1",
        apiKey: "<SecretRef/env reference to OPENGATE_API_KEY>",
        api: "openai-completions",
        models: [{
          id: "opengate/local",            // = OPENGATE_VIRTUAL_MODEL
          name: "OpenGate local (llama3.1:8b)",
          input: ["text"],                 // OpenGate accepts text only
          reasoning: false,
          contextWindow: 32768,            // = OLLAMA_NUM_CTX, see "Context window"
          maxTokens: 4096,
          compat: {
            supportsTools: true,           // was false; tools now work
            strictMessageKeys: false,      // MUST be false: true strips tool_calls and tool_call_id
            requiresStringContent: true,   // optional; OpenGate accepts text parts too
            supportsUsageInStreaming: true,// OpenGate sends the usage chunk; gives OpenClaw real token counts
            supportsStrictMode: false,     // strict:true tools are refused by OpenGate
          },
        }],
      },
      // Your direct OpenAI provider stays as it is, a separate provider:
      // OpenGate neither replaces nor touches it.
    },
  },
  agents: {
    defaults: {
      model: {
        primary: "opengate/opengate/local",   // confirm with `openclaw models list`
      },
    },
  },
}
```

If the doubled `opengate/opengate/local` causes trouble, set
`OPENGATE_VIRTUAL_MODEL=local` and use model id `local`, so the ref becomes
`opengate/local`.

On the OpenGate side (Coolify environment), for OpenClaw:

```dotenv
OLLAMA_NUM_CTX=32768            # same as contextWindow; only as high as the PC's memory allows
CHATGPT_DROP_MAX_TOKENS=true    # only if you want OpenClaw turns to use the ChatGPT plan fallback
```

- **Context.** A fresh OpenClaw agent session already sends ~15k tokens of
  system prompt and tool definitions, so an 8k window overflows on the first
  message. Use 32768 (or more, if `ollama ps` still shows `100% GPU`). See
  [Context window](#context-window).
- **Fallback.** Leave OpenClaw's own fallback to OpenGate out of this
  provider, or two layers will each try to recover from the same failure.
  OpenGate's fallback rejects `max_tokens` by default and OpenClaw always sends
  `max_completion_tokens`, so without `CHATGPT_DROP_MAX_TOKENS=true` an OpenClaw
  turn that reaches the fallback fails with `all_providers_unavailable`
  (`unsupported_request`, `param: max_tokens`). OpenClaw sends `temperature`
  only when configured; if it does, also set
  `CHATGPT_DROP_UNSUPPORTED_SAMPLING=true`.

### Limitations

- `llama3.1:8b` does tool calling, but a small model picks and fills tools
  less reliably than a large hosted one, and may answer in text where a tool
  was needed. For heavier agent work, point `OLLAMA_MODEL` at a stronger
  tools-capable model.
- `tool_choice` and `parallel_tool_calls: false` are emulated on Ollama and
  **checked**; an answer that breaks them is an error, not a silent pass.
- Text only: images in messages and tool results are rejected (`input:
  ["text"]` keeps OpenClaw from sending them).
- ChatGPT plan tool calls follow the Sign in with ChatGPT documentation
  (namespaced function tools) but have only been tested against a mock. Run
  `opengate chatgpt test` and one tool request with Ollama off before relying
  on it; a refusal surfaces as `unsupported_request`.
- Prior reasoning (`reasoning_content`) in the history is not forwarded.

### Verify end to end (manual)

The automated tests prove the protocol against mocks. These checks need the
real OpenClaw UI, PC and Ollama, so run them after each deployment:

1. `openclaw models list` shows the OpenGate model; select it for a new chat.
2. Send "Hi" in a new session: an answer streams in (no immediate context
   error). `curl $B/health/providers` then shows `ollama.context_length` equal
   to `OLLAMA_NUM_CTX`.
3. Ask something that needs a tool, e.g. "List the files in your workspace".
   OpenClaw shows the tool call, runs it, and the final answer uses its result.
   OpenGate's log shows two `POST /v1/chat/completions status=200` lines.
4. Ask a follow-up in the same session (multi-turn with tool history).
5. Switch the chat to the direct OpenAI provider and back: both work.

### Troubleshooting

| Symptom | Cause and fix |
|---|---|
| `400` `context_length_exceeded` | The conversation does not fit the window Ollama runs with. Raise `OLLAMA_NUM_CTX` (and `contextWindow`) if memory allows, or let OpenClaw compact. |
| OpenClaw: "Context window exceeded: estimated input … leaves only 0 output tokens" | OpenClaw's own pre-check: `contextWindow` is too small for its prompt. Raise `contextWindow` (and `OLLAMA_NUM_CTX` to match). |
| `400` with `param` naming a field | OpenClaw sent something OpenGate cannot honour; the message says what. With `strictMessageKeys: true`, tool results lose `tool_call_id` and fail ordering checks — set it to `false`. |
| `400` `tools_not_supported` | `OLLAMA_MODEL` has no tool support. Pick a tools-capable model. |
| `502` `tool_choice_not_satisfied` / `upstream_invalid_tool_call` | The local model did not follow the tool request or produced broken arguments. Retry, or use a stronger model. |
| Tools never called, model "describes" calls in text | The model is too weak for the toolset, or `supportsTools` is `false` in OpenClaw. |
| Answers slow after raising the context | The model no longer fits in VRAM: `ollama ps` shows a CPU share. Lower `OLLAMA_NUM_CTX`. |
| Long chats "forget" their start, no error | Ollama is too old to honour `truncate: false`; update it on the PC. |
| `503` `all_providers_unavailable`, `chatgpt: unsupported_request`, `param: max_tokens` | Fallback refused OpenClaw's output limit; set `CHATGPT_DROP_MAX_TOKENS=true`. |

## Out of scope

No tev1 or other routing model, no complexity scoring or prompt-based
escalation. No OpenAI API keys, Platform credits or API billing. No Codex
app-server, browser cookies, scraped ChatGPT endpoints or copied Codex `auth.json`.
No database, no dashboard, one local model only. OpenGate never executes
tools: it only carries tool definitions, calls and results between the client
and the model.
