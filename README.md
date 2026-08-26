# llm-proxy

An OpenAI-compatible HTTP proxy in front of online LLM APIs (DeepSeek, OpenRouter, or
anything else that speaks the OpenAI protocol). Point any OpenAI client at
`http://localhost:8600/v1`, and calls are forwarded to the configured provider with the
real API key attached. Streaming responses are relayed chunk by chunk as they arrive —
nothing is buffered.

## Quick start

```sh
cp config.example.toml config.toml     # then edit it
export DEEPSEEK_API_KEY=sk-...         # if you kept the "${DEEPSEEK_API_KEY}" placeholder
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
- **Multiple providers.** Address a non-default provider as `"<provider>:<model>"`, e.g.
  `"openrouter:qwen/qwen3-max"`.
- **Model aliases.** Map names a client hardcodes onto real ones — `"gpt-4o"` →
  `"deepseek-chat"` — and the body is rewritten on the way through.
- **`GET /v1/models`** returns every provider's models merged into one list (fetched from
  the provider when not listed in the config), so client model pickers work.
- **`GET /health`** returns `{"status":"ok"}`.

## Configuration

See `config.example.toml` for the annotated version. Minimum viable config:

```toml
default_provider = "deepseek"

[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
api_key = "${DEEPSEEK_API_KEY}"
```

`default_provider` must appear **above** the first `[table]` header — in TOML a bare key
after a table header belongs to that table.

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
| `base_url` | Upstream API root, e.g. `https://api.deepseek.com/v1`. |
| `api_key` | Literal key, or `"${ENV_VAR}"` / `"env:ENV_VAR"` to read the environment. Omit for a provider that needs none. |
| `api_key_env` | Alternative spelling: the name of the environment variable. |
| `models` | Models this provider serves. Listed models route here **without** a `<provider>:` prefix. If omitted, the list is fetched from the provider for `/v1/models`. |
| `model_aliases` | `{ "client-name" = "upstream-name" }`; the request body is rewritten. |
| `headers` | Extra headers sent upstream (values support `"${ENV_VAR}"`). |
| `fetch_models` | Ask the provider for its model list when `models` is empty (default `true`). |

### How a model name is routed

1. `"<provider>:<model>"` — explicit, wins over everything.
2. A key in some provider's `model_aliases`.
3. A name listed in some provider's `models`.
4. `default_provider` (or the sole provider, if only one is configured).

The default provider is checked first at steps 2 and 3, so it wins ambiguous names. A colon
in a name that is not a configured provider (`"llama3:8b"`) falls through to step 2 rather
than being read as a prefix.

## CLI

```
llm-proxy [--config <path>] [--port <port>] [--host <addr>] [--check]
```

`--config` defaults to `config.toml` and also reads `LLM_PROXY_CONFIG`. `--check` validates
the config and exits. Log level comes from `LLM_PROXY_LOG` (e.g.
`LLM_PROXY_LOG=llm_proxy=debug` to log every forwarded request).

## Notes

- With `api_keys` empty the proxy is open to anyone who can reach the port. That is fine
  bound to `127.0.0.1`; set `api_keys` before binding `0.0.0.0`.
- The proxy speaks plain HTTP. Put it behind a TLS terminator if it leaves the machine.
- `cargo test` covers model routing, URL joining, and client auth.
