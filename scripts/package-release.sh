#!/usr/bin/env bash
# Archive a release binary with its README. Never ships debug or test builds.
set -Eeuo pipefail
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "$script_dir/release-version.sh"
fail() { echo "error: $*" >&2; exit 1; }
[[ $# == 2 ]] || fail 'Usage: package-release.sh <version> <platform>'
version=$(release_version "$1")
platform=$2
cd "$script_dir/.."
binary=locum
case "$platform" in
  linux-x64) target=x86_64-unknown-linux-musl ;;
  linux-arm64) target=aarch64-unknown-linux-musl ;;
  macos-x64) target=x86_64-apple-darwin ;;
  macos-arm64) target=aarch64-apple-darwin ;;
  windows-x64) target=x86_64-pc-windows-msvc; binary=locum.exe ;;
  *) fail 'unsupported release platform' ;;
esac
built="target/$target/release/$binary"
[[ -s $built ]] || fail "missing release binary $built"
name="locum-$version-$platform"
archive=$(release_archive "$version" "$platform")
stage="artifacts/stage"
rm -rf "$stage/$name"
mkdir -p "$stage/$name" artifacts/release
cp "$built" "$stage/$name/$binary"
cp README.md "$stage/$name/README.md"
[[ $binary == *.exe ]] || chmod 755 "$stage/$name/$binary"
rm -f "artifacts/release/$archive"
case "$archive" in
  *.tar.gz)
    tar -czf "artifacts/release/$archive" -C "$stage" "$name"
    listing=$(tar -tzf "artifacts/release/$archive") ;;
  *.zip)
    if command -v zip >/dev/null; then
      (cd "$stage" && zip -qr "../release/$archive" "$name")
    elif command -v 7z >/dev/null; then
      (cd "$stage" && 7z a -tzip -bso0 -bsp0 "../release/$archive" "$name")
    else
      fail 'zip or 7z is required to package Windows releases'
    fi
    if command -v unzip >/dev/null; then
      listing=$(unzip -Z1 "artifacts/release/$archive")
    else
      listing=$(7z l -ba -slt "artifacts/release/$archive" | sed -n 's/^Path = //p' | tr -d '\r' | tr '\\' '/')
    fi ;;
esac
for file in "$binary" README.md; do
  grep -Fxq "$name/$file" <<< "$listing" || fail "archive is missing $name/$file"
done
printf '%s\n' "$listing"
