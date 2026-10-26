#!/usr/bin/env bash
# One-time: create a self-signed code-signing identity "WuwaTappo Local Signing" in the login keychain.
# macOS ties the Input Monitoring grant to the app's code identity. With this identity, rebuilt apps keep
# the same identity and the grant survives rebuilds. Remove it any time in Keychain Access.
set -euo pipefail
NAME="WuwaTappo Local Signing"
if security find-identity -p codesigning 2>/dev/null | grep -q "$NAME"; then
  echo "'$NAME' already exists"; exit 0
fi
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
openssl req -x509 -newkey rsa:2048 -keyout "$TMP/key.pem" -out "$TMP/cert.pem" -days 3650 -nodes \
  -subj "/CN=$NAME" -addext "extendedKeyUsage=codeSigning" -addext "keyUsage=critical,digitalSignature" 2>/dev/null
PASS="$(openssl rand -hex 12)"
openssl pkcs12 -export -legacy -inkey "$TMP/key.pem" -in "$TMP/cert.pem" -out "$TMP/id.p12" -passout "pass:$PASS" 2>/dev/null \
  || openssl pkcs12 -export -inkey "$TMP/key.pem" -in "$TMP/cert.pem" -out "$TMP/id.p12" -passout "pass:$PASS"
security import "$TMP/id.p12" -k "$HOME/Library/Keychains/login.keychain-db" -P "$PASS" -T /usr/bin/codesign
echo "created '$NAME'. The first codesign may ask for your login password; choose 'Always Allow'."
