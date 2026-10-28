# Troubleshooting

Start with:

```sh
locum doctor --live
```

It resolves each binary, prints the version, checks auth where possible and – with `--live` – runs
a one-token canary through every enabled harness. Each line tells you which layer is broken.

To see what locum is doing, run the server with debug logging:

```sh
LOCUM_LOG=debug locum serve
```

Every finished turn logs one `turn finished` line with `req_id`, `harness`, `model`, `ttfb_ms`,
`duration_ms`, `pool_wait_ms`, token counts and any error. The `req_id` is returned to the client
in `X-Request-Id`, so you can match a client-side failure to its server log line.

---

## Errors you may see

### `503 harness_unavailable`

The CLI could not be used at all.

| Message | Fix |
| --- | --- |
| `` `codex` not found on PATH or in well-known locations `` | Install the CLI, or set `harness.codex.executable` / `LOCUM_CODEX_BIN`. |
| `Failed to authenticate: OAuth session expired and could not be refreshed` (Claude) | Run `claude login`. Claude exits successfully in this state, so only `doctor --live` or a real request reveals it. |
| `codex app-server exited` / `failed to spawn codex app-server` | Run `codex app-server` by hand to see the error. locum falls back to `codex exec` automatically for later requests. |
| `opencode serve not ready` | Run `opencode serve --hostname 127.0.0.1 --port 0` manually to see why it fails to start (e.g. a broken `opencode.json`; try `isolate_config = true`, the default). |

### `404 model_not_found`

- Check the spelling: `harness/model`, with `provider/model` for OpenCode
  (`opencode/github-copilot/gpt-6.1-terra`).
- The harness may be disabled (`harness.<name>.enabled = false`).
- Your client sends a fixed name like `gpt-4o`: add an alias, or set
  `models.fallback_to_default = true`.
- Run `locum models` to see every id the server accepts.

### `400 unsupported_parameter`

The request used a feature locum does not implement (`tools`, `n > 1`, `logprobs`, audio …). The
`param` field names it. If a client always includes `tools: []` or tool definitions you do not
need, set `compat.ignore_tools = true`.

### `400 context_length_exceeded`

Either the rendered prompt is longer than `limits.max_prompt_chars` (default 400k characters) or
the harness reported a context-window error. Trim history or pick a model with a larger window.

### `429 server_busy`

All `limits.max_inflight` slots were busy for `limits.queue_timeout`. Raise `max_inflight`, or
raise the per-harness `pool.max_workers` if one harness is the bottleneck.

### `502 harness_error`

The harness ran but the turn failed. The server log has the harness's last stderr lines at `warn`
level. Common causes: provider outage, a model that your subscription cannot use, OpenCode missing
provider credentials for that `provider/…`.

### `504 harness_timeout`

No first token within `limits.first_token_timeout` (90 s) or the whole turn exceeded
`limits.request_timeout` (5 min). Slow reasoning models at `#xhigh`/`#max` can hit the first-token
limit; raise it.

### Stream ends with an `{"error": …}` frame

The harness failed after it had already produced output. locum closes the stream cleanly
(`finish_reason: "stop"`, error frame, `[DONE]`). The partial text is valid; the error explains
why it stopped.

---

## Behaviour questions

### The first request is slow, later ones are fast

The harness process is started on demand (`codex app-server`, `opencode serve`). Set
`harness.<name>.pool.prewarm = true` to start it at boot, and raise `pool.idle_ttl` so it stays
warm between bursts.

### Codex reports ~20,000 prompt tokens for a tiny prompt

Codex loads your personal MCP servers, hooks and instructions into every thread. Set
`harness.codex.isolate_home = true` to run with a clean `CODEX_HOME` (your login is linked in). See
[harnesses.md](harnesses.md#isolation-and-your-personal-setup) for the trade-off.

### Replies mention "the working directory" or offer to run commands

The behavioural preamble should prevent this, but models occasionally ignore it. Make sure
`prompt.preamble = true` (default) and consider a stronger `prompt.preamble_text`. Tool actions
cannot actually run: Codex is read-only with approvals auto-declined, Claude has `--tools ""`, and
the OpenCode agent has every tool disabled.

### Multi-turn conversations lose context or repeat themselves

Harness turns are stateless, so locum re-sends the full history as a `<conversation>` transcript on
every request. Very long histories cost tokens on each turn. Enable `[session_cache]` to reuse the
harness session when the history prefix matches exactly.

### Usage says `estimated`

The harness did not report token counts – today that is OpenCode's one-shot path used for
non-streaming requests. Set `harness.opencode.oneshot = false` for exact counts, or request with
`stream: true`.

### `locum serve` refuses to start on `0.0.0.0`

Non-loopback binds require `auth.keys` to be non-empty. This is deliberate: without it anyone on
the network could spend your subscription.

### Child processes left behind after a crash

locum puts every child in its own process group and kills the group on cancel, timeout and
shutdown. If the locum process itself is `SIGKILL`ed, children are orphaned; find them with
`pgrep -fl 'codex app-server|opencode serve|claude --print'`. Note that Codex also runs its own
managed daemon (`codex app-server --managed-daemon`) which is **not** locum's.

### Where are locum's files?

| Thing | Location |
| --- | --- |
| Config | `~/.config/locum/config.toml` (`locum config path`) |
| Data (isolated Codex home, OpenCode config dir) | macOS `~/Library/Application Support/locum/`, Linux `$XDG_DATA_HOME/locum/` |
| Scratch dirs | OS temp dir, `locum-*`, removed per request |
| Logs | stdout/stderr only |

---

## Testing a client without spending tokens

```sh
LOCUM_FAKE_HARNESS=1 locum serve
```

exposes a `fake` model that echoes the prompt word by word. Put `!slow`, `!error`, `!context` or
`!unavailable` in the prompt to exercise the client's streaming and error paths.

## Still stuck?

Collect `locum doctor --live` output, the `turn finished` / `warn` log lines for the failing
`X-Request-Id`, and the exact request body (minus anything sensitive), and open an issue.
