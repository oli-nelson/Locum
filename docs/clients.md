# Client recipes

Anything that can talk to OpenAI can talk to locum. Use these settings everywhere:

| Setting | Value |
| --- | --- |
| Base URL | `http://127.0.0.1:11435/v1` |
| API key | anything (e.g. `locum`) unless you configured `auth.keys` |
| Model | `codex`, `claude/sonnet`, `opencode/openai/gpt-6.1-sol#medium`, or an alias |

Tip: many apps hard-code model names or validate them against a list. Either pick from
`GET /v1/models` (locum publishes everything it can serve) or add
[aliases](configuration.md#modelsaliases) such as `"gpt-4o" = "codex/gpt-6-luna"`.

---

## SDKs

### Python (`openai`)

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:11435/v1", api_key="locum")

# non-streaming
r = client.chat.completions.create(
    model="opencode/openai/gpt-6.1-sol#medium",
    messages=[{"role": "system", "content": "Be terse."},
              {"role": "user", "content": "What is a locum tenens?"}],
)
print(r.choices[0].message.content, r.usage)

# streaming with usage
with client.chat.completions.stream(
    model="codex#low",
    messages=[{"role": "user", "content": "Count to five."}],
    stream_options={"include_usage": True},
) as s:
    for ev in s:
        if ev.type == "content.delta":
            print(ev.delta, end="", flush=True)

# JSON mode
r = client.chat.completions.create(
    model="claude/haiku",
    response_format={"type": "json_object"},
    messages=[{"role": "user", "content": "Return {\"ok\": true}"}],
)
```

### Node / TypeScript (`openai`)

```ts
import OpenAI from "openai";

const openai = new OpenAI({ baseURL: "http://127.0.0.1:11435/v1", apiKey: "locum" });

const stream = await openai.chat.completions.create({
  model: "opencode/github-copilot/claude-sonnet-5",
  messages: [{ role: "user", content: "hello" }],
  stream: true,
});
for await (const chunk of stream) process.stdout.write(chunk.choices[0]?.delta?.content ?? "");
```

### Vercel AI SDK

```ts
import { createOpenAI } from "@ai-sdk/openai";
import { streamText } from "ai";

const locum = createOpenAI({ baseURL: "http://127.0.0.1:11435/v1", apiKey: "locum" });
const { textStream } = streamText({ model: locum("codex/gpt-6-luna"), prompt: "hello" });
```

### LangChain (Python)

```python
from langchain_openai import ChatOpenAI
llm = ChatOpenAI(base_url="http://127.0.0.1:11435/v1", api_key="locum", model="claude/sonnet")
llm.invoke("hello")
```

### curl

```sh
curl -N http://127.0.0.1:11435/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"codex","stream":true,"messages":[{"role":"user","content":"hi"}]}'
```

---

## Apps

### Open WebUI

Settings → Connections → OpenAI API: URL `http://127.0.0.1:11435/v1`, key `locum`. Or via
environment when running it directly:

```sh
OPENAI_API_BASE_URL=http://127.0.0.1:11435/v1 OPENAI_API_KEY=locum open-webui serve
```

Running Open WebUI **in Docker**: the container cannot reach your host's loopback. Bind locum on
all interfaces with a key and use `host.docker.internal`:

```toml
[server]
bind = "0.0.0.0:11435"
[auth]
keys = ["sk-local-change-me"]
```

```sh
docker run -e OPENAI_API_BASE_URL=http://host.docker.internal:11435/v1 -e OPENAI_API_KEY=sk-local-change-me …
```

### Continue (VS Code / JetBrains)

`~/.continue/config.yaml`:

```yaml
models:
  - name: Codex via locum
    provider: openai
    model: codex/gpt-6-luna#medium
    apiBase: http://127.0.0.1:11435/v1
    apiKey: locum
  - name: Claude via locum
    provider: openai
    model: claude/sonnet
    apiBase: http://127.0.0.1:11435/v1
    apiKey: locum
```

### Zed

`settings.json`:

```json
{
  "language_models": {
    "openai": {
      "api_url": "http://127.0.0.1:11435/v1",
      "available_models": [
        { "name": "codex/gpt-6-luna", "display_name": "Codex (locum)", "max_tokens": 200000 },
        { "name": "claude/sonnet",   "display_name": "Claude (locum)", "max_tokens": 200000 }
      ]
    }
  }
}
```

Set the OpenAI API key in Zed's assistant settings to any value.

### aider

```sh
export OPENAI_API_BASE=http://127.0.0.1:11435/v1
export OPENAI_API_KEY=locum
aider --model openai/codex/gpt-6-luna#high --no-stream   # or drop --no-stream; streaming works
```

aider prefixes OpenAI-compatible models with `openai/`. Add `--edit-format whole` if diffs
misbehave – the harnesses are tuned for chat, not aider's diff format.

### Cursor

Cursor Settings → Models → **OpenAI API Key**: enter any key, tick *Override OpenAI Base URL* and
set `http://127.0.0.1:11435/v1`. Add a custom model name such as `codex/gpt-6-luna`. Note that
Cursor's agent features use tool calls, which locum rejects; use it for chat.

### Shell one-liner

```sh
ask() { curl -s http://127.0.0.1:11435/v1/chat/completions -H 'content-type: application/json' \
  -d "$(jq -nc --arg m "${LOCUM_MODEL:-codex#low}" --arg q "$*" \
      '{model:$m,messages:[{role:"user",content:$q}]}')" | jq -r '.choices[0].message.content'; }
ask "what does 'locum' mean?"
```

---

## Model-name cheat sheet

| You want | `model` |
| --- | --- |
| Whatever is configured as default | `default` |
| Codex, default model, high effort | `codex#high` |
| A specific Codex model | `codex/gpt-6-luna` |
| Claude Opus / Sonnet / Haiku | `claude/opus`, `claude/sonnet`, `claude/haiku` |
| Copilot-backed Claude via OpenCode | `opencode/github-copilot/claude-sonnet-5` |
| OpenAI GPT-6.1 Sol via OpenCode, medium effort | `opencode/openai/gpt-6.1-sol#medium` |
| Something your client hard-codes | add it to `[models.aliases]` |

## Things to know

- **Conversations are stateless.** Each request carries the full history; locum renders it into a
  single prompt. Enable `[session_cache]` to resume harness sessions when the history prefix
  matches exactly.
- **No tool calling.** Requests with `tools` get a `400`. If your client always sends an empty
  `tools` array or you just want it ignored, set `compat.ignore_tools = true`.
- **Token counts.** Exact when the harness reports them (Codex, Claude, OpenCode sessions);
  estimated for OpenCode one-shot requests. Check `X-Locum-Usage`.
- **Model names in responses.** `model` echoes what you sent; `X-Locum-Resolved-Model` has the
  resolved `harness/model`.
