#!/usr/bin/env bash
# Build the signed apt + dnf repository that GitHub Pages serves.
#
# Usage: packaging/build-repo.sh <packages-dir> <site-dir>
#   <packages-dir>  .deb files and signed .rpm files (searched recursively);
#                   every version found is published.
# Environment:
#   SIGNING_KEY          fingerprint of the signing key in the gpg keyring
#   GPG_PASSPHRASE_FILE  passphrase for it (optional)
#   BASE_URL             public URL of the site
set -euo pipefail
: "${SIGNING_KEY:?}" "${BASE_URL:?}"
pkgs=$(realpath "$1")
here=$(cd "$(dirname "$0")" && pwd)
rm -rf "$2"
mkdir -p "$2"
site=$(realpath "$2")

sign=(gpg --batch --yes --local-user "$SIGNING_KEY" --digest-algo SHA512)
if [[ -n "${GPG_PASSPHRASE_FILE:-}" ]]; then
  sign+=(--pinentry-mode loopback --passphrase-file "$GPG_PASSPHRASE_FILE")
fi

# --- apt: one "stable" suite; the binary is static, so it fits every release.
pool=$site/apt/pool/main/r/rterm
mkdir -p "$pool"
find "$pkgs" -name '*.deb' -exec cp -t "$pool" {} +
cd "$site/apt"
for arch in amd64 arm64; do
  dir=dists/stable/main/binary-$arch
  mkdir -p "$dir"
  apt-ftparchive --arch "$arch" packages pool > "$dir/Packages"
  gzip -9nk "$dir/Packages"
done
apt-ftparchive \
  -o APT::FTPArchive::Release::Origin=rterm \
  -o APT::FTPArchive::Release::Label=rterm \
  -o APT::FTPArchive::Release::Suite=stable \
  -o APT::FTPArchive::Release::Codename=stable \
  -o APT::FTPArchive::Release::Architectures="amd64 arm64" \
  -o APT::FTPArchive::Release::Components=main \
  -o APT::FTPArchive::Release::Description="rterm packages" \
  release dists/stable > "$site/Release.tmp"
mv "$site/Release.tmp" dists/stable/Release
"${sign[@]}" --clearsign --output dists/stable/InRelease dists/stable/Release
"${sign[@]}" --armor --detach-sign --output dists/stable/Release.gpg dists/stable/Release

# --- dnf: one repo for all architectures.
cd "$site"
mkdir -p rpm
rpmdb=$(mktemp -d)
gpg --armor --export "$SIGNING_KEY" > rterm.asc
rpm --dbpath "$rpmdb" --initdb
rpm --dbpath "$rpmdb" --import rterm.asc
while IFS= read -r -d '' f; do
  # Refuse to publish anything that isn't signed by our key.
  if ! rpm --dbpath "$rpmdb" --checksig "$f" | grep -q "digests signatures OK"; then
    echo "$f is not signed with $SIGNING_KEY" >&2
    exit 1
  fi
  arch=$(rpm -qp --nosignature --qf '%{ARCH}' "$f")
  mkdir -p "rpm/$arch"
  cp "$f" "rpm/$arch/"
done < <(find "$pkgs" -name '*.rpm' -print0)
rm -rf "$rpmdb"
createrepo_c --no-database --general-compress-type=gz --checksum=sha256 rpm > /dev/null
"${sign[@]}" --armor --detach-sign --output rpm/repodata/repomd.xml.asc rpm/repodata/repomd.xml

# --- keys, client configuration, installer, landing page.
gpg --export "$SIGNING_KEY" > rterm.gpg
fingerprint=$(gpg --with-colons --fingerprint "$SIGNING_KEY" | awk -F: '/^fpr/ {print $10; exit}')
version=$(find apt/pool -name '*.deb' -exec dpkg-deb -f {} Version \; | sed 's/-[^-]*$//' | sort -V | tail -n1)
echo "$version" > VERSION

cat > rterm.sources <<SRC
Types: deb
URIs: $BASE_URL/apt
Suites: stable
Components: main
Signed-By: /etc/apt/keyrings/rterm.gpg
SRC
cat > rpm/rterm.repo <<REPO
[rterm]
name=rterm
baseurl=$BASE_URL/rpm
enabled=1
gpgcheck=1
repo_gpgcheck=1
gpgkey=$BASE_URL/rterm.asc
metadata_expire=6h
REPO

grouped=$(echo "$fingerprint" | sed 's/.\{4\}/& /g; s/ $//')
subst() {
  sed -e "s|@BASE_URL@|$BASE_URL|g" -e "s|@VERSION@|$version|g" -e "s|@FINGERPRINT@|$grouped|g" "$1"
}
subst "$here/install.sh" > install.sh
subst "$here/index.html" > index.html
touch .nojekyll
echo "built repository for rterm $version in $site (key $fingerprint)"
