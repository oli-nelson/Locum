# API reference

locum implements the subset of the OpenAI REST API that chat clients actually use. Everything is
served under `/v1`; the same routes are also mounted without the prefix (`/chat/completions`) for
Ollama-style base URLs.

Base URL: `http://127.0.0.1:11435/v1`

## Authentication

Send `Authorization: Bearer <key>` (or `x-api-key: <key>`). With the default configuration any
value – or no header at all – is accepted. When `auth.keys` is configured, the key must match one
of them or you get `401 invalid_api_key`.

## Endpoints

### `GET /v1/models`

```json
{
  "object": "list",
  "data": [
    { "id": "default",                 "object": "model", "created": 1761600000, "owned_by": "locum" },
    { "id": "gpt-4o",                  "object": "model", "created": 1761600000, "owned_by": "locum" },
    { "id": "codex",                   "object": "model", "created": 1761600000, "owned_by": "codex" },
    { "id": "codex/gpt-6-luna",        "object": "model", "created": 1761600000, "owned_by": "codex" },
    { "id": "claude/sonnet",           "object": "model", "created": 1761600000, "owned_by": "claude-code" },
    { "id": "opencode/openai/gpt-6.1-sol", "object": "model", "created": 1761600000, "owned_by": "opencode" }
  ]
}
```

The list contains `default`, every alias, each enabled harness (bare name and default model), the
static Claude names, and – when `models.discover = true` – models discovered live from Codex and
OpenCode (cached for `models.discover_ttl`).

### `GET /v1/models/{id}`

Returns one entry, or `404 model_not_found`. Any id the router can resolve (e.g. a `#effort`
variant) is accepted even if it is not in the list.

### `POST /v1/chat/completions`

#### Request fields

| Field | Support |
| --- | --- |
| `model` | **Required.** See [Model names](#model-names). Unknown → `404 model_not_found`. |
| `messages` | **Required.** Roles `system`, `developer`, `user`, `assistant`. Content may be a string or a parts array of `text` and `image_url` parts. `tool`/`function` roles and `tool_calls` → `400` unless `compat.ignore_tools`. |
| `stream` | `true` → Server-Sent Events. Default `false`. |
| `stream_options.include_usage` | Append a final usage-only chunk before `[DONE]`. |
| `max_tokens`, `max_completion_tokens` | Soft limit: output is cut at ≈ 4 characters/token with `finish_reason: "length"` (disable via `compat.max_tokens_truncation = false`). Harnesses have no hard cap flag. |
| `stop` | String or array. Output is truncated at the first match (client-side). |
| `reasoning_effort` | `low` · `medium` · `high` · `xhigh` · `max`. Overrides a `#effort` suffix on the model. Mapped to Codex `effort`, Claude `--effort`, OpenCode `#variant`. |
| `response_format` | `{"type":"json_object"}` adds a JSON-only instruction and extracts the first JSON value from the reply. `{"type":"json_schema","json_schema":{"schema":…}}` additionally passes the schema natively to Codex (`outputSchema`) and Claude (`--json-schema`). |
| `temperature`, `top_p`, `presence_penalty`, `frequency_penalty`, `seed`, `user` | Accepted and ignored. |
| `n` | Must be `1` or absent → otherwise `400 unsupported_parameter`. |
| `tools`, `functions`, `tool_choice`, `logprobs`, `top_logprobs`, `logit_bias`, `audio`, `modalities` (non-text) | `400 unsupported_parameter`. Tool fields are ignored instead when `compat.ignore_tools = true`. |

Image inputs (`image_url` parts, data URIs or remote URLs) are supported for **Codex** only; other
harnesses return `400 unsupported_parameter`.

#### Non-streaming response

```json
{
  "id": "chatcmpl-01a1840f833f7c03a25eeeaf",
  "object": "chat.completion",
  "created": 1793216971,
  "model": "codex/gpt-6-luna#low",
  "system_fingerprint": "locum-0.1.0",
  "choices": [{
    "index": 0,
    "message": { "role": "assistant", "content": "Usually blue outside." },
    "finish_reason": "stop",
    "logprobs": null
  }],
  "usage": {
    "prompt_tokens": 21310, "completion_tokens": 8, "total_tokens": 21318,
    "prompt_tokens_details": { "cached_tokens": 12544 }
  }
}
```

`model` echoes the request string; the resolved harness and model are in response headers.
`message.reasoning_content` is present when `compat.reasoning_content = true` and the harness
produced reasoning.

#### Streaming response

`Content-Type: text/event-stream`, `Cache-Control: no-cache`, `X-Accel-Buffering: no`.

```
data: {"id":"chatcmpl-…","object":"chat.completion.chunk","created":…,"model":"…","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}

data: {"id":"chatcmpl-…","object":"chat.completion.chunk",…,"choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}

data: {"id":"chatcmpl-…","object":"chat.completion.chunk",…,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: {"id":"chatcmpl-…","object":"chat.completion.chunk",…,"choices":[],"usage":{"prompt_tokens":429,"completion_tokens":19,"total_tokens":448}}

data: [DONE]
```

Guarantees:

- The first chunk carries `role`; every chunk shares the same `id` and `created`.
- Deltas are flushed as soon as the harness produces them, coalesced only within a 5 ms window
  (`limits.micro_batch`) to avoid one-byte frames.
- Reasoning deltas appear as `delta.reasoning_content` when enabled.
- The usage chunk (empty `choices`) is only sent with `stream_options.include_usage`.
- If the harness fails **after** bytes have been sent, locum emits a `finish_reason: "stop"` chunk,
  then an error frame `data: {"error":{…}}`, then `[DONE]`, so clients terminate cleanly.
- Closing the connection interrupts the harness turn within about 2 seconds.

### `POST /v1/completions` (legacy)

`prompt` (string or one-element array) becomes a single user message. The response is reshaped as
`text_completion` with `choices[].text`. Streaming works the same way. `echo`, `suffix`, `best_of
> 1` and token-id prompts are rejected.

### Health

| Route | Purpose |
| --- | --- |
| `GET /healthz` | Process alive; returns uptime and free in-flight slots. |
| `GET /readyz` | `200` when at least one harness probes OK (binary found, auth not known-bad), otherwise `503`. Body lists each harness. |
| `GET /` | Name and version. |

Health routes do not require authentication.

## Model names

```
<alias> | default | <harness> | <harness>/<model>   [#<effort>]
```

- `<harness>` ∈ `codex`, `claude`, `opencode`.
- For OpenCode the model part itself contains a slash: `opencode/github-copilot/gpt-6.1-terra`.
- `#effort` ∈ `low`, `medium`, `high`, `xhigh`, `max` (whatever the harness accepts). Body
  `reasoning_effort` takes precedence.
- Bare harness names use `harness.<name>.default_model`; `default` uses `models.default`.
- Aliases from `[models.aliases]` are expanded first and may chain.
- With `models.fallback_to_default = true`, unknown names route to `default` instead of failing.

## Response headers

| Header | Meaning |
| --- | --- |
| `X-Request-Id` | Echoed from the request or generated (UUIDv7). Appears in the server log for every turn. |
| `X-Locum-Harness` | `codex` · `claude` · `opencode`. |
| `X-Locum-Resolved-Model` | `harness/model` after alias and default resolution. |
| `X-Locum-Usage` | `exact` when token counts came from the harness, `estimated` (≈ chars/4) otherwise. Non-streaming only. |

## Errors

Always the OpenAI envelope:

```json
{ "error": { "message": "`n` is not supported by locum", "type": "invalid_request_error", "param": "n", "code": "unsupported_parameter" } }
```

| HTTP | `type` | `code` | When |
| --- | --- | --- | --- |
| 400 | `invalid_request_error` | — | malformed JSON, missing/empty `messages`, bad role |
| 400 | `invalid_request_error` | `unsupported_parameter` | tools, `n>1`, logprobs, audio, … |
| 400 | `invalid_request_error` | `context_length_exceeded` | prompt over `limits.max_prompt_chars`, or the harness reported a context-window error |
| 400 | `invalid_request_error` | `harness_rejected` | the harness rejected the request (e.g. bad model id for OpenCode) |
| 401 | `authentication_error` | `invalid_api_key` | `auth.keys` set and no match |
| 404 | `invalid_request_error` | `model_not_found` | unknown model / disabled harness / unknown route |
| 429 | `rate_limit_error` | `server_busy` | no in-flight slot within `limits.queue_timeout` |
| 429 | `rate_limit_error` | `harness_rate_limited` | the provider rate-limited the harness |
| 502 | `api_error` | `harness_error` | the harness ran but the turn failed |
| 503 | `api_error` | `harness_unavailable` | binary missing, not logged in, or failed to start |
| 504 | `api_error` | `harness_timeout` | first-token or request timeout |

Harness stderr is never returned to clients; a bounded excerpt is logged with the request id.
