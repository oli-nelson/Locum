# locum

**An OpenAI-compatible API served by the agent CLIs you're already signed into.**

`locum` is a small, fast Rust server that speaks the OpenAI chat API on `http://127.0.0.1:11435/v1`
and answers each request by driving a locally installed agent harness as a child process:
**Codex CLI**, **Claude Code** or **OpenCode**. No API keys to manage, no extra billing – your
existing ChatGPT / Claude / Copilot subscription does the work.

```
  any OpenAI client ──▶ locum ──▶ codex app-server │ claude --print │ opencode serve ──▶ the model
                       :11435       (JSON-RPC)        (stream-json)     (HTTP + SSE)
```

A *locum* (from *locum tenens*, "one holding the place") is a professional who stands in for the
regular one. locum holds the place of the OpenAI API for your tools.

---

## Why

- **Use what you already pay for.** Editors, chat UIs, scripts and SDKs that only know the OpenAI
  API can run on your Codex, Claude Code or Copilot subscription.
- **Real streaming.** Token deltas from every harness, not chunked-at-the-end output.
- **Plain chat, not an agent.** Tools, MCP servers, skills, hooks and repo context are disabled;
  each run happens in an empty temp directory, so answers look like a direct model call.
- **Boring and safe by default.** Loopback only, single static binary, async end to end, warm
  process pools, children always reaped, prompts never on the command line.

## Install

Requires at least one of `codex`, `claude`, `opencode` on your `PATH` (or in the
usual places: `~/.local/bin`, `~/.cargo/bin`, `/opt/homebrew/bin`, …).

**Prebuilt binaries** for Linux (x64, ARM64), macOS (Intel, Apple Silicon) and Windows x64 are
attached to each [GitHub Release](https://github.com/oli-nelson/Locum/releases). Extract the
archive and put `locum` on your `PATH`. Binaries are unsigned; on macOS run
`xattr -d com.apple.quarantine ./locum` before first use.

**From source** (Rust 1.85+):

```sh
git clone <this repo> && cd locum
cargo install --path .          # → ~/.cargo/bin/locum
locum doctor --live             # probe each harness + run a one-token canary
locum serve                     # listening on 127.0.0.1:11435
```

Example `doctor --live` output:

```
codex     ok      /opt/homebrew/bin/codex codex-cli 0.165.0 auth=true app-server ok; account: chatgpt
          canary (gpt-6-luna): Stop in 3556 ms → "pong"
claude    ok      /opt/homebrew/bin/claude 2.1.310 (Claude Code) auth=unknown …
          canary (sonnet): error: Failed to authenticate: OAuth session expired …   ← run `claude login`
opencode  ok      /opt/homebrew/bin/opencode opencode v2.2.0 auth=true serve ok; 56 models visible
          canary (github-copilot/claude-sonnet-5): Stop in 2223 ms → "pong"
```

## Use it

Point any OpenAI client at `http://127.0.0.1:11435/v1` with **any** API key.

```sh
curl http://127.0.0.1:11435/v1/chat/completions \
  -H 'content-type: application/json' \
  -H 'authorization: Bearer anything' \
  -d '{
    "model": "opencode/openai/gpt-6.1-sol#medium",
    "stream": true,
    "messages": [{"role": "user", "content": "Explain locum tenens in one sentence."}]
  }'
```

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:11435/v1", api_key="locum")
for chunk in client.chat.completions.create(
    model="codex#low",
    messages=[{"role": "user", "content": "hello"}],
    stream=True,
):
    print(chunk.choices[0].delta.content or "", end="", flush=True)
```

```ts
import OpenAI from "openai";
const openai = new OpenAI({ baseURL: "http://127.0.0.1:11435/v1", apiKey: "locum" });
const r = await openai.chat.completions.create({
  model: "claude/sonnet",
  messages: [{ role: "user", content: "hello" }],
});
console.log(r.choices[0].message.content);
```

More recipes – Open WebUI, Continue, Zed, aider, Cursor, Raycast, LangChain – in
**[docs/clients.md](docs/clients.md)**.

## Picking a model

The `model` string tells locum *which harness* and *which model*:

| `model` | Runs |
| --- | --- |
| `codex` · `claude` · `opencode` | that harness's default model |
| `codex/gpt-6-luna` | Codex CLI with GPT-6 Luna |
| `claude/opus` · `claude/sonnet` · `claude/haiku` | Claude Code |
| `opencode/github-copilot/gpt-6.1-terra` | OpenCode → provider `github-copilot`, model `gpt-6.1-terra` |
| `…#low` `#medium` `#high` `#xhigh` `#max` | reasoning effort (body `reasoning_effort` wins) |
| `default` | `models.default` from your config |
| `gpt-4o`, `gpt-4o-mini`, … | anything you alias in `[models.aliases]` |

`GET /v1/models` returns everything above plus models discovered live from Codex and OpenCode, so
model pickers in client apps just work.

For clients that hard-code OpenAI names, add aliases:

```toml
# ~/.config/locum/config.toml
[models.aliases]
"gpt-4o"      = "codex/gpt-6-luna"
"gpt-4o-mini" = "opencode/github-copilot/claude-haiku-5.5"
"o3"          = "codex/gpt-6-luna#high"
```

## Configuration

`~/.config/locum/config.toml` (or `--config FILE` / `LOCUM_CONFIG`). Every key has a sensible
default – `locum config print` shows the effective configuration. The ones you're most likely to
touch:

```toml
[server]
bind = "127.0.0.1:11435"     # non-loopback binds require [auth] keys

[auth]
keys = []                    # empty = accept any key; otherwise list of allowed bearer tokens

[models]
default = "codex"            # what `default` (and bare harness names) resolve to

[compat]
reasoning_content = false    # also stream reasoning as delta.reasoning_content
ignore_tools = false         # accept and ignore `tools` instead of returning 400

[harness.codex]
default_model = "gpt-6-luna"
default_effort = "medium"

[harness.claude]
default_model = "sonnet"

[harness.opencode]
default_model = "github-copilot/claude-sonnet-5"
```

Full reference, including pools, limits, isolation and the session cache:
**[docs/configuration.md](docs/configuration.md)**.

## What's supported

| Endpoint | Status |
| --- | --- |
| `GET /v1/models`, `GET /v1/models/{id}` | ✅ static + live discovery |
| `POST /v1/chat/completions` | ✅ streaming & non-streaming, `stream_options.include_usage`, `response_format` (`json_object`, `json_schema`), `reasoning_effort`, `stop`, `max_tokens` (soft), image inputs (Codex) |
| `POST /v1/completions` | ✅ legacy shim (prompt → one user message) |
| `tools` / function calling, `n > 1`, `logprobs`, `logit_bias`, audio, embeddings | ❌ clear `400 unsupported_parameter` (tools can be ignored via `compat.ignore_tools`) |

Wire-level details, headers and error codes: **[docs/api.md](docs/api.md)**.

## How it works

1. The request is validated and `model` is resolved to `(harness, model, effort)`.
2. `messages[]` is rendered into one prompt. A single user message is passed verbatim; a
   multi-turn conversation is wrapped in a `<conversation>` transcript because harness turns are
   stateless. System messages go through the harness's native system-prompt channel.
3. A worker is taken from the harness pool (or spawned) and the turn runs in a fresh, empty temp
   directory with tools, MCP, hooks and session persistence disabled.
4. Harness events are normalised into text/reasoning deltas, usage and a finish reason, then
   written out as OpenAI JSON or SSE chunks. A client disconnect interrupts the harness turn.

Per-harness specifics (exact flags, isolation, auth, quirks): **[docs/harnesses.md](docs/harnesses.md)**.

## CLI

```
locum serve [--bind ADDR]   run the server (default command)
locum doctor [--live]       check path, version and auth for each harness; --live sends a canary
locum models                print the model list the server would expose
locum config print          effective configuration as TOML
locum config path           where the config file is read from
```

## Troubleshooting

- `503 harness_unavailable` → the CLI is missing or not logged in. Run `locum doctor --live`.
- `404 model_not_found` → check the `harness/model` spelling or add an alias.
- Slow first request → the harness process is starting; set `pool.prewarm = true`.
- Codex counts ~20k prompt tokens → your personal MCP servers/hooks are loaded; see
  `harness.codex.isolate_home` in the docs.

More in **[docs/troubleshooting.md](docs/troubleshooting.md)**.

## Development

```sh
cargo test                       # unit + API conformance tests (fake harness, no model calls)
LOCUM_FAKE_HARNESS=1 locum serve # exposes a deterministic `fake` model for client testing
python3 demo/tuios_demo.py       # interactive tuios dashboard driving live requests
```

Design notes live in [SPEC.md](SPEC.md). Changes are tracked in [CHANGELOG.md](CHANGELOG.md);
releases are cut by GitHub Actions as described in [docs/releases.md](docs/releases.md).
