#!/usr/bin/env bash
# One-time setup: creates a self-signed code-signing certificate (10 years, codeSigning EKU)
# named "RD Local Code Signing" (override with RD_SIGN_IDENTITY) and imports it with its key
# into the login keychain, usable by /usr/bin/codesign. Signing every build with it keeps the
# app's designated requirement stable, so TCC grants survive rebuilds. (Keychain item access
# does not: a self-signed certificate has no Team ID, so the agent keeps its token in its
# config file instead.)
set -euo pipefail

NAME="${RD_SIGN_IDENTITY:-RD Local Code Signing}"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"
# The system LibreSSL writes PKCS#12 files that `security import` accepts.
OPENSSL=/usr/bin/openssl

if security find-identity -p codesigning "$KEYCHAIN" | grep -Fq "\"$NAME\""; then
  echo "code-signing identity \"$NAME\" already exists"
  exit 0
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

cat > "$WORK/cert.conf" <<EOF
[ req ]
distinguished_name = dn
x509_extensions = ext
prompt = no
[ dn ]
CN = $NAME
[ ext ]
basicConstraints = critical, CA:false
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
subjectKeyIdentifier = hash
EOF

"$OPENSSL" req -x509 -newkey rsa:2048 -nodes -days 3650 -config "$WORK/cert.conf" \
  -keyout "$WORK/key.pem" -out "$WORK/cert.pem" 2>/dev/null

PASS="$("$OPENSSL" rand -hex 16)"
if "$OPENSSL" version | grep -q '^OpenSSL 3'; then
  # OpenSSL 3 defaults to PKCS#12 ciphers `security` cannot read.
  "$OPENSSL" pkcs12 -export -legacy -inkey "$WORK/key.pem" -in "$WORK/cert.pem" -name "$NAME" \
    -out "$WORK/identity.p12" -passout "pass:$PASS"
else
  "$OPENSSL" pkcs12 -export -inkey "$WORK/key.pem" -in "$WORK/cert.pem" -name "$NAME" \
    -out "$WORK/identity.p12" -passout "pass:$PASS"
fi

security import "$WORK/identity.p12" -k "$KEYCHAIN" -P "$PASS" -T /usr/bin/codesign

echo "created code-signing identity \"$NAME\":"
security find-identity -p codesigning "$KEYCHAIN" | grep -F "\"$NAME\""
cat <<EOF

The certificate is self-signed and therefore shown as "not trusted"; codesign can still use it.
If codesign fails with errSecInternalComponent or keeps asking for the key, allow it once with:
  security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k <login password> "$KEYCHAIN"
EOF
