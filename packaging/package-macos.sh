#!/usr/bin/env bash
# Build the macOS tarball: one universal rterm binary (from the per-arch
# builds given) plus rterm-connect. Homebrew installs from this.
# Usage: packaging/package-macos.sh <output-dir> <binary>...
set -euo pipefail
out=$1
shift
binaries=()
for b in "$@"; do binaries+=("$(cd "$(dirname "$b")" && pwd)/$(basename "$b")"); done
mkdir -p "$out"
out=$(cd "$out" && pwd)
cd "$(dirname "$0")/.."
version=$(packaging/version.sh)

stage=$(mktemp -d)
dir="$stage/rterm-$version"
mkdir "$dir"
lipo -create -output "$dir/rterm" "${binaries[@]}"
lipo -info "$dir/rterm"
install -m 0755 contrib/rterm-connect.py "$dir/rterm-connect"
cp README.md LICENSE "$dir/"
tar -C "$stage" -czf "$out/rterm-$version-macos-universal.tar.gz" "rterm-$version"
rm -rf "$stage"
ls -l "$out"
