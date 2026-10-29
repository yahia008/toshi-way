#!/bin/sh
# Starts toshi-server. The server reads the Lightning node's certificate and macaroons from files, but
# a hosting platform keeps secrets in settings, so they arrive as base64 text and are written to files
# here, readable only by this process, on every start. Nothing secret is baked into the image.
set -eu

dir="${TOSHI_CRED_DIR:-/tmp/lnd}"
umask 077
mkdir -p "$dir"

# materialize SETTING_B64 file ENV_NAME: decode the setting into $dir/file and point ENV_NAME at it.
materialize() {
  eval "value=\${$1:-}"
  if [ -n "$value" ]; then
    printf '%s' "$value" | base64 -d > "$dir/$2" || { echo "$1 is not valid base64" >&2; exit 2; }
    export "$3=$dir/$2"
  fi
}

materialize LND_TLS_CERT_B64 tls.cert LND_TLS_CERT
materialize LND_MACAROON_B64 invoice.macaroon LND_MACAROON
materialize PAYOUT_MACAROON_B64 payout.macaroon PAYOUT_MACAROON

# The server no longer needs the encoded copies; keep them out of its environment.
unset LND_TLS_CERT_B64 LND_MACAROON_B64 PAYOUT_MACAROON_B64

exec /usr/local/bin/toshi-server
