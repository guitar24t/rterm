#!/bin/bash
# Install rterm from a built repository site the way a user would (through
# install.sh, with signature checking on), then smoke-test and remove it.
# Run inside a distro container with the site mounted, e.g.:
#   docker run --rm -v "$PWD/site:/repo:ro" ubuntu:24.04 bash /repo-tests/test-install.sh /repo
set -euxo pipefail
site=${1:-/repo}
expected=$(cat "$site/VERSION")
. /etc/os-release
echo "=== $PRETTY_NAME on $(uname -m)"

if command -v apt-get > /dev/null; then
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  apt-get install -y -qq curl ca-certificates > /dev/null
fi

RTERM_REPO_URL="file://$site" sh "$site/install.sh"

[[ "$(rterm --version)" == "rterm $expected" ]]
rterm-connect --help | grep -q 'Choose an rterm session'  # needs python3 (recommended)
if command -v dpkg-query > /dev/null; then
  [[ "$(dpkg-query -W -f '${Version}' rterm)" == "$expected-1" ]]
  apt-cache policy rterm
  # The repository must stay verifiable: a refresh may not complain.
  apt-get update 2>&1 | tee /tmp/update.log
  ! grep -Eqi 'not signed|NO_PUBKEY|insecure|EXPKEYSIG|BADSIG' /tmp/update.log
  ! grep -E '^(W|E):' /tmp/update.log | grep -qi rterm
  test -f /etc/apt/apt.conf.d/51rterm-unattended-upgrades
else
  [[ "$(rpm -q --qf '%{VERSION}-%{RELEASE}' rterm)" == "$expected-1" ]]
  for f in "$site"/rpm/*/rterm-*.rpm; do
    rpm --checksig "$f" | grep -q 'digests signatures OK'
  done
  dnf -q --refresh repoinfo rterm
fi

# Smoke test: a detached session, listed and killed.
rterm new -d smoke -- sh -c 'echo hello; sleep 600'
rterm ls | tee /tmp/ls.txt
grep -q '^smoke ' /tmp/ls.txt
rterm kill smoke
for _ in $(seq 50); do
  rterm ls | grep -q 'no sessions' && break
  sleep 0.1
done
rterm ls | grep -q 'no sessions'

if command -v apt-get > /dev/null; then
  apt-get remove -y -qq rterm
else
  dnf remove -y -q rterm
fi
! command -v rterm
! command -v rterm-connect
echo "=== OK: $PRETTY_NAME on $(uname -m)"
