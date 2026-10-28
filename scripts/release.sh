#!/usr/bin/env bash
# Dispatch only: never edit files, commit, tag, or push.
set -Eeuo pipefail
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "$script_dir/release-version.sh"
[[ $# == 1 ]] || { echo 'Usage: bash scripts/release.sh <version>' >&2; exit 1; }
version=$(release_version "$1")
cd "$script_dir/.."
command -v gh >/dev/null || { echo 'error: install GitHub CLI and run gh auth login first' >&2; exit 1; }
[[ -z $(git status --porcelain) ]] || { echo 'error: commit and push your changes first' >&2; exit 1; }
branch=$(git symbolic-ref --quiet --short HEAD) || { echo 'error: select a branch, not detached HEAD' >&2; exit 1; }
source_sha=$(git rev-parse HEAD)
remote_sha=$(git ls-remote --heads origin "refs/heads/$branch" | cut -f1)
[[ $remote_sha == "$source_sha" ]] || { echo 'error: HEAD must match the branch on origin; push/pull explicitly first' >&2; exit 1; }
remote=$(git remote get-url origin)
case "$remote" in
  https://github.com/*) repository=${remote#https://github.com/} ;;
  git@*:*) repository=${remote#*:} ;;
  ssh://git@*/*) repository=${remote#ssh://git@*/} ;;
  *) echo 'error: origin must be a GitHub.com HTTPS or SSH remote' >&2; exit 1 ;;
esac
repository=${repository%.git}
[[ $repository =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || { echo 'error: cannot determine owner/repository from origin' >&2; exit 1; }
gh workflow run release.yml --repo "github.com/$repository" --ref "$branch" \
  -f "version=$version" -f "expected_sha=$source_sha"
echo "Requested locum $version from $branch at $source_sha. Follow Actions → Release."
echo 'No local files or Git refs were changed.'
