# llm-proxy

An OpenAI-compatible HTTP proxy in front of online LLM APIs (DeepSeek, OpenRouter, or
anything else that speaks the OpenAI protocol). Point any OpenAI client at
`http://localhost:8600/v1`, and calls are forwarded to the configured provider with the
real API key attached. Streaming responses are relayed chunk by chunk as they arrive —
nothing is buffered.

## Quick start

```sh
cp config.example.toml config.toml     # then edit it
cp .env.example .env                   # then paste your API keys in
cargo run --release
```

```
INFO llm_proxy: provider "deepseek" (default) -> https://api.deepseek.com/v1 [api key: yes]
INFO llm_proxy: listening on http://0.0.0.0:8600/v1 (client auth: open)
```

Use it from any client:

```sh
curl http://localhost:8600/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"deepseek-chat","stream":true,"messages":[{"role":"user","content":"hi"}]}'
```

```python
from openai import OpenAI
client = OpenAI(base_url="http://localhost:8600/v1", api_key="unused")
client.chat.completions.create(model="deepseek-chat", messages=[...], stream=True)
```

## What it does

- **Any OpenAI endpoint.** `/v1/chat/completions`, `/v1/completions`, `/v1/embeddings`,
  `/v1/responses` and anything else are forwarded path-for-path; the proxy does not need to
  know the endpoint. Status codes, error bodies, and headers come back unchanged.
- **Streaming.** `stream: true` responses are relayed as they arrive, so tokens appear in
  the client at the same pace the provider emits them.
- **Keys stay server-side.** The client's `Authorization` header is dropped and replaced
  with the provider key from the config, so clients never hold a real key.
- **One active provider.** Configure as many as you like; exactly one carries
  `active = true` and receives every request. Switching provider is moving that flag.
- **Nothing is rewritten.** The request body is forwarded byte for byte, model name
  included, so you ask for whatever the active provider serves.
- **`GET /health`** returns `{"status":"ok"}`.

## Configuration

See `config.example.toml` for the annotated version. Minimum viable config:

```toml
[providers.deepseek]
active = true
base_url = "https://api.deepseek.com/v1"
api_key = "${DEEPSEEK_API_KEY}"
```

**Exactly one provider must set `active = true`.** That one receives every request; the
rest are ignored entirely, including their API keys — so an inactive provider may reference
a key you have not set yet. Zero active providers, or two, is a startup error naming them:

```
Error: providers deepseek, llamacpp are all active; exactly one may set active = true
Error: no provider is active; set active = true on exactly one of: deepseek, llamacpp
```

Switching provider means moving the flag and restarting. Model names go to the active
provider untouched — the proxy neither renames models nor keeps a list of them.

### API keys and `.env`

Keys are kept out of `config.toml` by writing `"${DEEPSEEK_API_KEY}"` and putting the value
in a `.env` file:

```
DEEPSEEK_API_KEY=sk-...
OPENROUTER_API_KEY=sk-...
```

The proxy loads `.env` from beside the config file, falling back to the nearest `.env` from
the working directory; `--env-file <path>` overrides both. A real environment variable of
the same name wins over the file, so a shell `export` still takes effect. Startup fails with
a pointed error if the active provider's key is missing or blank, rather than sending an
empty key upstream and failing later with a 401. Both `.env` and `config.toml` are
gitignored; `.env.example` and `config.example.toml` are the tracked templates.

### `[server]`

| Key | Default | Meaning |
| --- | --- | --- |
| `host` | `"0.0.0.0"` | Bind address; use `"127.0.0.1"` for local-only. |
| `port` | `8600` | Listen port. |
| `api_keys` | `[]` | If non-empty, clients must send one as `Authorization: Bearer <key>`. Empty means any client is accepted. |
| `request_timeout_secs` | `600` | Whole upstream request, a long stream included. |
| `connect_timeout_secs` | `15` | TCP/TLS connect timeout. |
| `max_body_mb` | `64` | Largest request body accepted from a client. |

### `[providers.<name>]`

| Key | Meaning |
| --- | --- |
| `active` | Exactly one provider must set this to `true`. |
| `base_url` | Upstream API root, e.g. `https://api.deepseek.com/v1`. |
| `api_key` | Literal key, or `"${ENV_VAR}"` / `"env:ENV_VAR"` to read the environment or `.env`. Omit for a provider that needs none. |
| `api_key_env` | Alternative spelling: the name of the environment variable. |
| `headers` | Extra headers sent upstream (values support `"${ENV_VAR}"`). |

## CLI

```
llm-proxy [-v|-vv] [--config <path>] [--env-file <path>] [--port <port>] [--host <addr>] [--check]
```

`--config` defaults to `config.toml` and also reads `LLM_PROXY_CONFIG`. `--check` validates
the config and exits.

### Watching traffic (`-v`)

Without a flag the proxy only logs startup, then stays quiet. `-v` prints a line per
request - where it was routed, the status, and how long it took:

```
DEBUG -> POST /v1/chat/completions  provider=deepseek  model=deepseek-chat  stream
DEBUG    upstream https://api.deepseek.com/v1/chat/completions
DEBUG <- 200 OK for POST /v1/chat/completions  headers in 412ms
DEBUG    done POST /v1/chat/completions  8241 B in 96 chunk(s), 6019ms
```

For a stream, the `headers in` and `done` timings are separate on purpose: the first is how
long the provider took to start answering, the second how long the whole stream ran. A
client that hangs up early logs `aborted` instead, with what it received before leaving.

`-vv` additionally logs request bodies and every response chunk. That includes prompts and
completions, so use it for debugging rather than as a default. `LLM_PROXY_LOG` still
overrides both (e.g. `LLM_PROXY_LOG=llm_proxy=trace,reqwest=debug`).

## Notes

- With `api_keys` empty the proxy is open to anyone who can reach the port. That is fine
  bound to `127.0.0.1`; set `api_keys` before binding `0.0.0.0`.
- The proxy speaks plain HTTP. Put it behind a TLS terminator if it leaves the machine.
- `cargo test` covers model routing, URL joining, and client auth.
