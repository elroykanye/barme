#!/usr/bin/env bash
#
# End-to-end proof that S3 object listing works against a real signed client,
# not just the in-process router. Boots barmed, writes more objects than fit in
# one response, then drives ListObjectsV2 over real SigV4: pages the pot with the
# server's own continuation tokens, groups by delimiter, and mirrors a prefix by
# listing it and reading every object back.
#
# What this catches that the unit tests can't: a signature that only agrees when
# the query string is trivial (listing sends prefix, delimiter and a token, and
# they have to canonicalize the same way on both sides), and keys that survive a
# hand-rolled round trip but not a real one.
#
# Runs in WSL/Linux. Needs curl and python3 — no third-party packages.
#
set -uo pipefail

HOST=127.0.0.1
S3=9000
NATIVE=7373
AK=listingtest
SK=listingtest-secret-key
N="${N:-1150}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${BARMED:-$HOME/barme-target/release/barmed}"
[[ -x "$BIN" ]] || { echo "no barmed at $BIN (set BARMED=)"; exit 1; }

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

cleanup() { [[ -n "$SRV" ]] && kill -9 "$SRV" 2>/dev/null; rm -rf "$DATA" "$SCRATCH"; }
trap cleanup EXIT

BARME_CONFIG="$CFG" "$BIN" >"$SCRATCH/barmed.log" 2>&1 &
SRV=$!
for _ in $(seq 1 100); do
  curl -s "http://$HOST:$NATIVE/health" >/dev/null 2>&1 && break
  kill -0 "$SRV" 2>/dev/null || { echo "server died:"; cat "$SCRATCH/barmed.log"; exit 1; }
  sleep 0.1
done

echo "== driving ListObjectsV2 over SigV4 with $N objects =="
AK="$AK" SK="$SK" HOST="$HOST:$S3" N="$N" python3 "$here/scripts/s3_listing.py"
rc=$?

if [[ "$rc" -ne 0 ]]; then
  echo "FAIL: listing driver reported failures"
  exit 1
fi

# A listing that walked thousands of keys shouldn't have left the server unwell.
curl -sf "http://$HOST:$NATIVE/health" >/dev/null 2>&1 \
  && echo "PASS: listing works over real SigV4; server healthy" \
  || { echo "FAIL: server unhealthy after listing"; exit 1; }
