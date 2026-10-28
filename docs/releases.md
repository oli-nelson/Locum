# Releases

locum's release model: maintain `CHANGELOG.md` under `Unreleased`, build and test
an exact source commit on every platform, and only then promote the changelog, tag
and publish. No local release command edits files or pushes Git refs.

## Request a release

1. Review/consolidate `Unreleased` into concise user-facing notes. Keep released
   sections unchanged. The release fails if `Unreleased` has no entries.
2. Commit and push to the release branch. The branch must remain at that exact
   source commit until finalization; concurrent changes make publication fail safely.
3. Authenticate GitHub CLI, then run:

   ```sh
   bash scripts/release.sh 0.1.0
   # Or: bash scripts/release.sh 0.2.0-rc.1
   ```

   Alternatively use **Actions → Release → Run workflow**, select a branch, and
   enter the version (optional `v` prefix). The helper supplies an expected-SHA
   guard; it requires a clean checkout matching the branch on `origin`.
4. Follow the workflow. Never promote the changelog or create a release tag manually.

Versions are `X.Y.Z` or `X.Y.Z-{alpha,beta,rc}.N`, without leading zeroes.
Prereleases are marked as such and do not become GitHub's latest stable release.

## Automated gates and artifacts

`.github/workflows/release.yml` has three phases:

- **Validate:** test the release scripts hermetically, validate branch/version/source,
  reject existing tags, and prepare a read-only changelog/notes artifact.
- **Build:** each platform independently runs rustfmt, strict Clippy and the test
  suite for its target, builds the release binary with default features, checks that
  it reports the release version (and that Linux binaries are static), then packages it.

  | Platform | Runner | Target | Archive |
  | --- | --- | --- | --- |
  | `linux-x64` | `ubuntu-24.04` | `x86_64-unknown-linux-musl` | `.tar.gz` |
  | `linux-arm64` | `ubuntu-24.04-arm` | `aarch64-unknown-linux-musl` | `.tar.gz` |
  | `macos-x64` | `macos-15-intel` | `x86_64-apple-darwin` | `.tar.gz` |
  | `macos-arm64` | `macos-15` | `aarch64-apple-darwin` | `.tar.gz` |
  | `windows-x64` | `windows-latest` | `x86_64-pc-windows-msvc` | `.zip` |

- **Publish:** only after every build job succeeds, check the source branch again,
  commit only the promoted changelog, create annotated `v<version>`, and atomically
  push branch and tag with an exact old-value lease. Create a draft Release, upload
  all archives and `SHA256SUMS`, then publish it with changelog-derived notes.

Each archive contains a `locum-<version>-<platform>/` directory with the `locum`
(or `locum.exe`) binary and `README.md`. The version is written into `Cargo.toml`
and `Cargo.lock` **only in build checkouts**; the release commit changes only
`CHANGELOG.md`, so the repository's development version is not bumped automatically.

The Rust toolchain is pinned by `RUST_TOOLCHAIN` in both workflow files; update
them together.

**Binaries are unsigned; macOS binaries are not notarized.** On macOS, clear the
download quarantine before first run:

```sh
xattr -d com.apple.quarantine ./locum
```

Passing automated jobs does not validate real harness behaviour on each platform.
In particular, Windows is compile- and test-checked only.

## Repository setup and failure recovery

- Actions must be enabled, with access to the listed GitHub-hosted runners
  (including `ubuntu-24.04-arm` and `macos-15-intel`).
- The publish job uses `GITHUB_TOKEN` with `contents: write`. Repository rules must
  allow that job's changelog-only branch commit and annotated tag. No PAT, signing
  secret or protection bypass is configured; a rejected atomic push publishes nothing.
- Release runs are serialized, not cancelled by newer runs. Ordinary validation
  (`.github/workflows/validate.yml`) also tests the release scripts.
- If any build/test fails, the changelog and remote refs remain unchanged.
- If the branch advances, fix/review changes and request a new run from its latest
  commit. Do not force/reset the branch to make an old run publish.
- If upload/publication fails after the atomic push, use **Re-run failed jobs** in
  the **same workflow run**. Finalization verifies the original source parent,
  run identity, changelog and branch ancestry before resuming an owned draft.
  Re-running every job will reject the now-existing tag; use failed-job retries.
- Published assets are never overwritten. Only a draft belonging to the same run
  may have uploads replaced during recovery. A different run/version owner fails
  closed. Expired artifacts or conflicting state require deliberate manual recovery,
  not deleting tags or rewriting released changelog sections automatically.

## Local validation of automation

```sh
bash scripts/test-release.sh
```

Requires Bash, Git, jq, tar, zip/unzip and shasum. Tests use disposable local
remotes, fake binaries and a fake `gh`; they exercise version/changelog validation,
manifest version embedding, packaging, dispatch guards, branch races, rejected
pushes, draft-upload retry and immutable published releases. They do not build
binaries, contact GitHub, or validate a real hosted workflow.
