#!/usr/bin/env bash
# Print the Homebrew formula for a macOS tarball.
# Usage: packaging/render-formula.sh <tarball> <download-url>
set -euo pipefail
tarball=$1 url=$2
here=$(cd "$(dirname "$0")" && pwd)
version=$("$here/version.sh")
if command -v sha256sum > /dev/null; then
  sha=$(sha256sum "$tarball" | cut -d' ' -f1)
else
  sha=$(shasum -a 256 "$tarball" | cut -d' ' -f1)
fi
sed -e "s|@VERSION@|$version|" -e "s|@URL@|$url|" -e "s|@SHA256@|$sha|" "$here/homebrew/rterm.rb"
