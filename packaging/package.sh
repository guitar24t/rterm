#!/usr/bin/env bash
# Build the .deb, .rpm and tarball for one architecture.
# Usage: packaging/package.sh <binary> <amd64|arm64> <output-dir>
set -euo pipefail
binary=$(realpath "$1") arch=$2 out=$3
cd "$(dirname "$0")/.."
version=$(packaging/version.sh)
mkdir -p "$out"

mkdir -p target/package
install -m 0755 "$binary" target/package/rterm
export VERSION=$version ARCH=$arch
nfpm package --config packaging/nfpm.yaml --packager deb --target "$out/"
nfpm package --config packaging/nfpm.yaml --packager rpm --target "$out/"

case $arch in
  amd64) triple=x86_64-linux ;;
  arm64) triple=aarch64-linux ;;
  *) echo "unknown arch $arch" >&2; exit 1 ;;
esac
stage=$(mktemp -d)
mkdir "$stage/rterm-$version"
cp "$binary" "$stage/rterm-$version/rterm"
install -m 0755 contrib/rterm-connect.py "$stage/rterm-$version/rterm-connect"
cp README.md LICENSE "$stage/rterm-$version/"
tar -C "$stage" -czf "$out/rterm-$version-$triple.tar.gz" "rterm-$version"
rm -rf "$stage"
ls -l "$out"
