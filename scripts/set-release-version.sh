#!/usr/bin/env bash
# Build-checkout only: embed the release version in Cargo.toml and Cargo.lock.
set -Eeuo pipefail
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "$script_dir/release-version.sh"
[[ $# == 1 ]] || { echo 'Usage: set-release-version.sh <version>' >&2; exit 1; }
version=$(release_version "$1")
cd "$script_dir/.."
temp=$(mktemp -d)
trap 'rm -rf "$temp"' EXIT
# Validate every replacement before writing anything. Tolerate CRLF checkouts.
awk -v version="$version" '
  { line = $0; cr = sub(/\r$/, "", line) ? "\r" : "" }
  line ~ /^\[/ { section = line }
  section == "[package]" && !done && line ~ /^version = "[^"]*"$/ {
    print "version = \"" version "\"" cr; done = 1; next
  }
  { print }
  END { exit !done }
' Cargo.toml > "$temp/Cargo.toml" || { echo 'error: missing [package] version in Cargo.toml' >&2; exit 1; }
awk -v version="$version" '
  { line = $0; cr = sub(/\r$/, "", line) ? "\r" : "" }
  line == "[[package]]" { next_is_name = 1; locum = 0; print; next }
  next_is_name { locum = line == "name = \"locum\""; next_is_name = 0 }
  locum && line ~ /^version = "[^"]*"$/ { print "version = \"" version "\"" cr; done++; locum = 0; next }
  { print }
  END { exit done != 1 }
' Cargo.lock > "$temp/Cargo.lock" || { echo 'error: missing locum package version in Cargo.lock' >&2; exit 1; }
cp "$temp/Cargo.toml" Cargo.toml
cp "$temp/Cargo.lock" Cargo.lock
