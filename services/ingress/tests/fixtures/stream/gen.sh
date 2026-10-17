#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Test PKI for the core stream's mutual TLS (F-038, tests/stream.rs). Test values only: never a
# deployment's. Regenerate with `services/ingress/tests/fixtures/stream/gen.sh`.
#
#   ca.pem / ca.key            the operator's CA
#   ingress.pem / ingress.key  ingress's server certificate, for `ingress.test` and 127.0.0.1
#   core.pem / core.key        a core host's client certificate
#   rogue-ca.pem, rogue.pem / rogue.key   a client certificate from another CA
set -euo pipefail
cd "$(dirname "$0")"
DAYS=36500

ext() {
  cat <<EOF
basicConstraints = critical, CA:FALSE
keyUsage = critical, digitalSignature
extendedKeyUsage = $1
subjectAltName = $2
EOF
}

ca() { # name
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days "$DAYS" \
    -keyout "$1.key" -out "$1.pem" -subj "/CN=aledgent test $1" \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" \
    2>/dev/null
}

leaf() { # name ca eku san
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout "$1.key" \
    -out "$1.csr" -subj "/CN=aledgent test $1" 2>/dev/null
  openssl x509 -req -in "$1.csr" -CA "$2.pem" -CAkey "$2.key" -CAcreateserial -days "$DAYS" \
    -out "$1.pem" -extfile <(ext "$3" "$4") 2>/dev/null
  rm -f "$1.csr"
}

ca ca
ca rogue-ca
leaf ingress ca serverAuth "DNS:ingress.test,IP:127.0.0.1"
leaf core ca clientAuth "DNS:core.test"
leaf rogue rogue-ca clientAuth "DNS:core.test"
rm -f ./*.srl rogue-ca.key ca.key
for k in ./*.key; do openssl pkcs8 -topk8 -nocrypt -in "$k" -out "$k.p8" && mv "$k.p8" "$k"; done
