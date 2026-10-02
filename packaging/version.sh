#!/usr/bin/env bash
# Print the package version (from Cargo.toml). On a tag build, fail unless
# the tag is exactly "v<version>".
set -euo pipefail
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n1)
if [[ "${GITHUB_REF_TYPE:-}" == tag && "${GITHUB_REF_NAME:-}" != "v$version" ]]; then
  echo "tag ${GITHUB_REF_NAME} does not match Cargo.toml version $version" >&2
  exit 1
fi
echo "$version"
