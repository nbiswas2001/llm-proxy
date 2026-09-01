# llm-proxy

An HTTP proxy in front of online LLM APIs (DeepSeek, OpenRouter, Claude, or anything else
that speaks the OpenAI protocol). Point any OpenAI client at `http://localhost:8600/v1`, and
calls are forwarded to the configured provider with the real API key attached. Streaming
responses are relayed chunk by chunk as they arrive — nothing is buffered.

The proxy exposes whichever protocol the provider it is running as speaks — every provider
declares it as `api = "openai"` or `api = "anthropic"`. Nothing is translated between the two — see
[Claude](#claude-api--anthropic).

## Quick start

```sh
cp config.example.toml config.toml     # then edit it
cp .env.example .env                   # then paste your API keys in
cargo run --release
```

```
INFO llm_proxy: provider "deepseek" ACTIVE (default) -> https://api.deepseek.com/v1 [api key: yes]
INFO llm_proxy: provider "claude" available
INFO llm_proxy: listening on http://0.0.0.0:8600/v1 (client auth: open)
```

`--provider <name>` runs a different one of the configured providers instead:
`cargo run --release -- --provider claude`.

On Windows two batch files wrap the same thing: `build.bat` rebuilds
`target\release\llm-proxy.exe` (and stops with `BUILD FAILED` rather than leaving you with a
stale binary), and `run.bat` launches that binary with `-v`. Rebuild after pulling changes,
since `run.bat` never compiles anything itself.

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
- **One provider per run.** Configure as many as you like; `--provider <name>` picks the
  one that serves this run, and without it the one marked `default = true` does.
- **Nothing is rewritten.** The request body is forwarded byte for byte, model name
  included, so you ask for whatever the active provider serves.
- **`GET /health`** returns `{"status":"ok"}`.
- **Claude too.** A provider with `api = "anthropic"` is authenticated with `x-api-key`
  instead of a bearer token, and the proxy then exposes Anthropic's Messages API rather
  than OpenAI's.
- **`--balance`** prints the credit left on the provider this run selected, for the
  providers that publish such an endpoint.

## Configuration

See `config.example.toml` for the annotated version. Minimum viable config:

```toml
[providers.deepseek]
default = true
api = "openai"
base_url = "https://api.deepseek.com/v1"
api_key = "${DEEPSEEK_API_KEY}"
```

**One provider serves each run.** `--provider <name>` names it; with no flag, the one
marked `default = true` is used. Everything else in the file is ignored entirely, API keys
included — so an unused provider may reference a key you have not set yet:

```sh
llm-proxy                        # the default provider
llm-proxy --provider openrouter  # this run only; LLM_PROXY_PROVIDER works too
```

At most one provider may set `default = true`; a name that is not in the file, or a missing
default with no `--provider`, is a startup error listing what is there:

```
Error: providers deepseek, llamacpp are all marked default; at most one may set default = true
Error: --provider "llamacp" is not in the config; available: claude, deepseek, llamacpp
Error: no default provider; set default = true on one of: deepseek, llamacpp, or name one with --provider
```

Model names go to the chosen provider untouched — the proxy neither renames models nor
keeps a list of them. That provider is the **active** one for the run: what the startup log
and the rest of this README mean by the word, whether it got there by `default = true` or by
`--provider`.

**Every provider must also declare `api`**, the wire protocol it speaks — `"openai"` for
almost everything, `"anthropic"` for Claude. There is no default: the wrong guess surfaces
as a 401 from upstream rather than a startup error, so the file says it outright. A provider
missing the field fails to load, pointing at the table:

```
Error: parsing config file config.toml

Caused by:
    TOML parse error at line 19, column 1
      |
   19 | [providers.deepseek]
      | ^^^^^^^^^^^^^^^^^^^^
    missing field `api`

    hint: every [providers.<name>] sets api = "openai", or "anthropic" for Claude
```

### API keys and `.env`

Keys are kept out of `config.toml` by writing `"${DEEPSEEK_API_KEY}"` and putting the value
in a `.env` file:

```
DEEPSEEK_API_KEY=sk-...
OPENROUTER_API_KEY=sk-...
ANTHROPIC_API_KEY=sk-ant-...
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
| `default` | Marks the provider used when `--provider` names none. At most one may set it. |
| `base_url` | Upstream API root, e.g. `https://api.deepseek.com/v1`. |
| `api` | **Required.** Wire protocol: `"openai"` or `"anthropic"`. See [Claude](#claude-api--anthropic). |
| `api_key` | Literal key, or `"${ENV_VAR}"` / `"env:ENV_VAR"` to read the environment or `.env`. Omit for a provider that needs none. |
| `api_key_env` | Alternative spelling: the name of the environment variable. |
| `headers` | Extra headers sent upstream (values support `"${ENV_VAR}"`). |
| `balance.url` | Balance endpoint for `--balance`, for a provider that is not built in. |

### Claude (`api = "anthropic"`)

Anthropic's API is not OpenAI-shaped, so a Claude provider says so:

```toml
[providers.claude]
default = true
api = "anthropic"
base_url = "https://api.anthropic.com/v1"
api_key = "${ANTHROPIC_API_KEY}"
```

Two things change, and only two: the key goes upstream as `x-api-key: <key>` rather than
`Authorization: Bearer <key>`, and `anthropic-version: 2023-06-01` is added when the caller
does not send one (a client that pins its own version keeps it). Everything else is the
usual pass-through — the body is forwarded byte for byte, `anthropic-beta` and the rest of
the headers ride along, and streaming is relayed the same way.

Because nothing is translated, **the proxy speaks Anthropic's protocol while this provider
is active**, and clients must too:

```sh
curl http://localhost:8600/v1/messages   -H "Content-Type: application/json"   -d '{"model":"claude-opus-4-6","max_tokens":1024,
       "messages":[{"role":"user","content":"hi"}]}'
```

```python
from anthropic import Anthropic
client = Anthropic(base_url="http://localhost:8600", api_key="unused")
client.messages.create(model="claude-opus-4-6", max_tokens=1024, messages=[...])
```

An OpenAI-shaped request sent while Claude is active is rejected by Anthropic, not converted
here. (Anthropic also serves an OpenAI-compatible endpoint; to use *that*, set
`api = "openai"` and point `base_url` at it as with any other provider — which is why the
field is stated per provider rather than guessed from the host.) `--balance` cannot help
here either — Anthropic publishes no balance endpoint.

## CLI

```
llm-proxy [-v|-vv] [--provider <name>] [--config <path>] [--env-file <path>]
          [--port <port>] [--host <addr>] [--check] [--balance]
```

`--config` defaults to `config.toml` and also reads `LLM_PROXY_CONFIG`. `--provider` picks
the provider for this run by its config section name, overriding `default = true`, and also
reads `LLM_PROXY_PROVIDER`. `--check` validates the config and exits.

### Remaining balance (`--balance`)

`--balance` asks the provider serving this run — the default one, or whichever
`--provider` names — how much credit is left, prints it, and exits
without binding a port:

```
$ llm-proxy --balance
provider "deepseek" -> https://api.deepseek.com/user/balance
  remaining            110.00 CNY
  granted              10.00 CNY
  topped up            100.00 CNY
  accepting requests   yes
```

```
$ llm-proxy --provider openrouter --balance
provider "openrouter" -> https://openrouter.ai/api/v1/credits
  remaining            6.50 credits
  granted              10.00 credits
  used                 3.50 credits
  key limit remaining  1.50
```

A balance API is **not part of the OpenAI protocol**, so this only works where the provider
offers one. DeepSeek (`/user/balance`) and OpenRouter (`/api/v1/credits`, plus
`/api/v1/key` for a key-scoped spend cap) are built in. OpenAI and Anthropic publish no
balance endpoint at all, and a local server has no balance to report; those cases exit
non-zero with an explanation rather than a number, so a script can tell them apart from a
low balance. The report is the only thing on stdout - startup logging is off for this flag,
and anything `-v` turns on goes to stderr - so `llm-proxy --balance` can be piped straight
into another command.

For any other provider that does document an endpoint, name it in the config:

```toml
[providers.someone.balance]
url = "https://api.example.com/account/credits"
```

The provider's key and headers are attached to the request, and the JSON that comes back is
pretty-printed as-is — nothing is assumed about its shape. An explicit `balance.url` also
overrides the built-in endpoint, should one ever move.

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
- `cargo test` covers model routing, URL joining, client auth, and balance parsing.
