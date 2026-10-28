# Configuration reference

locum reads, in order of precedence:

1. command-line flags (`locum serve --bind …`)
2. environment variables (`LOCUM_*`, listed at the bottom)
3. `~/.config/locum/config.toml` – or the file given by `--config` / `LOCUM_CONFIG`
4. built-in defaults

Every key is optional. `locum config print` prints the effective configuration; `locum config
path` prints where the file is expected. Unknown keys are rejected at startup so typos are caught
early. Durations use humantime syntax (`"30s"`, `"5m"`, `"1m 30s"`, `"250ms"`).

---

## `[server]`

| Key | Default | Meaning |
| --- | --- | --- |
| `bind` | `"127.0.0.1:11435"` | TCP address to listen on. Binding to a non-loopback address is refused unless `auth.keys` is non-empty. |
| `unix` | *(unset)* | Listen on a Unix socket instead of TCP, e.g. `"/tmp/locum.sock"`. |
| `cors_origins` | `["*"]` | Allowed CORS origins for browser clients. Use explicit origins to restrict. |

## `[auth]`

| Key | Default | Meaning |
| --- | --- | --- |
| `keys` | `[]` | Accepted API keys. Empty means **any** `Authorization: Bearer …` (or none) is accepted – the key is a placeholder. When set, keys are compared in constant time and also accepted via the `x-api-key` header. |

```toml
[auth]
keys = ["sk-local-3f9a…"]
```

## `[limits]`

| Key | Default | Meaning |
| --- | --- | --- |
| `max_inflight` | `8` | Maximum concurrent turns across all harnesses. |
| `queue_timeout` | `"30s"` | How long a request waits for a free slot before `429 server_busy`. |
| `request_timeout` | `"5m"` | Wall-clock limit for one turn → `504 harness_timeout`. |
| `first_token_timeout` | `"1m 30s"` | Limit for the first output byte from the harness. |
| `max_body_bytes` | `4194304` | Request body cap (4 MiB). |
| `max_prompt_chars` | `400000` | Rendered prompt cap → `400 context_length_exceeded`. |
| `max_line_bytes` | `1048576` | Per-line cap when parsing harness output. |
| `max_completion_bytes` | `8388608` | Total completion cap; output beyond this is truncated with `finish_reason: "length"`. |
| `micro_batch` | `"5ms"` | Window for coalescing tiny text deltas into one SSE chunk. `"0ms"` disables batching. |

## `[models]`

| Key | Default | Meaning |
| --- | --- | --- |
| `default` | `"codex"` | What `model: "default"` resolves to. May be a harness name or a full `harness/model#effort` id. |
| `discover` | `true` | Ask Codex (`model/list`) and OpenCode (`/api/model`) for live model lists in `GET /v1/models`. |
| `discover_ttl` | `"10m"` | Cache lifetime for discovered models. |
| `fallback_to_default` | `false` | Route unknown model names to `default` instead of returning `404 model_not_found`. Handy for clients with fixed model pickers. |

### `[models.aliases]`

Map any client-facing name to a locum model id. Aliases may chain (up to 4 hops) and may carry an
effort suffix.

```toml
[models.aliases]
"gpt-4o"       = "codex/gpt-6-luna"
"gpt-4o-mini"  = "opencode/github-copilot/claude-haiku-5.5"
"o3"           = "codex/gpt-6-luna#high"
"claude"       = "claude/sonnet"
"fast"         = "opencode#low"
```

## `[compat]`

| Key | Default | Meaning |
| --- | --- | --- |
| `ignore_tools` | `false` | Accept `tools`, `functions`, `tool_choice` and `tool`-role messages and ignore them (tool messages become user observations) instead of returning `400 unsupported_parameter`. |
| `reasoning_content` | `false` | Forward reasoning/thinking deltas as `delta.reasoning_content` (DeepSeek/vLLM convention) and `message.reasoning_content`. |
| `stop_sequences` | `true` | Honour `stop` client-side by truncating output at the first match. |
| `max_tokens_truncation` | `true` | Honour `max_tokens` / `max_completion_tokens` as a soft, client-side cut at ≈ 4 chars per token with `finish_reason: "length"`. Set `false` to ignore the field. |

## `[prompt]`

| Key | Default | Meaning |
| --- | --- | --- |
| `preamble` | `true` | Prepend a short behavioural instruction to the system prompt telling the harness to act as a plain chat model (no tools, no mention of the working directory, no "I'll now…" preambles). |
| `preamble_text` | *(built-in)* | Replace the preamble text. |
| `transcript_template` | *(built-in)* | Template for multi-turn rendering. Placeholders: `{transcript}` (the `<user>`/`<assistant>` blocks) and `{last_user}`. |

Default preamble:

> You are being used as a plain chat model through an API. Do not use tools, do not inspect files,
> do not mention the working directory, and do not add preambles about what you are about to do.
> Reply only with the assistant message.

## `[harness.codex]`

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | Register the Codex harness. |
| `executable` | *(auto)* | Explicit path to `codex`. |
| `mode` | `"app-server"` | `"app-server"` for pooled JSON-RPC workers with token streaming; `"exec"` for one `codex exec --json` process per request (no token streaming). If app-server fails to start, locum falls back to exec automatically. |
| `default_model` | `"gpt-6-luna"` | Model for `codex` / `codex#…`. |
| `default_effort` | `"medium"` | Reasoning effort when none is given. |
| `isolate_home` | `false` | Run app-server with a private `CODEX_HOME` (under the locum data dir) that contains only a symlink to your `auth.json`. This stops your personal MCP servers, hooks and plugins from loading into every thread (big prompt-token and latency win). Off by default because Codex may rewrite `auth.json` on token refresh; see [harnesses.md](harnesses.md#codex-cli). |
| `strip_provider_keys` | `false` | Remove `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `OPENROUTER_API_KEY` from the child environment so the harness always uses its own login. |
| `reasoning_summary` | `true` | Ask Codex for reasoning summaries (needed for `compat.reasoning_content`). `false` passes `model_reasoning_summary="none"`. |

### `[harness.codex.pool]`

| Key | Default | Meaning |
| --- | --- | --- |
| `min_idle` | `0` | Processes kept alive even when idle. |
| `max_workers` | `2` | Maximum app-server processes. Each hosts many concurrent threads; requests are spread across them. |
| `idle_ttl` | `"2m"` | Kill an idle process after this long (down to `min_idle`). |
| `max_turns_per_worker` | `200` | Recycle a process after this many turns to bound memory. `0` disables. |
| `prewarm` | `false` | Start a process at boot instead of on first request. |

## `[harness.claude]`

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | Register the Claude Code harness. |
| `executable` | *(auto)* | Explicit path to `claude`. |
| `default_model` | `"sonnet"` | Model for `claude` / `claude#…`. Any name `claude --model` accepts works (`opus`, `sonnet`, `haiku`, or a full model id). |
| `default_effort` | *(unset)* | Passed as `--effort` (`low`, `medium`, `high`, `xhigh`, `max`). |
| `models` | `["opus","sonnet","haiku","fable"]` | Names advertised in `GET /v1/models` (Claude has no list API). |
| `warm` | `false` | Keep `--no-session-persistence` off so sessions can be resumed by the session cache. |
| `max_budget_usd` | *(unset)* | Per-request spend cap passed as `--max-budget-usd`. |
| `strip_provider_keys` | `false` | As for Codex. |

### `[harness.claude.pool]`

Claude runs one process per request; only `max_workers` (default `4`) matters – it bounds
concurrent Claude processes.

## `[harness.opencode]`

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | Register the OpenCode harness. |
| `executable` | *(auto)* | Explicit path to `opencode`. |
| `default_model` | `"github-copilot/claude-sonnet-5"` | Model for `opencode` / `opencode#…`. Must be `provider/model`. |
| `default_effort` | *(unset)* | Default variant (`low`/`medium`/`high`/…) when none is given. |
| `oneshot` | `true` | Serve **non-streaming** requests through `POST /api/experimental/generate/text` (one round trip, no session bookkeeping; usage is estimated). Streaming requests always use a session + SSE. |
| `isolate_config` | `true` | Point `OPENCODE_CONFIG_DIR` at an empty directory so your personal MCP servers and plugins are not started. Provider auth lives elsewhere and keeps working. |
| `strip_provider_keys` | `false` | As for Codex. |

### `[harness.opencode.pool]`

One `opencode serve` multiplexes many sessions, so `max_workers` defaults to `1` and `idle_ttl` to
`"5m"`. Other keys as for Codex.

## `[session_cache]`

Off by default. When enabled, locum remembers the harness session behind each reply it produced
(keyed by a hash of harness, model and the transcript prefix). If the next request's history
matches exactly – same prefix *and* the previous assistant message equals what locum returned –
only the new user message is sent to the resumed session instead of the whole transcript.

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `false` | Turn the cache on. |
| `ttl` | `"10m"` | Entries (and their harness sessions) are deleted after this. |
| `max_entries` | `256` | LRU bound. |

Caveats: client-side edits to history fall back to stateless rendering; harness-side context
compaction can make a resumed session drift from the client's view of the conversation.

## `[log]`

| Key | Default | Meaning |
| --- | --- | --- |
| `level` | `"info"` | A `tracing` filter (`"debug"`, `"locum=debug,hyper=warn"`, …). `LOCUM_LOG` overrides. |
| `json` | `false` | Emit JSON lines instead of human-readable logs. |

Prompts and completions are never logged above `debug`. Harness stderr is only logged (bounded to
8 KiB) when a turn fails.

---

## Environment variables

| Variable | Overrides |
| --- | --- |
| `LOCUM_CONFIG` | config file path |
| `LOCUM_BIND` | `server.bind` |
| `LOCUM_KEYS` | `auth.keys` (comma-separated) |
| `LOCUM_DEFAULT_MODEL` | `models.default` |
| `LOCUM_LOG` | `log.level` |
| `LOCUM_CODEX_BIN`, `LOCUM_CLAUDE_BIN`, `LOCUM_OPENCODE_BIN` | `harness.*.executable` |
| `LOCUM_FAKE_HARNESS=1` | also register the deterministic `fake` harness (for client testing) |

## Full default file

```toml
[server]
bind = "127.0.0.1:11435"
cors_origins = ["*"]

[auth]
keys = []

[limits]
max_inflight = 8
queue_timeout = "30s"
request_timeout = "5m"
first_token_timeout = "1m 30s"
max_body_bytes = 4194304
max_prompt_chars = 400000
max_line_bytes = 1048576
max_completion_bytes = 8388608
micro_batch = "5ms"

[models]
default = "codex"
discover = true
fallback_to_default = false
discover_ttl = "10m"

[models.aliases]

[compat]
ignore_tools = false
reasoning_content = false
stop_sequences = true
max_tokens_truncation = true

[prompt]
preamble = true

[harness.codex]
enabled = true
mode = "app-server"
default_model = "gpt-6-luna"
default_effort = "medium"
isolate_home = false
strip_provider_keys = false
reasoning_summary = true
pool = { min_idle = 0, max_workers = 2, idle_ttl = "2m", max_turns_per_worker = 200, prewarm = false }

[harness.claude]
enabled = true
default_model = "sonnet"
models = ["opus", "sonnet", "haiku", "fable"]
warm = false
strip_provider_keys = false
pool = { min_idle = 0, max_workers = 4, idle_ttl = "2m", max_turns_per_worker = 200, prewarm = false }

[harness.opencode]
enabled = true
default_model = "github-copilot/claude-sonnet-5"
oneshot = true
isolate_config = true
strip_provider_keys = false
pool = { min_idle = 0, max_workers = 1, idle_ttl = "5m", max_turns_per_worker = 200, prewarm = false }

[session_cache]
enabled = false
ttl = "10m"
max_entries = 256

[log]
level = "info"
json = false
```
