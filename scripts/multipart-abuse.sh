#!/usr/bin/env bash
#
# End-to-end proof that multipart error paths don't leak chunks. Boots barmed
# under aggressive GC (grace 0, sweep every 1s), drives a batch of abusive
# multipart uploads over real SigV4 (each a fresh random part, so nothing dedups),
# then checks that the chunk store does NOT grow: a leaked pin would keep those
# orphan chunks off GC's radar, so the on-disk chunks/ dir would balloon.
#
# With the pin-leak bug: ~30 leaked cycles x ~300 KB = megabytes stranded.
# With the fix: orphans are unpinned, GC reclaims them, chunks/ stays tiny.
#
# Runs in WSL/Linux. Needs curl, python3, du, awk.
#
set -uo pipefail

HOST=127.0.0.1
S3=9000
NATIVE=7373
AK=abusetest
SK=abusetest-secret-key
N="${N:-60}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${BARMED:-$HOME/barme-target/release/barmed}"
[[ -x "$BIN" ]] || { echo "no barmed at $BIN (set BARMED=)"; exit 1; }

DATA="$(mktemp -d)"
SCRATCH="$(mktemp -d)"
CFG="$SCRATCH/barme.toml"
SRV=""
cat >"$CFG" <<EOF
data_dir = "$DATA"
gc_grace_secs = 0
gc_interval_secs = 1
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

chunks_bytes() { du -sb "$DATA/chunks" 2>/dev/null | awk '{print $1+0}'; }

echo "== driving $N abusive multipart cycles over SigV4 =="
AK="$AK" SK="$SK" HOST="$HOST:$S3" N="$N" python3 "$here/scripts/s3_multipart_abuse.py"
abuse_rc=$?
[[ "$abuse_rc" -ne 0 ]] && { echo "FAIL: abuse driver reported bad statuses"; exit 1; }

# Let aggressive GC run several sweeps to reclaim the orphaned part chunks.
echo "== waiting for GC to reclaim orphans (grace=0, sweep=1s) =="
prev=$(chunks_bytes)
stable=0
for i in $(seq 1 15); do
  sleep 1
  cur=$(chunks_bytes)
  [[ "$cur" -eq "$prev" ]] && stable=$((stable+1)) || stable=0
  prev=$cur
  [[ "$stable" -ge 3 ]] && break
done

final=$(chunks_bytes)
sweeps=$(grep -c 'gc:' "$SCRATCH/barmed.log" 2>/dev/null || echo 0)
echo "chunks/ after abuse + GC: $final bytes over ~$sweeps sweeps"

# The only legitimately-retained bytes are the one clean good.bin (~550 KB).
# A leak of ~30 cycles would leave multiple MB. Threshold well between them.
LIMIT=2000000
if [[ "$final" -lt "$LIMIT" ]]; then
  echo "PASS: chunk store stayed small ($final < $LIMIT bytes); no pin leak under multipart abuse."
  # Server still healthy?
  curl -sf "http://$HOST:$NATIVE/health" >/dev/null 2>&1 && echo "server healthy" || { echo "FAIL: server unhealthy"; exit 1; }
  exit 0
else
  echo "FAIL: chunk store grew to $final bytes (>= $LIMIT) — orphans not reclaimed, likely a pin leak."
  exit 1
fi
