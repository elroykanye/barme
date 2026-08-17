#!/usr/bin/env bash
# End-to-end proof for standard-SDK SigV4 query-string presigned URLs.
set -uo pipefail

HOST=127.0.0.1
S3=9000
NATIVE=7373
AK=presigntest
SK=presigntest-secret-key
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${BARMED:-/tmp/barme-target/debug/barmed}"
PYTHON="${PYTHON:-/tmp/barme-presign-venv/bin/python}"
[[ -x "$BIN" ]] || { echo "no barmed at $BIN (set BARMED=)"; exit 1; }
"$PYTHON" -c "import boto3" >/dev/null 2>&1 \
  || { echo "boto3 is unavailable at $PYTHON (set PYTHON=)"; exit 1; }

DATA="$(mktemp -d)"
SCRATCH="$(mktemp -d)"
CFG="$SCRATCH/barme.toml"
SRV=""
cat >"$CFG" <<EOF
data_dir = "$DATA"
[credentials]
access_key = "$AK"
secret_key = "$SK"
EOF

cleanup() {
  [[ -n "$SRV" ]] && kill "$SRV" 2>/dev/null
  rm -rf "$DATA" "$SCRATCH"
}
trap cleanup EXIT

BARME_CONFIG="$CFG" "$BIN" >"$SCRATCH/barmed.log" 2>&1 &
SRV=$!
for _ in $(seq 1 100); do
  curl -s "http://$HOST:$NATIVE/health" >/dev/null 2>&1 && break
  kill -0 "$SRV" 2>/dev/null \
    || { echo "server died:"; cat "$SCRATCH/barmed.log"; exit 1; }
  sleep 0.1
done

AK="$AK" SK="$SK" ENDPOINT="http://$HOST:$S3" "$PYTHON" \
  "$here/scripts/s3_presign.py"
