#!/bin/sh
# Install rterm from its package repository so the system package manager
# keeps it up to date.
#
#   curl -fsSL @BASE_URL@/install.sh | sudo sh
#
# Supports apt (Ubuntu, Debian) and dnf (RHEL, Rocky, Alma, Fedora).
# Set RTERM_NO_AUTO_UPDATE=1 to skip enabling unattended upgrades on apt.
set -eu

BASE=${RTERM_REPO_URL:-@BASE_URL@}

die() { echo "rterm install: $*" >&2; exit 1; }
fetch() {
  if command -v curl > /dev/null 2>&1; then
    curl -fsSL "$1"
  elif command -v wget > /dev/null 2>&1; then
    wget -qO- "$1"
  else
    die "curl or wget is required"
  fi
}

[ "$(id -u)" -eq 0 ] || die "must run as root (curl -fsSL $BASE/install.sh | sudo sh)"

if command -v apt-get > /dev/null 2>&1; then
  install -d -m 0755 /etc/apt/keyrings
  fetch "$BASE/rterm.gpg" > /etc/apt/keyrings/rterm.gpg.new
  chmod 0644 /etc/apt/keyrings/rterm.gpg.new
  mv /etc/apt/keyrings/rterm.gpg.new /etc/apt/keyrings/rterm.gpg
  cat > /etc/apt/sources.list.d/rterm.sources <<SRC
Types: deb
URIs: $BASE/apt
Suites: stable
Components: main
Signed-By: /etc/apt/keyrings/rterm.gpg
SRC
  if [ "${RTERM_NO_AUTO_UPDATE:-0}" != 1 ]; then
    # unattended-upgrades only installs updates from allowed origins.
    cat > /etc/apt/apt.conf.d/51rterm-unattended-upgrades <<'CONF'
// Let unattended-upgrades keep rterm up to date.
Unattended-Upgrade::Origins-Pattern { "origin=rterm,label=rterm"; };
CONF
  fi
  export DEBIAN_FRONTEND=noninteractive
  apt-get update
  apt-get install -y rterm
elif command -v dnf > /dev/null 2>&1; then
  cat > /etc/yum.repos.d/rterm.repo <<REPO
[rterm]
name=rterm
baseurl=$BASE/rpm
enabled=1
gpgcheck=1
repo_gpgcheck=1
gpgkey=$BASE/rterm.asc
metadata_expire=6h
REPO
  dnf install -y rterm
else
  die "unsupported system: need apt-get or dnf"
fi

echo
echo "rterm $(rterm --version | cut -d' ' -f2) installed. Run 'rterm' to start a session."
