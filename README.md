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
  timeouts, a concurrency limit, health, and logs that never contain prompts.
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
| `POST /v1/chat/completions` | Bearer | OpenAI-style chat completion, streaming or not. |

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
| 503 | `upstream_unavailable` | Ollama unreachable. |
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

The process refuses to start if a required value is missing or invalid. Error
messages name the variable, never its value. Blank values count as unset.
Commit only `.env.example`; `.env` is git- and docker-ignored.

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
3. Under **Domains**, add a domain for service `opengate` with the internal
   port suffix, e.g. `https://opengate.<your-domain>:8000`. Keep it reachable
   only from trusted networks (LAN/VPN or proxy allowlist); it is the single
   protected endpoint. Never publish Ollama's port.
4. Deploy, then verify from VM 104 / LXC 103:
   - `/health/live` returns 200 and the container shows *healthy*.
   - `/health/ready` returns 200. This proves the container can reach Ollama.
   - `/v1/models` returns 401 without the key and 200 with it.
   - A streaming request arrives incrementally and ends in `[DONE]`.
   - Logs show one line per request and no prompt text.

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
