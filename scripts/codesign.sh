#!/usr/bin/env bash
# Sign a boombox binary with the stable local development identity, so every
# build carries the same signing requirement. See scripts/setup-codesign.sh for
# the one-time setup, and for why that does not stop keychain prompts.
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  exit 0  # Nothing to do off macOS.
fi

KEYCHAIN="${BOOMBOX_CODESIGN_KEYCHAIN:-$HOME/Library/Keychains/boombox-codesign.keychain-db}"
IDENTITY="${BOOMBOX_CODESIGN_IDENTITY:-BoomboxDev}"
TARGET="${1:-target/debug/boombox}"

if [[ ! -f "$KEYCHAIN" ]]; then
  # Not an error: a checkout without the identity should still build.
  echo "codesign.sh: no signing keychain, leaving $TARGET ad-hoc signed" >&2
  echo "             run scripts/setup-codesign.sh to stop the keychain prompts" >&2
  exit 0
fi

if [[ ! -f "$TARGET" ]]; then
  echo "codesign.sh: $TARGET does not exist" >&2
  exit 1
fi

security unlock-keychain -p "${BOOMBOX_CODESIGN_PASSWORD:-boombox-codesign}" "$KEYCHAIN"

# No --keychain here on purpose: codesign resolves identities through the
# keychain search list and reports "no identity found" if the keychain is only
# named on the command line. setup-codesign.sh puts it in the search list.
#
# A signing failure must not break the build -- an unsigned binary still runs,
# it just goes back to prompting on every rebuild.
if ! codesign --sign "$IDENTITY" --force --timestamp=none "$TARGET" 2>&1; then
  echo "codesign.sh: could not sign $TARGET as '$IDENTITY'" >&2
  echo "             finish setup with: make setup-codesign" >&2
  exit 0
fi
echo "signed $TARGET as $IDENTITY"
