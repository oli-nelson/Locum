#!/usr/bin/env bash
# Print a rolled-over changelog without modifying the input.
set -Eeuo pipefail
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "$script_dir/release-version.sh"
[[ $# == 2 ]] || { echo 'Usage: release-changelog.sh <version> <CHANGELOG.md>' >&2; exit 1; }
version=$(release_version "$1")
awk -v version="$version" '
  /^## / {
    if (!first++ && $0 != "## [Unreleased]") bad = 1
    if ($0 == "## [Unreleased]") unreleased++
    if (index($0, "## [" version "]") == 1) duplicate = 1
  }
  END { exit !(unreleased == 1 && !bad && !duplicate) }
' "$2" || { echo 'error: expected one leading Unreleased section and no existing release version' >&2; exit 1; }
awk -v version="$version" -v date="$(date -u +%Y-%m-%d)" '
  $0 == "## [Unreleased]" { print; print ""; print "## [" version "] - " date; next }
  { print }
' "$2"
