#!/usr/bin/env bash
# Shared release version contract.
release_version() {
  local value=${1-}
  value=${value#v}
  local number='(0|[1-9][0-9]*)'
  local pattern="^${number}\\.${number}\\.${number}(-(alpha|beta|rc)\\.(0|[1-9][0-9]*))?$"
  if [[ $# != 1 || ! $value =~ $pattern ]]; then
    echo 'error: use X.Y.Z or X.Y.Z-{alpha,beta,rc}.N (optional v prefix, no leading zeroes)' >&2
    return 1
  fi
  printf '%s\n' "$value"
}

# Release platforms and their archive names. Keep in sync with release.yml.
release_platforms=(linux-x64 linux-arm64 macos-x64 macos-arm64 windows-x64)

release_archive() {
  local version=$1 platform=$2
  case "$platform" in
    windows-*) printf 'locum-%s-%s.zip\n' "$version" "$platform" ;;
    *) printf 'locum-%s-%s.tar.gz\n' "$version" "$platform" ;;
  esac
}
