# Harnesses

locum talks to each CLI through its best headless interface – never by scraping a terminal UI. This
page explains exactly what runs, how it is isolated, and the quirks of each harness.

All harnesses share:

- **Executable lookup** – `harness.<name>.executable`, then `PATH`, then `~/.local/bin`,
  `~/.cargo/bin`, `~/.npm-global/bin`, `~/.bun/bin`, `~/.volta/bin`, `/opt/homebrew/bin`,
  `/usr/local/bin`.
- **Direct `argv` launch** – no shell, no templated command strings. Prompts travel over stdin or
  an RPC body, never on the command line (keeps them out of `ps`).
- **Empty scratch cwd** – each request gets its own temp directory, so no `AGENTS.md`, `CLAUDE.md`
  or repository is picked up. It is deleted when the request ends.
- **Inherited environment** – your login/keychain state is reused so native auth works.
  `NO_COLOR=1`, `TERM=dumb`, `CI=1` are set. `strip_provider_keys = true` additionally removes
  `OPENAI_API_KEY`/`ANTHROPIC_API_KEY`/`OPENROUTER_API_KEY`.
- **Process groups** – every child runs in its own process group and is `SIGTERM`ed then
  `SIGKILL`ed on cancel, timeout or shutdown. Nothing is left behind.
- **Best-effort suppression** – tools, MCP, hooks and plugins are disabled through flags and
  config. Managed/enterprise settings can still apply. Treat this as "plain chat", not a sandbox.

---

## Codex CLI

Tested with codex-cli 0.165.0.

### `app-server` mode (default)

```
codex app-server -c sandbox_mode="read-only" -c approval_policy="never"
```

One long-lived JSON-RPC process serves many requests; each request is an **ephemeral thread**:

1. `thread/start { model, cwd: <scratch>, approvalPolicy: "never", sandbox: "read-only",
   ephemeral: true, developerInstructions: <system prompt> }`
2. `turn/start { threadId, input: [text, images…], model, effort, outputSchema? }`
3. Notifications → events: `item/agentMessage/delta` (text), `item/reasoning/summaryTextDelta`
   (reasoning), `thread/tokenUsage/updated` (usage), `turn/completed` (done / failed).
4. Client disconnect → `turn/interrupt`.

Any approval or user-input request from Codex (command execution, file change, tool call) is
automatically **declined** and logged – with `approvalPolicy: never` and a read-only sandbox these
should not occur during plain chat.

Model discovery uses `model/list`; efforts come from each model's `supportedReasoningEfforts`.

**Pooling.** Up to `pool.max_workers` processes (default 2). Requests go to the least-loaded one;
a process is recycled after `max_turns_per_worker` turns and killed after `idle_ttl` idle.

### `exec` mode (fallback)

Used when `mode = "exec"` or when app-server fails to initialise:

```
codex exec --json --ephemeral --ignore-user-config --ignore-rules --skip-git-repo-check
           --sandbox read-only --color never -m MODEL [-c model_reasoning_effort="…"]
           [--output-schema FILE] [-i IMAGE…] -C <scratch> -
```

Prompt on stdin. The final message arrives as one delta (no token streaming). The system prompt is
prepended under a `# System` heading because exec has no instructions flag.

### Isolation and your personal setup

Codex merges `-c` overrides into your `~/.codex/config.toml`, so `-c mcp_servers={}` does **not**
remove your MCP servers – every thread starts them and your hooks run. You will notice this as
~20k prompt tokens per request and a few seconds of extra latency.

`harness.codex.isolate_home = true` fixes this by running app-server with a private `CODEX_HOME`
(`~/Library/Application Support/locum/codex-home` on macOS, `$XDG_DATA_HOME/locum/codex-home` on
Linux) containing only a symlink to your real `auth.json`. It is off by default because Codex can
rewrite `auth.json` when it refreshes tokens; through a symlink that normally updates the original,
but if Codex ever writes via rename the two copies could diverge and you would need to log in
again. If you enable it and Codex auth starts failing, delete the `codex-home` directory and run
`codex login`.

### Auth

`locum doctor` calls `account/read` and reports the account type (`chatgpt`, `apikey`). Not logged
in → `503 harness_unavailable`. Context-window errors map to `400 context_length_exceeded`; usage
or rate-limit errors to `429 harness_rate_limited`.

---

## Claude Code

Tested with Claude Code 2.1.310. One process per request, bounded by `pool.max_workers`.

```
claude --print --verbose --output-format stream-json --include-partial-messages --input-format text
       --safe-mode --tools "" --strict-mcp-config --mcp-config '{"mcpServers":{}}'
       --disable-slash-commands --permission-prompts none --setting-sources ""
       [--system-prompt-file <scratch>/system-prompt.txt] --model MODEL [--effort E]
       [--json-schema SCHEMA] [--max-budget-usd N] --no-session-persistence
```

- The prompt goes to stdin; the system prompt goes through `--system-prompt-file` (undocumented in
  `--help` but supported) so neither appears on the command line.
- `--safe-mode` disables CLAUDE.md, skills, plugins, hooks, MCP, custom commands and agents.
  `--bare` is deliberately **not** used: it changes credential discovery and expects an API key in
  the environment.
- `--tools ""` disables the built-in tool set; `--permission-prompts none` denies anything that
  would otherwise prompt.
- Events: `stream_event` → `content_block_delta` with `text_delta` (text) / `thinking_delta`
  (reasoning); `assistant` → authoritative final message (reconciled with the deltas); `result` →
  usage and finish.
- `init.model` is the real resolved model id and is exposed through the event stream.

### Auth quirk

When the OAuth session has expired, Claude exits 0 and emits a `result` with
`subtype: "success"` **and** `is_error: true`, plus an `assistant` message whose `model` is
`"<synthetic>"` containing the error text. locum detects this and returns
`503 harness_unavailable` with the message (`Failed to authenticate: OAuth session expired…`).
Fix: run `claude login` (or `claude` interactively once).

`locum doctor` cannot verify Claude auth without a model call – use `--live`.

### Models

Claude has no list API. `GET /v1/models` advertises `harness.claude.models` (default `opus`,
`sonnet`, `haiku`, `fable`); any id `claude --model` accepts works in requests.

---

## OpenCode

Tested with opencode v2.2.0. A **private** `opencode serve` process (never the shared background
service) multiplexes all requests.

```
OPENCODE_SERVER_PASSWORD=<random>          # basic auth user "opencode"
OPENCODE_CONFIG_CONTENT='{"agent":{"locum":{"mode":"primary","prompt":"<preamble>",
                           "tools":{"*":false},"permission":{"*":"deny"}}},
                          "share":"disabled","autoupdate":false}'
OPENCODE_CONFIG_DIR=<empty dir>            # when isolate_config = true (default)
OPENCODE_DISABLE_AUTOUPDATE=1 OPENCODE_DISABLE_PROJECT_CONFIG=1
opencode serve --hostname 127.0.0.1 --port <free port> --log-level warn
```

Readiness is polled on `GET /api/info` (up to 15 s). A dedicated `locum` agent carries the
behavioural preamble, has every tool disabled and every permission denied.

### Streaming requests

1. `POST /api/session { title, agent: "locum" }`
2. Subscribe to the global event bus `GET /api/event` **before** prompting (events are filtered by
   `data.sessionID`).
3. `POST /api/session/{id}/prompt { text, model: { providerID, id, variant? }, agent: "locum" }`
4. Events: `session.text.delta` (text), `session.reasoning.delta` (reasoning),
   `session.step.ended` / `session.usage.updated` (usage), `session.execution.succeeded` /
   `session.idle` (done), `session.execution.failed` / `session.step.failed` (error). Any
   permission request is answered `reject`.
5. Disconnect → `POST /api/session/{id}/interrupt`; cleanup → `DELETE /api/session/{id}`.

### Non-streaming requests (`oneshot = true`)

`POST /api/experimental/generate/text { prompt, model }` → `{ data: { text } }`. One round trip,
no session. This endpoint ignores agents, so the system prompt is prepended to the text under a
`# System` heading and usage is **estimated** (`X-Locum-Usage: estimated`). Set `oneshot = false`
to always use sessions and get exact usage.

### Model ids

OpenCode models are `provider/model`, so the locum id has two slashes:
`opencode/github-copilot/gpt-6.1-terra`, `opencode/openai/gpt-6.1-sol#medium`. `#effort` maps to the
model's **variant** (`low`, `medium`, `high`, `xhigh`, `max` for reasoning models). Discovery uses
`GET /api/model`; the server's own default comes from `GET /api/model/default`.

### Isolation

`isolate_config = true` points `OPENCODE_CONFIG_DIR` at an empty directory, so your personal
`opencode.json`, MCP servers and plugins are not loaded by the private server. Provider
credentials are stored in OpenCode's data directory, not the config directory, so auth keeps
working – `locum doctor` showed 56 models with isolation on.

---

## Fake harness (testing)

`LOCUM_FAKE_HARNESS=1 locum serve` (or `cargo test`) registers a deterministic `fake` model that
echoes the prompt word by word. Control words in the prompt: `!slow` (50 ms per word), `!reasoning`
(emits a reasoning delta), `!error` (fails mid-stream), `!context` (`400 context_length_exceeded`),
`!unavailable` (`503`). Useful for checking a client's streaming and error handling without
spending tokens.
