# Changelog

Noteworthy changes to locum are recorded here. Add entries under Unreleased;
the validated release workflow moves them into a dated version section and creates
a fresh Unreleased section. Released versions are listed newest first.

## [Unreleased]

- Add `locum`, an OpenAI-compatible HTTP server on `127.0.0.1:11435/v1` that answers
  requests through a locally installed Codex CLI, Claude Code or OpenCode, using your
  existing subscription instead of API keys.
- Support `GET /v1/models` (static plus live discovery), streaming and non-streaming
  `POST /v1/chat/completions`, and a legacy `POST /v1/completions` shim, including
  usage reporting, `response_format` JSON modes, `reasoning_effort`, `stop` and image
  inputs for Codex. Unsupported features such as tools and `n > 1` return a clear
  `400 unsupported_parameter`.
- Select harness, model and reasoning effort through the `model` string
  (e.g. `codex/gpt-6-luna#high`), with a configurable default and model aliases.
- Run each turn as a plain chat in an empty temporary directory with tools, MCP,
  hooks and session persistence disabled. A client disconnect interrupts the harness
  turn. Bind to loopback by default; non-loopback binds require bearer keys.
- Add `locum doctor [--live]`, `locum models` and `locum config print|path` commands.
- Add user documentation for configuration, client setup, harness details, the API
  and troubleshooting, plus an interactive tuios demo.
- Publish prebuilt, unsigned binaries for Linux (x64, ARM64; statically linked),
  macOS (Intel, Apple Silicon) and Windows x64. Windows builds are compiled and
  tested in CI only: Unix sockets and process-group cleanup are unavailable there,
  and `harness.codex.isolate_home` does not link Codex credentials.
