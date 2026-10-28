# Agent Development Guide

## Source of truth

- Read `SPEC.md` before working. Keep product requirements, architecture and
  unresolved design decisions there—not in this file. User-facing behaviour is
  documented in `README.md` and `docs/`.
- Keep this file limited to actionable development workflow and repository
  knowledge that helps future sessions. Do not duplicate the specification or
  maintain a chronological session log here.

## Development setup

- Rust crate with a single `locum` binary; `rust-version` is in `Cargo.toml`. CI pins
  its toolchain through `RUST_TOOLCHAIN` in `.github/workflows/{validate,release}.yml`.
- Checks (all run in CI on Linux, macOS and Windows):
  `cargo fmt --all -- --check`,
  `cargo clippy --locked --all-targets --all-features -- -D warnings`,
  `cargo test --locked --all-features`.
- Tests use the fake harness (`fake` feature or `LOCUM_FAKE_HARNESS=1`) and never
  call real models. Real harness runs (`locum doctor --live`) are opt-in because they
  transmit data and may incur cost.
- Keep non-Unix builds compiling without warnings: guard Unix-only APIs (process
  groups, signals, Unix sockets, symlinks) with `#[cfg(unix)]` and give the
  `#[cfg(not(unix))]` path explicit behaviour. Windows is only validated in CI.
- Release automation lives in `scripts/` and is tested by `bash scripts/test-release.sh`.
  Run it after changing any release script or workflow. See `docs/releases.md`.

## Working practices

- When making changes, update the `Unreleased` section of [CHANGELOG.md](CHANGELOG.md)
  with concise, user-facing entries for noteworthy changes (features, fixes,
  breaking changes, or meaningful operational/documentation changes). Skip routine
  internal churn and avoid duplicate entries. Consolidate related changes into
  coherent entries rather than listing each commit. Revise or remove existing
  Unreleased entries when later changes supersede them, so the section describes
  the current behavior rather than obsolete intermediate implementations. Keep
  noteworthy breaking changes and migration guidance explicit.
  Keep previously released sections unchanged; only after all release builds and
  tests pass does GitHub Actions promote `Unreleased` into a dated version section
  and create a new empty `Unreleased` section. Never promote the changelog, bump the
  version or create release tags by hand. The local release helper only dispatches
  that workflow. See [docs/releases.md](docs/releases.md) for release operations.
- Preserve unfamiliar user changes and keep work within the requested scope.
- Verify external CLI behavior against the installed version and official docs;
  do not guess flags. Document flags, quirks and tested CLI versions in
  `docs/harnesses.md` so they stay tied to the implementation.
- Report what was actually validated. Distinguish documentation review, automated
  tests, and platform-specific manual checks; never infer Windows or Linux
  validation from a macOS run.
