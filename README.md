# OpenGate

An OpenAI-compatible gateway. Send `"model": "auto"` and the `tev1` model on
Ollama decides whether the prompt goes to your local model or ChatGPT. If the
local model fails or times out, the request falls back to ChatGPT.

```
your app ──► OpenGate ──► tev1 (Ollama): local or chatgpt?
                       ├─► local  → Ollama LOCAL_MODEL ──(on failure)──► ChatGPT
                       └─► chatgpt → OpenAI CHATGPT_MODEL
```

You can still pick a provider directly: `ollama/llama3`, `chatgpt/gpt-5-mini`,
`claude/claude-haiku-5-5`.

## Configuration

| Variable | Default | Purpose |
|---|---|---|
| `OPENGATE_API_KEY` | _(none)_ | Clients must send `Authorization: Bearer <key>`. **Set this on any public deployment.** |
| `OLLAMA_URL` | `http://localhost:11434` | Where Ollama runs |
| `ROUTER_MODEL` | `tev1` | Ollama model that decides local vs chatgpt |
| `ROUTER_PROMPT` | _(none)_ | Optional system prompt for the router. Leave unset to use tev1's own Modelfile instructions |
| `LOCAL_MODEL` | `llama3` | Ollama model that answers "local" prompts |
| `OPENAI_API_KEY` | _(none)_ | Needed for ChatGPT routing and fallback |
| `CHATGPT_MODEL` | `gpt-5-mini` | OpenAI model for "chatgpt" prompts and fallback |
| `OPENAI_URL` | `https://api.openai.com` | Override for an OpenAI-compatible endpoint |
| `ANTHROPIC_API_KEY` | _(none)_ | Only for `claude/*` models |
| `OLLAMA_TIMEOUT_SECS` | `120` | Give up on Ollama (and fall back) after this long |
| `PORT` | `8080` | Listen port |

The router reads tev1's reply and looks for `chatgpt` (or `cloud` / `remote`)
versus `local`. Anything else counts as `local`. Each decision is logged, e.g.
`router tev1 said "local"`.

## Deploy on Coolify

1. Push this repo to GitHub.
2. In Coolify: **New Resource → Private Repository (GitHub App)**, pick the
   repo, set **Build Pack** to **Dockerfile** and **Ports Exposes** to `8080`.
3. Under **Environment Variables**, set at least `OPENGATE_API_KEY`,
   `OLLAMA_URL`, `LOCAL_MODEL` and `OPENAI_API_KEY`.
4. Give it a domain so Coolify serves it over HTTPS, then deploy.

Ollama must be reachable from the Coolify container:

- Use the Ollama machine's LAN address, e.g. `OLLAMA_URL=http://192.168.1.50:11434`.
  `localhost` inside the container is the container itself.
- Start Ollama with `OLLAMA_HOST=0.0.0.0` so it accepts connections from other machines.
- Pull both models on it: `ollama pull <LOCAL_MODEL>`, and make sure `tev1` is in `ollama list`.

## Use it

```bash
curl https://opengate.example.com/v1/chat/completions \
  -H "Authorization: Bearer $OPENGATE_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model": "auto", "messages": [{"role": "user", "content": "Say hi in five words"}]}'
```

Any OpenAI-compatible client works with base URL `https://opengate.example.com/v1`
and your `OPENGATE_API_KEY` as the API key. Turn streaming off in the client:
streaming isn't supported yet.

Run locally with `cargo run`.
