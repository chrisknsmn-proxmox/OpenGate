# OpenGate

An authenticated, OpenAI-compatible gateway that puts your local Ollama model first.

**Local inference → Wake-on-LAN if needed → optional ChatGPT plan fallback.**

## What it does

- Exposes one model: `opengate/local`.
- Supports chat, streaming, and tool calling (your client executes the tools).
- Can wake your Ollama PC before answering.
- Falls back only when local inference is unavailable—not when a prompt is difficult.
- Has no OpenAI API-key billing path. Optional ChatGPT fallback requires an eligible sign-in and uses the plan’s limits.

## Quick start

You need Docker Compose and a reachable Ollama server with your chosen model installed.

```bash
git clone https://github.com/chrisknsmn-proxmox/OpenGate.git
cd OpenGate
cp .env.example .env
```

Edit `.env` and set:

| Setting | Value |
| --- | --- |
| `OPENGATE_API_KEY` | A strong, randomly generated client secret |
| `OLLAMA_BASE_URL` | Your Ollama server URL, reachable from Docker |
| `OLLAMA_MODEL` | An installed model; default: `llama3.1:8b` |
| `OPENGATE_PUBLISH_ADDRESS` | `127.0.0.1` for local use |

```bash
docker compose up --build -d
curl http://127.0.0.1:8100/health/ready
```

## Connect a client

Use these settings in OpenClaw or another OpenAI-compatible client:

- **Base URL:** `http://127.0.0.1:8100/v1`
- **API key:** your `OPENGATE_API_KEY`
- **Model:** `opengate/local`

For a remote client, publish on an appropriate server address and use that address instead of localhost. Protect network access.

## Optional features

- **Wake-on-LAN:** configure the PC and WOL settings. Docker deployments also use the separate [WOL relay](compose.wol-relay.yaml). [Setup guide](REFERENCE.md#wake-on-lan).
- **ChatGPT fallback:** disabled by default; requires explicit configuration and sign-in. [Setup and eligibility](REFERENCE.md#chatgpt-plan-fallback).

## Deployment

The Coolify setup runs two services: **OpenGate** and the **WOL relay**.
A private server-side timer checks `main` every minute and queues both through Coolify when the commit changes. No public webhook endpoint is required. Deployment logs and manual retries are available in Coolify.

Server-side operating notes: `/opt/opengate-autodeploy/README.md`. The detailed reference’s older automatic-deployment instructions describe an alternative setup, not this installed timer.

## Development

```bash
git config core.hooksPath .githooks  # enable the repository’s secret guard
cargo test                        # requires the Rust toolchain
```

[Full reference](REFERENCE.md): API fields, configuration, security, troubleshooting, Docker-based tests, and manual deployment.
