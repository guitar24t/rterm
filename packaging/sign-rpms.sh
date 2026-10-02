#!/usr/bin/env bash
# Sign .rpm files in place.
# Environment: SIGNING_KEY (fingerprint, in the gpg keyring) and optionally
# GPG_PASSPHRASE_FILE.
set -euo pipefail
: "${SIGNING_KEY:?}"
args=(
  --define "__gpg $(command -v gpg)"
  --define "_gpg_name $SIGNING_KEY"
  --define "_gpg_digest_algo sha256"
)
if [[ -n "${GPG_PASSPHRASE_FILE:-}" ]]; then
  args+=(--define "_gpg_sign_cmd_extra_args --pinentry-mode loopback --passphrase-file $GPG_PASSPHRASE_FILE")
fi
rpmsign "${args[@]}" --addsign "$@"
