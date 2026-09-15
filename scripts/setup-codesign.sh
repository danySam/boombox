#!/usr/bin/env bash
# One-time setup: create a self-signed code-signing identity for boombox's dev
# builds, in a keychain of its own so your login keychain stays untouched.
#
# Why: macOS pins a keychain item's ACL to the caller's code signature. An
# unsigned Rust binary is ad-hoc signed, so its identity is a hash of its own
# contents. A fixed certificate gives every build the same signing
# requirement. In practice the keychain still asked again after each rebuild,
# most likely because a self-signed certificate carries no Apple Team ID, so
# this does not stop the prompts. It only matters with `[auth] keyring = true`;
# by default boombox keeps tokens in a file and never asks.
#
# The final step needs your login password: `codesign` filters candidate
# identities by the code-signing trust policy, so an untrusted certificate is
# invisible to it.
set -euo pipefail

KEYCHAIN="$HOME/Library/Keychains/boombox-codesign.keychain-db"
CERT="$HOME/Library/Keychains/boombox-codesign.crt"
PASSWORD="${BOOMBOX_CODESIGN_PASSWORD:-boombox-codesign}"
NAME="BoomboxDev"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if [[ -f "$KEYCHAIN" ]]; then
  echo "==> keychain already exists at $KEYCHAIN, reusing it"
  # add-trusted-cert needs the certificate as a file. Export it back out of
  # the keychain if it went missing, so a half-finished setup can be resumed
  # without throwing away the identity.
  if [[ ! -f "$CERT" ]]; then
    echo "==> exporting the certificate from the keychain"
    security unlock-keychain -p "$PASSWORD" "$KEYCHAIN"
    security find-certificate -c "$NAME" -p "$KEYCHAIN" > "$CERT"
  fi
else
  echo "==> generating a self-signed code-signing certificate"
  openssl req -x509 -newkey rsa:2048 -nodes \
    -keyout "$TMP/key.pem" -out "$CERT" \
    -subj "/CN=$NAME" -days 3650 \
    -addext "keyUsage=critical,digitalSignature" \
    -addext "extendedKeyUsage=critical,codeSigning" \
    -addext "basicConstraints=critical,CA:false" 2>/dev/null

  # macOS `security import` cannot read OpenSSL 3's default AES-256-CBC
  # PKCS#12 encryption; it wants the legacy 3DES/SHA1 combination.
  openssl pkcs12 -export -inkey "$TMP/key.pem" -in "$CERT" \
    -out "$TMP/id.p12" -name "$NAME" \
    -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1 \
    -passout pass:import

  echo "==> creating a dedicated keychain"
  security create-keychain -p "$PASSWORD" "$KEYCHAIN"
  security set-keychain-settings "$KEYCHAIN"   # no auto-lock timeout
  security unlock-keychain -p "$PASSWORD" "$KEYCHAIN"

  # -T pre-authorises codesign to use the private key, so signing never asks.
  security import "$TMP/id.p12" -k "$KEYCHAIN" -P import -T /usr/bin/codesign -f pkcs12
  security set-key-partition-list -S apple-tool:,apple: -s -k "$PASSWORD" "$KEYCHAIN" >/dev/null
fi

echo
echo "==> trusting the certificate for code signing (asks for your password)"
security add-trusted-cert -r trustRoot -p codeSign "$CERT"

# codesign resolves signing identities through the keychain SEARCH LIST, not
# through its own --keychain flag; without this it reports "no identity found"
# even though `security find-identity -v -p codesigning` lists ours as valid.
echo
echo "==> adding the keychain to your search list"
search=()
while IFS= read -r line; do
  line="${line#"${line%%[![:space:]]*}"}"   # strip leading spaces
  line="${line#\"}"
  line="${line%\"}"
  [[ -n "$line" ]] && search+=("$line")
done < <(security list-keychains -d user)

already=0
for k in "${search[@]}"; do
  [[ "$k" == "$KEYCHAIN" ]] && already=1
done
if [[ $already -eq 0 ]]; then
  security list-keychains -d user -s "${search[@]}" "$KEYCHAIN"
fi
security list-keychains -d user

echo
security find-identity -v -p codesigning "$KEYCHAIN"
echo
echo "Done. Build with:  make build"
echo "Only relevant with [auth] keyring = true; by default tokens go to a file."
echo "To undo everything:  make unsign-teardown"
