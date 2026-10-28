# locum

An OpenAI-compatible model API served by the agent CLIs you already have signed in.

`locum` is a small Rust HTTP server that exposes `GET /v1/models` and `POST /v1/chat/completions`
(streaming and non-streaming) and fulfils each request by driving a locally installed harness as a
child process:

| Harness | Interface used | Token streaming |
| --- | --- | --- |
| Codex CLI | `codex app-server` (JSON-RPC over stdio), `codex exec --json` fallback | yes |
| Claude Code | `claude --print --output-format stream-json` | yes |
| OpenCode v2 | private `opencode serve` + HTTP/SSE (`generate/text` one-shot for non-streaming) | yes |

Point any OpenAI client at `http://127.0.0.1:11435/v1` with a placeholder key.

## Quick start

```sh
cargo install --path .
locum doctor --live        # probe each harness and run a one-token canary
locum serve                # 127.0.0.1:11435
```

```sh
curl http://127.0.0.1:11435/v1/chat/completions \
  -H 'content-type: application/json' -H 'authorization: Bearer anything' \
  -d '{"model":"codex#low","messages":[{"role":"user","content":"hello"}]}'
```

```python
from openai import OpenAI
c = OpenAI(base_url="http://127.0.0.1:11435/v1", api_key="x")
c.chat.completions.create(model="opencode/github-copilot/claude-sonnet-5", messages=[...], stream=True)
```

## Model names

| Form | Meaning |
| --- | --- |
| `codex`, `claude`, `opencode` | the harness's configured default model |
| `codex/gpt-6-luna`, `claude/opus`, `opencode/github-copilot/gpt-6.1-terra` | explicit model |
| `…#low` / `#medium` / `#high` / `#xhigh` / `#max` | reasoning effort (overridden by `reasoning_effort` in the body) |
| `default` | `models.default` from the config |
| anything in `[models.aliases]` | e.g. `"gpt-4o" = "codex/gpt-6-luna"` for clients that hard-code names |

`GET /v1/models` lists aliases, harness defaults and live-discovered models (Codex `model/list`,
OpenCode `/api/model`).

## Configuration

`~/.config/locum/config.toml` (or `--config`, `LOCUM_CONFIG`). Print the effective defaults with
`locum config print`. Highlights:

```toml
[server]
bind = "127.0.0.1:11435"          # non-loopback requires [auth] keys

[auth]
keys = []                         # empty = accept any bearer token

[models]
default = "codex"
[models.aliases]
"gpt-4o" = "codex/gpt-6-luna"

[compat]
reasoning_content = false         # forward reasoning deltas as delta.reasoning_content
ignore_tools = false              # accept and ignore tools/tool_choice instead of 400

[harness.codex]
mode = "app-server"               # or "exec"
isolate_home = false              # private CODEX_HOME (auth.json linked) → no user MCP servers/hooks

[harness.opencode]
oneshot = true                    # non-streaming requests use /api/experimental/generate/text
isolate_config = true             # empty OPENCODE_CONFIG_DIR → no user MCP servers/plugins

[session_cache]
enabled = false                   # resume harness sessions when the transcript prefix matches
```

Environment overrides: `LOCUM_BIND`, `LOCUM_KEYS`, `LOCUM_DEFAULT_MODEL`, `LOCUM_LOG`,
`LOCUM_CODEX_BIN`, `LOCUM_CLAUDE_BIN`, `LOCUM_OPENCODE_BIN`.

## Behaviour notes

- Harness runs are plain chat: tools, MCP servers, skills, hooks and session persistence are
  disabled, the working directory is an empty per-request temp dir, and a short behavioural
  preamble is added to the system prompt. Treat this as best-effort suppression, not a sandbox.
- Multi-turn conversations are rendered into a single prompt (`<conversation>…</conversation>`)
  because harness turns are stateless. Single-message requests are passed verbatim.
- `max_tokens` is a soft, client-side truncation (`finish_reason: "length"`); `stop` is honoured
  client-side; `temperature`, `top_p`, penalties and `seed` are accepted and ignored.
- `response_format: json_object | json_schema` adds a prompt instruction and extracts the JSON
  value; Codex also receives the schema natively (`outputSchema`).
- Usage comes from the harness where available, otherwise it is estimated (`chars/4`) and marked
  with `X-Locum-Usage: estimated`.
- Mid-stream harness failures emit `finish_reason: "stop"`, then an `{"error": …}` frame, then
  `[DONE]` so clients terminate cleanly. Client disconnects interrupt the harness turn.

## Testing

```sh
cargo test                        # unit + API conformance tests (fake harness, no model calls)
LOCUM_FAKE_HARNESS=1 locum serve  # expose the `fake` model for client testing
locum doctor --live               # real one-token canary per harness
```
