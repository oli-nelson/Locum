# Locum — Design Specification

**Status:** Draft v0.1 · 2026-10-28
**CLI versions inspected:** codex-cli 0.165.0 · Claude Code 2.1.310 · opencode v2.2.0 (macOS arm64)

---

## 1. Purpose

`locum` is a small, fast Rust HTTP server that exposes an **OpenAI-compatible model API** (the same surface Ollama, LM Studio, vLLM, etc. expose) and fulfils each request by driving a **locally installed agent harness** — Codex CLI, Claude Code, or OpenCode v2 — as a child process.

The name: a *locum* (from *locum tenens*, "one holding the place") is a professional who stands in for the regular one. Locum holds the place of the OpenAI API for your applications, staffed by the agent CLIs you already have signed in.

Any application that can talk to OpenAI (editors, chat UIs, SDKs, CI scripts) can be pointed at `http://127.0.0.1:11435/v1` with a placeholder API key, and the responses come from whichever harness/subscription the user already has signed in.

```
┌──────────────┐  POST /v1/chat/completions   ┌───────────────┐   spawn / JSON-RPC / SSE   ┌────────────────┐
│  OpenAI-     │ ───────────────────────────▶ │     locum     │ ─────────────────────────▶ │ codex          │
│  compatible  │ ◀─────────────────────────── │  (axum/tokio) │ ◀───────────────────────── │ claude         │
│  client      │   JSON or SSE chunk stream   └───────────────┘   normalised event stream   │ opencode serve │
└──────────────┘                                                                            └────────────────┘
```

### 1.1 Goals

- Faithful OpenAI wire compatibility for the endpoints clients actually use: `GET /v1/models`, `POST /v1/chat/completions` (streaming and non-streaming), plus error envelope parity.
- Token-level **streaming** wherever the harness can deliver it; chunked fallback otherwise.
- **High efficiency**: single static binary, async I/O end to end, warm process pools, bounded memory, zero shell usage, no TUI or PTY.
- Use each harness through its **best headless interface** (not screen scraping): Codex `app-server` JSON-RPC, Claude `--print --output-format stream-json`, OpenCode `serve` HTTP/SSE API.
- Harness runs are **pure chat**: tools, MCP servers, skills, hooks, repo context and session persistence are disabled so output mirrors a plain model call.
- Safe defaults: loopback bind, no secrets logged, child processes always reaped.

### 1.2 Non-goals (v1)

- Function/tool calling passthrough (`tools`, `tool_choice`) — rejected with a clear 400 unless `compat.ignore_tools = true`.
- `n > 1`, `logprobs`, `logit_bias`, audio, embeddings, fine-tuning, files, batches.
- Multi-tenant auth / billing. One local user, one machine.
- Being a general agent runner. locum never lets the harness touch the filesystem or network beyond what the model provider needs.

---

## 2. Architecture overview

```
src/
├── main.rs            CLI (clap) → config → server bootstrap
├── config.rs          TOML + env + flags; model aliases; harness settings
├── server/
│   ├── mod.rs         axum Router, middleware (auth, CORS, request-id, body limit)
│   ├── models.rs      GET /v1/models, /v1/models/{id}
│   ├── chat.rs        POST /v1/chat/completions (stream + non-stream)
│   ├── completions.rs POST /v1/completions (legacy shim → chat)
│   ├── health.rs      /healthz, /readyz, /metrics (optional)
│   └── error.rs       OpenAI error envelope + status mapping
├── openai/            Wire types (serde) — request/response/chunk/usage
├── prompt.rs          messages[] → (system, transcript, last_user) rendering
├── harness/
│   ├── mod.rs         `Harness` trait, `Turn`, `Event`, `Usage`, registry
│   ├── codex.rs       codex app-server (JSON-RPC over stdio) + exec fallback
│   ├── claude.rs      claude --print stream-json
│   ├── opencode.rs    private `opencode serve` + HTTP/SSE
│   └── fake.rs        deterministic test harness (cfg(test) / `--features fake`)
├── process/
│   ├── spawn.rs       executable resolution, argv building, env sanitising
│   ├── supervisor.rs  process groups, kill-on-drop, timeouts, bounded pipes
│   └── pool.rs        warm pools, idle TTL, concurrency limits
└── session_cache.rs   (phase 2) transcript-prefix → harness session reuse
```

Runtime: **tokio** (multi-thread), **axum 0.8** on **hyper 1**, `serde`/`serde_json`, `tokio-util` (`CancellationToken`, codecs), `process-wrap` (process groups), `bytes`, `tracing`, `clap`, `reqwest` (OpenCode HTTP client, rustls, no default features), `tokio-stream`/`async-stream` for SSE bodies.

Request lifecycle:

1. Middleware: generate request id, enforce body limit (default 4 MiB), validate `Authorization` (see §4.3).
2. Parse `ChatCompletionRequest`; validate and reject unsupported fields.
3. Resolve `model` → `(HarnessKind, model_id, reasoning_effort)` via registry (§5).
4. `prompt::render` → `Turn { system, input, images, response_schema }` (§6).
5. Acquire a worker from the harness pool (semaphore-bounded, §8). Pool miss → spawn.
6. Worker runs the turn and yields a `Stream<Event>`:
   `Event::TextDelta(str) | ReasoningDelta(str) | Usage(Usage) | Done(FinishReason) | Error(HarnessError)`.
7. `chat.rs` maps events to OpenAI chunks (SSE) or accumulates into one `ChatCompletion`.
8. Client disconnect → `CancellationToken` cancels → worker interrupts turn (`turn/interrupt` / `session/interrupt`) or kills the process group; worker returned to pool only if it is still healthy.

---

## 3. OpenAI API surface

### 3.1 `GET /v1/models`

Returns the union of configured aliases and (optionally, when `models.discover = true`) live-discovered models:

| Harness | Discovery source |
| --- | --- |
| Codex | `model/list` over app-server (cached, TTL 10 min) |
| OpenCode | `GET /api/model` on the private server (or `opencode models` once at boot) |
| Claude Code | No list API → static set from config: `opus`, `sonnet`, `haiku`, `fable` aliases + any user-defined full names |

```json
{ "object": "list", "data": [
  { "id": "codex/gpt-6-luna", "object": "model", "created": 1761600000, "owned_by": "codex" },
  { "id": "claude/opus",       "object": "model", "created": 1761600000, "owned_by": "claude-code" },
  { "id": "opencode/github-copilot/claude-sonnet-5", "object": "model", "created": 1761600000, "owned_by": "opencode" },
  { "id": "default",           "object": "model", "created": 1761600000, "owned_by": "locum" }
] }
```

`GET /v1/models/{id}` returns the single entry or a 404 `model_not_found`.

### 3.2 `POST /v1/chat/completions`

Supported request fields and their mapping:

| Field | Handling |
| --- | --- |
| `model` | Required. Resolved per §5. Unknown → 404 `model_not_found`. |
| `messages` | Required. `system`/`developer`, `user`, `assistant` roles. `tool` role → 400 unless `compat.ignore_tools`. Content may be string or parts array (`text`, `image_url`). |
| `stream` | `true` → SSE. `false`/absent → single JSON body. |
| `stream_options.include_usage` | Emit a final usage-only chunk before `[DONE]`. |
| `max_tokens` / `max_completion_tokens` | Soft limit: proxy truncates output stream at approx. token count and sets `finish_reason: "length"` (harnesses have no hard cap flag). |
| `temperature`, `top_p`, `presence_penalty`, `frequency_penalty`, `seed`, `stop`, `user` | Accepted and ignored (`stop` is honoured client-side by truncating the stream). Logged at debug level once per field. |
| `reasoning_effort` | Mapped: Codex `effort`, Claude `--effort`, OpenCode `#variant`. Overrides any effort baked into the model alias. |
| `response_format` | `{"type":"json_object"}` → prompt instruction + strict JSON extraction. `{"type":"json_schema", ...}` → Codex `outputSchema`, Claude `--json-schema`, OpenCode: prompt instruction + validation. |
| `n` | Must be 1 or absent → else 400 `unsupported_parameter`. |
| `tools`, `functions`, `tool_choice`, `logprobs`, `logit_bias`, `audio`, `modalities` | 400 `unsupported_parameter` unless `compat.ignore_tools`. |

Non-streaming response:

```json
{
  "id": "chatcmpl-7f3c…",
  "object": "chat.completion",
  "created": 1761600000,
  "model": "codex/gpt-6-luna",
  "system_fingerprint": "locum-0.1.0+codex-0.165.0",
  "choices": [{ "index": 0, "message": { "role": "assistant", "content": "…" }, "finish_reason": "stop" }],
  "usage": { "prompt_tokens": 123, "completion_tokens": 45, "total_tokens": 168 }
}
```

Streaming response (`text/event-stream`, `Cache-Control: no-cache`, `X-Accel-Buffering: no`):

```
data: {"id":"chatcmpl-…","object":"chat.completion.chunk","created":…,"model":"…","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}

data: {"id":"chatcmpl-…","object":"chat.completion.chunk",…,"choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}

data: {"id":"chatcmpl-…","object":"chat.completion.chunk",…,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: {"id":"chatcmpl-…","object":"chat.completion.chunk",…,"choices":[],"usage":{…}}     ← only with include_usage

data: [DONE]
```

Rules: the first chunk carries `role`; the same `id`/`created` are used for every chunk; chunks are flushed as soon as a harness delta arrives (no coalescing beyond a 5 ms micro-batch window, configurable, to avoid one-byte SSE frames); a mid-stream harness failure after bytes have been sent emits `finish_reason: "stop"` followed by an `error` event frame (`data: {"error":{…}}`) and `[DONE]` so clients terminate cleanly.

Reasoning: when `compat.reasoning_content = true`, reasoning summary deltas are forwarded as `delta.reasoning_content` (DeepSeek/vLLM convention). Off by default.

### 3.3 `POST /v1/completions` (legacy)

Thin shim: `prompt` (string or single-element array) becomes one user message; response re-shaped to `text_completion`. Exists because some tools still probe it.

### 3.4 `POST /v1/responses` (phase 3, optional)

Structurally close to Codex's own item model; deferred until a concrete client needs it.

### 3.5 Errors

Always the OpenAI envelope:

```json
{ "error": { "message": "…", "type": "invalid_request_error", "param": "n", "code": "unsupported_parameter" } }
```

| Situation | HTTP | `type` / `code` |
| --- | --- | --- |
| Bad JSON / missing field | 400 | `invalid_request_error` |
| Unsupported param | 400 | `invalid_request_error` / `unsupported_parameter` |
| Unknown model | 404 | `invalid_request_error` / `model_not_found` |
| Bad/missing key when `auth.keys` configured | 401 | `authentication_error` / `invalid_api_key` |
| Pool saturated beyond `queue_timeout` | 429 | `rate_limit_error` / `server_busy` |
| Harness binary missing / not authenticated | 503 | `api_error` / `harness_unavailable` |
| Harness failed / timed out | 502 / 504 | `api_error` / `harness_error`, `harness_timeout` |
| Context window exceeded (Codex `ContextWindowExceeded`, Claude prompt-too-long) | 400 | `invalid_request_error` / `context_length_exceeded` |

Harness stderr is never returned to the client; a bounded excerpt (last 8 KiB) is logged at `warn` with the request id.

---

## 4. Server behaviour

### 4.1 Binding & transport

- Default `127.0.0.1:11435` (one above Ollama's 11434). HTTP/1.1 with keep-alive; HTTP/2 cleartext not needed.
- Optional `--bind 0.0.0.0` requires `auth.keys` to be non-empty (refuse to start otherwise).
- Optional Unix socket (`--unix /path.sock`).
- CORS: permissive by default for loopback use (`cors.origins = ["*"]`), configurable.

### 4.2 Limits

- Request body ≤ 4 MiB; messages total ≤ `limits.max_prompt_chars` (default 400 k chars).
- Per-request wall clock `limits.request_timeout` (default 300 s); first-byte timeout `limits.first_token_timeout` (default 90 s).
- Harness stdout buffered per event line, cap 1 MiB per line; accumulated completion cap 8 MiB.

### 4.3 Authentication

- Default: **accept any** `Authorization: Bearer …` (or none). The key is a placeholder by design.
- `auth.keys = ["sk-local-…"]`: constant-time compare against the list; also accepted via `x-api-key` for Anthropic-style clients.

---

## 5. Model routing

A request's `model` string is resolved in order:

1. **Exact alias** from `[models.aliases]` in config, e.g. `"gpt-4o" = "codex/gpt-6-luna"`, `"default" = "claude/sonnet"`. Aliases let unmodifiable clients that hard-code `gpt-4o` work.
2. **Prefixed form** `<harness>/<model>[#<effort>]` where `<harness>` ∈ `codex | claude | opencode`. For OpenCode the model part itself contains a slash: `opencode/github-copilot/claude-sonnet-5#high`.
3. **Harness default**: bare `codex`, `claude`, `opencode` → that harness's configured default model.
4. **Global default** when `model` is `default` or `models.fallback_to_default = true`.

Effort may be appended as `#low|medium|high|xhigh|max` (OpenCode's own syntax, generalised) and is overridden by `reasoning_effort` in the request body.

---

## 6. Prompt rendering

Harness turns are **single-shot** by default (no server-side conversation state), so the proxy must carry the whole conversation in each turn. Rendering is deterministic and harness-aware:

- `system`/`developer` messages are concatenated (in order) into **`system`** and delivered through the harness's native system-prompt channel when one exists (Claude `--system-prompt`, OpenCode agent `prompt`, Codex `developer_instructions`/`model_instructions_file` config override — verify at implementation time; otherwise prepended to the transcript under a `# System` heading).
- If there is **exactly one user message and no assistant history**, the user content is the turn input verbatim (no wrapping — this is the common case and keeps the model's behaviour identical to a direct API call).
- Otherwise the transcript is rendered as:

  ```
  <conversation>
  <user>…</user>
  <assistant>…</assistant>
  …
  </conversation>
  Reply to the final user message. Output only the assistant reply.
  ```

  followed by the final user message as the turn input. The wrapper text is configurable (`prompt.transcript_template`).

- A fixed **behavioural preamble** is appended to `system` to counteract agent-harness habits: *"You are being used as a plain chat model through an API. Do not use tools, do not inspect files, do not mention the working directory, do not add preambles about what you are about to do. Reply only with the assistant message."* Configurable / disableable.
- `image_url` parts: data-URIs are written to the request's scratch dir and passed as `localImage` (Codex) or `--image`-style attachments where supported; remote URLs passed through for Codex, otherwise 400 `unsupported_parameter` (v1: Codex only).
- Empty final user message (some clients send a trailing empty user turn) → trimmed.

---

## 7. Harness adapters

```rust
#[async_trait]
pub trait Harness: Send + Sync {
    fn kind(&self) -> HarnessKind;
    async fn probe(&self) -> Result<HarnessInfo, HarnessError>;          // version, auth ok, default model
    async fn list_models(&self) -> Result<Vec<ModelInfo>, HarnessError>;
    async fn acquire(&self) -> Result<Box<dyn Worker>, HarnessError>;    // from pool or spawn
}

#[async_trait]
pub trait Worker: Send {
    async fn run(&mut self, turn: Turn, cancel: CancellationToken)
        -> Result<BoxStream<'static, Event>, HarnessError>;
    fn healthy(&self) -> bool;          // reusable after this turn?
    async fn shutdown(self: Box<Self>);
}
```

All adapters share: executable resolution (PATH + well-known dirs such as `~/.local/bin`, `~/.cargo/bin`, `/opt/homebrew/bin`, `~/.npm-global/bin`, since GUI-launched processes often lack the shell's PATH), env sanitising (inherit the user's env so native auth works; strip `OPENAI_API_KEY`/`ANTHROPIC_API_KEY` *only if* `harness.<kind>.strip_provider_keys = true`), direct argv launch with **no shell**, prompts via stdin or RPC body **never argv** (process-listing hygiene), an empty per-request **scratch cwd** under the OS temp dir (so no `AGENTS.md`/`CLAUDE.md`/repo gets picked up), and `--color never`/`NO_COLOR=1`.

### 7.1 Codex CLI — `codex app-server` (primary)

Why: it is the only Codex interface with **true token deltas** (`item/agentMessage/delta`) and a persistent process that can host many threads, so one worker serves many requests with zero spawn cost.

Process: `codex app-server` (stdio transport, JSONL JSON-RPC). One process per worker; pool default `min 0 / max 2`. Started with:

```
codex app-server --ignore-user-config? (not available on app-server; use -c overrides instead)
  -c 'sandbox_mode="read-only"' -c 'approval_policy="never"'
  -c 'features.*' disabled as needed (web_search etc.)
  -c 'mcp_servers={}' -c 'hooks={}'            # verify key names against 0.165 config reference
```

Handshake: `initialize` (`clientInfo.name = "locum"`, `capabilities.optOutNotificationMethods` for noisy events we don't consume: `turn/diff/updated`, `turn/plan/updated`, `item/commandExecution/outputDelta`, `thread/tokenUsage/updated` *unless* usage wanted) → `initialized`.

Per request:

1. `thread/start { model, cwd: <scratch>, approvalPolicy: "never", sandbox: "readOnly", ephemeral: true, serviceName: "locum" }` → `threadId`. (If `ephemeral` is not accepted on `thread/start` in the installed version, start normally and `thread/delete` afterwards.)
2. `turn/start { threadId, input: [{type:"text", text}], model, effort, outputSchema?, summary: "concise"|"none" }`.
3. Consume notifications filtered by `threadId`:
   - `item/agentMessage/delta` → `TextDelta`
   - `item/reasoning/summaryTextDelta` → `ReasoningDelta`
   - `item/completed` with `agentMessage` → authoritative final text (reconcile: if deltas ≠ final, send the diff as a last delta)
   - `thread/tokenUsage/updated` → `Usage`
   - `turn/completed` → `Done(status)`; `failed` carries `error.codexErrorInfo` (map `ContextWindowExceeded` → 400)
   - `error` → `Error`
   - Any `item/started` for `commandExecution|fileChange|mcpToolCall|webSearch` → log at warn (the preamble should prevent these; `approvalPolicy: never` + `readOnly` makes them inert) and continue.
4. Cancellation → `turn/interrupt`. Cleanup → `thread/unsubscribe` (+ `thread/delete` for non-ephemeral).

System prompt: delivered via `turn/start`/config `developer_instructions` if exposed; fallback to transcript prepend (§6).

Model list: `model/list` (`includeHidden: false`) → ids, `defaultReasoningEffort`, `supportedReasoningEfforts`.

Fallback `codex exec` (used if app-server fails to initialise, or `harness.codex.mode = "exec"`):

```
codex exec --json --ephemeral --ignore-user-config --ignore-rules --skip-git-repo-check
           --sandbox read-only --color never [-m MODEL] [-c model_reasoning_effort="high"]
           [--output-schema FILE] -C <scratch> -
```

prompt on stdin; parse JSONL `item.completed` (`agent_message` → full text, emitted as one delta), `turn.completed.usage` → `Usage`, `error` → `Error`. No token streaming in this mode.

### 7.2 Claude Code — `claude --print --output-format stream-json`

Process (one per request by default; see warm-mode below):

```
claude --print --verbose
       --output-format stream-json --include-partial-messages
       --safe-mode --tools "" --strict-mcp-config --mcp-config '{"mcpServers":{}}'
       --disable-slash-commands --no-session-persistence --permission-prompts none
       --setting-sources ""            # verify: ignore user/project/local settings
       [--system-prompt SYSTEM] [--model MODEL] [--effort EFFORT]
       [--json-schema SCHEMA] [--max-budget-usd N]
```

cwd = scratch dir; prompt via **stdin** (`--input-format text`). `--safe-mode` is used rather than `--bare` because `--bare` changes auth discovery (observed in prior validation: `--bare` skips the keychain/OAuth credential lookup and expects an API key in the environment).

Event parsing (one JSON object per line):

| Line | Action |
| --- | --- |
| `{"type":"system","subtype":"init",…}` | record `session_id`, `model` |
| `{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":…}}}` | `TextDelta` |
| `…"delta":{"type":"thinking_delta","thinking":…}` | `ReasoningDelta` |
| `{"type":"assistant","message":{…}}` | authoritative final content (reconcile as in Codex) |
| `{"type":"result","subtype":"success","result":…,"usage":{input_tokens,output_tokens,cache_read_input_tokens,…},"total_cost_usd":…}` | `Usage`, `Done(stop)` |
| `{"type":"result","subtype":"error_*",…}` | `Error` (prompt-too-long → 400) |

Exit code ≠ 0 after a `result` has been received is logged, not surfaced.

Warm mode (`harness.claude.warm = true`, phase 2): spawn with `--input-format stream-json --replay-user-messages` and keep the process alive; each new **conversation** (per §9 session cache) is a separate process, each turn is a `{"type":"user","message":{"role":"user","content":[…]}}` line. Idle TTL kills it. Not used for stateless mode because one Claude process = one conversation.

Model list: static. Defaults from config (`default_model = "sonnet"`); the `init` event's `model` field is reported back in responses so clients see the real resolved name.

### 7.3 OpenCode v2 — private `opencode serve`

Why: OpenCode's HTTP API gives per-session SSE with `session.text.delta` events, model listing, and a one-shot `generate/text` endpoint — all without the TUI and without the shared background service.

Process (one per worker, pool `min 0 / max 1` — a single server multiplexes sessions):

```
OPENCODE_CONFIG_CONTENT='{ "agent": { "locum": { "mode": "primary", "prompt": "<SYSTEM>", "tools": { "*": false }, "permission": { "*": "deny" } } }, "mcp": {}, "plugin": [], "share": "disabled", "autoupdate": false }'
opencode serve --hostname 127.0.0.1 --port <free port> --log-level warn
```

cwd = scratch dir. (Exact config keys to be confirmed against the v2 config reference; the intent is a primary agent with no tools, no MCP, no plugins and no permission prompts. If per-session system prompts cannot be set through agent config at runtime, the system text goes into the transcript per §6 and the agent prompt is just the behavioural preamble.) Readiness: poll `GET /api/info` with backoff up to 15 s.

Per request (streaming):

1. `POST /api/session` `{ title: "locum <req-id>" }` → `sessionID`.
2. Open `GET /api/experimental/session/{sessionID}/log?follow=true` (SSE) *before* prompting, so no events are missed.
3. `POST /api/session/{sessionID}/prompt` `{ text, model: {providerID, modelID, variant?}, agent: "locum" }` (model switch via `POST /api/session/{id}/model` if the prompt body doesn't accept it in the installed version).
4. Consume SSE: `session.text.delta` → `TextDelta`; `session.reasoning.delta` → `ReasoningDelta`; `session.usage.updated` → `Usage`; `session.execution.succeeded` / `session.idle{outcome}` → `Done`; `session.execution.failed` / `session.step.failed` → `Error`; `session.permissions` / `session.tool.called` → warn (should never happen with tools denied; reply `reject` to any permission request to unblock).
5. Cancellation → `POST /api/session/{id}/interrupt`. Cleanup → `DELETE /api/session/{id}` (fire-and-forget).

Non-streaming fast path (`harness.opencode.oneshot = true`, default): `POST /api/experimental/generate/text { prompt, model }` → `{ data: { text } }`. One round trip, no session bookkeeping; usage estimated.

Model list: `GET /api/model` → `provider/model` ids plus variants; `GET /api/model/default` for the default.

Fallback `opencode run --standalone --format json --agent locum [--model provider/model#variant] <prompt>` is retained only for diagnostics (`locum doctor`), since it puts the prompt in argv.

### 7.4 Event normalisation guarantees

- Every stream ends with exactly one `Done` or `Error`.
- `TextDelta`s concatenated == final text (adapters reconcile with the authoritative final message; if the harness's final text is *shorter* than the deltas we trust the deltas and log).
- `Usage` may arrive zero or more times; the last wins. Absent → estimated (`chars / 4`, rounded) and marked `"usage_estimated": true` in the response's `x-locum` metadata header (`X-Locum-Usage: estimated`).

---

## 8. Process management & efficiency

- **Pools per harness**: `min_idle`, `max_workers`, `idle_ttl` (default 120 s), `max_turns_per_worker` (recycle long-lived app-server/serve processes to bound memory; default 200). Warm-up on boot is opt-in (`pool.prewarm = true`) so the proxy starts in milliseconds and spawns on first use.
- **Concurrency**: `limits.max_inflight` (default 8) global semaphore + per-harness semaphores; waiting beyond `limits.queue_timeout` (default 30 s) → 429.
- **Supervision**: children in their own process group (`process-wrap`), `kill_on_drop`, SIGTERM then SIGKILL after 2 s; all children reaped on proxy shutdown (SIGINT/SIGTERM handler with 5 s grace that interrupts in-flight turns first).
- **I/O**: line-framed readers with `tokio_util::codec::LinesCodec` (max line length enforced); stderr drained concurrently into a 8 KiB ring buffer; stdout parsed incrementally; SSE bodies are `Bytes` written straight through hyper with no intermediate `String` copies; `serde_json` with `RawValue` for passthrough fields.
- **Latency**: first SSE chunk (role) is sent immediately after the worker is acquired so clients see headers/TTFB fast; micro-batching window 5 ms; `TCP_NODELAY`.
- **Memory**: bounded buffers everywhere; no per-request allocation of the full transcript beyond one render; `mimalloc` optional feature.
- **Binary**: `profile.release` with `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, `strip = true`; `reqwest` with `rustls-tls` and no default features; target < 10 MB.

---

## 9. Session cache (phase 2)

Resending the whole transcript each turn costs tokens and latency. Optional cache:

- Key: `blake3(model, system, messages[..n-1])` for the request → stored harness continuation (`codex threadId`, Claude `session_id`, OpenCode `sessionID`) plus the response text we produced for that prefix.
- Hit when a new request's `messages[..n-1]` hash equals a stored key **and** the stored assistant reply equals `messages[n-2].content` (so client-side edits invalidate). On hit, only the last user message is sent via `turn/start` / stream-json user line / `prompt`.
- Misses fall back to stateless rendering. Entries expire after `session_cache.ttl` (default 10 min) and are capped (`max_entries`, default 256, LRU). Requires non-ephemeral threads / `--no-session-persistence` off; all persisted sessions are deleted on expiry and on shutdown.

Off by default until validated: correctness of the prefix rule, and the harness's own context compaction can make the "same prefix" assumption drift.

---

## 10. Configuration

Precedence: CLI flags > env (`LOCUM_*`) > `~/.config/locum/config.toml` > defaults.

```toml
[server]
bind = "127.0.0.1:11435"
# unix = "/tmp/locum.sock"
cors_origins = ["*"]

[auth]
keys = []                      # empty = accept any key

[limits]
max_inflight = 8
queue_timeout = "30s"
request_timeout = "300s"
first_token_timeout = "90s"
max_body_bytes = 4194304

[models]
default = "codex"              # harness or full id
discover = true
fallback_to_default = false
[models.aliases]
"gpt-4o"      = "codex/gpt-6-luna"
"gpt-4o-mini" = "opencode/github-copilot/claude-haiku-5.5"
"claude"      = "claude/sonnet"

[compat]
ignore_tools = false
reasoning_content = false
stop_sequences = true

[prompt]
preamble = true
# transcript_template = "…"

[harness.codex]
enabled = true
mode = "app-server"            # or "exec"
default_model = "gpt-6-luna"
default_effort = "medium"
pool = { min_idle = 0, max_workers = 2, idle_ttl = "120s", max_turns_per_worker = 200 }

[harness.claude]
enabled = true
default_model = "sonnet"
models = ["opus", "sonnet", "haiku", "fable"]
warm = false
pool = { max_workers = 4 }

[harness.opencode]
enabled = true
default_model = "github-copilot/claude-sonnet-5"
oneshot = true
pool = { max_workers = 1, idle_ttl = "300s" }

[session_cache]
enabled = false
ttl = "10m"
max_entries = 256

[log]
level = "info"                 # tracing filter
json = false
```

CLI: `locum serve [--bind] [--config]`, `locum doctor` (probe each harness: path, version, auth, a 1-token canary when `--live`), `locum models`, `locum config print`.

---

## 11. Security & privacy

- Loopback by default; non-loopback bind demands configured keys.
- No shell, no templated command strings: argv is built from typed values. Prompts never on argv.
- Harness isolation: read-only sandbox / tools disabled / MCP empty / hooks and plugins off / scratch cwd. Treat as *best-effort suppression*, not a security boundary: managed/enterprise settings and provider-side integrations can still apply regardless of the flags passed.
- Logs never contain prompts or completions above `debug`; stderr excerpts are bounded and only logged on failure.
- Scratch dirs are per-request `tempdir`s removed on completion; image data-URIs are the only thing written to them.
- Provider credentials are handled entirely by the harnesses; locum never reads them.

---

## 12. Observability

- `tracing` spans per request: `req_id`, `model`, `harness`, `ttfb_ms`, `duration_ms`, `completion_chars`, `usage`, `pool_wait_ms`, `spawned: bool`.
- `GET /healthz` (process alive), `GET /readyz` (at least one enabled harness probed OK), optional `GET /metrics` (Prometheus text; feature flag `metrics`).
- `X-Request-Id` echoed/generated; `X-Locum-Harness`, `X-Locum-Usage: exact|estimated` response headers.

---

## 13. Testing

- **Fake harness** binary that speaks each of the three wire protocols from fixture files, selected by env var; used for all unit/integration tests so CI never calls a model.
- **Fixture corpus** under `tests/fixtures/<harness>/*.jsonl` captured from real runs (`locum doctor --live --record`), covering: normal stream, reasoning deltas, error mid-stream, context-length error, usage shapes, tool-call attempts, non-zero exit after success.
- **Conformance tests** using `async-openai` (Rust) and a tiny Python `openai` script: list models, non-stream, stream with/without `include_usage`, `response_format`, unsupported params, client disconnect mid-stream (assert child killed within 2 s).
- **Load test** (`oha`/`k6`) against the fake harness to validate pool limits, 429 behaviour and no fd/process leaks over 10 k requests.
- Property test: `concat(TextDeltas) == final_text` for every fixture.

---

## 14. Delivery phases

| Phase | Scope |
| --- | --- |
| **0 — skeleton** | axum server, config, `/v1/models` static, error envelope, fake harness, CI. |
| **1 — MVP** | Claude adapter (stream-json) + Codex `exec` fallback + OpenCode one-shot `generate/text`; non-stream and stream endpoints; `doctor`. |
| **2 — streaming everywhere** | Codex app-server worker with pooling; OpenCode `serve` SSE path; usage from harnesses; `reasoning_content`; `response_format`. |
| **3 — efficiency & state** | Warm pools/prewarm, session cache, Claude warm mode, `/metrics`, image inputs (Codex), `/v1/responses` if needed. |

---

## 15. Open questions / to verify during implementation

1. Codex `thread/start` — confirm `ephemeral` is accepted for root threads in 0.165, and the exact config key for developer/system instructions (`developer_instructions` vs `model_instructions_file`).
2. Claude `--setting-sources ""` semantics vs `--safe-mode`; whether `--include-partial-messages` emits `thinking_delta` for all models.
3. OpenCode v2 config keys for a tool-less primary agent via `OPENCODE_CONFIG_CONTENT`, whether `prompt` body accepts `model`/`agent` inline, and whether `generate/text` honours the agent prompt (if not, system text is prepended for the one-shot path).
4. Whether `max_tokens` should be a hard client-side truncation (current plan) or ignored; truncating mid-sentence may be worse for users than exceeding the limit.
5. Reporting the *proxy* model id vs the harness-resolved model id in `model` fields — plan: echo the request's `model` for client stability, expose resolved name in `X-Locum-Resolved-Model`.
