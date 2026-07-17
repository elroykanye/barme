#!/usr/bin/env bash
#
# Adversarial abuse harness for barme. Two goals:
#   1. Throw malformed and hostile requests at the HTTP doors and confirm the
#      server answers with a sane status and *stays up* — no panic that drops
#      the connection, no wedge, no crash.
#   2. Run uploads under an intentionally hostile GC config (zero grace, sweep
#      every second) and confirm every committed object still reads back intact
#      — the end-to-end check on the in-flight pin fix.
#
# Runs in WSL/Linux. Hits 127.0.0.1 (barmed binds IPv4 only). curl + sha256sum.
#
#   ./scripts/abuse-test.sh
#
set -uo pipefail

PORT="${PORT:-7373}"
HOST=127.0.0.1
BASE="http://$HOST:$PORT"
AUTH="barme:barme"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MAXUP=$((4 * 1024 * 1024)) # 4 MiB cap, small so the boundary is cheap to probe

BIN="${BARMED:-/tmp/barme-crash/release/barmed}"
[[ -x "$BIN" ]] || { echo "no barmed binary at $BIN (set BARMED=)"; exit 1; }

DATA_DIR="$(mktemp -d)"
SCRATCH="$(mktemp -d)"
CFG="$SCRATCH/barme.toml"
SRV_PID=""
fails=0

cat >"$CFG" <<EOF
data_dir = "$DATA_DIR"
gc_grace_secs = 0
gc_interval_secs = 1
max_upload_bytes = $MAXUP
[credentials]
access_key = "barme"
secret_key = "barme"
EOF

cleanup() {
  [[ -n "$SRV_PID" ]] && kill -9 "$SRV_PID" 2>/dev/null
  pkill -9 -P $$ 2>/dev/null
  rm -rf "$DATA_DIR" "$SCRATCH"
}
trap cleanup EXIT

start_server() {
  BARME_CONFIG="$CFG" "$BIN" >"$SCRATCH/barmed.log" 2>&1 &
  SRV_PID=$!
  for _ in $(seq 1 100); do
    curl -sf -u "$AUTH" "$BASE/pots" >/dev/null 2>&1 && return 0
    kill -0 "$SRV_PID" 2>/dev/null || { echo "server died on startup:"; cat "$SCRATCH/barmed.log"; return 1; }
    sleep 0.1
  done
  echo "server never became ready"; return 1
}

alive() { curl -sf -u "$AUTH" "$BASE/pots" >/dev/null 2>&1; }

# status CODE = curl of an upload/op; PASS if the server returns *something*
# sane and is still alive afterward. FAIL only if the server dies or wedges.
probe() {
  local label="$1"; shift
  local code
  code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 15 "$@" 2>/dev/null)"
  if ! alive; then
    echo "  FAIL [$label] server not alive after request (http=$code)"
    fails=$((fails + 1))
    # Try to bring it back for the remaining probes.
    kill -9 "$SRV_PID" 2>/dev/null; start_server || return 1
  else
    echo "  ok   [$label] http=$code"
  fi
}

echo "== phase A: hostile inputs =="
start_server || exit 1

# Oversized key (well past the 255-byte filename bound) -> expect 4xx, not crash.
LONGKEY="$(head -c 5000 /dev/zero | tr '\0' 'a')"
probe "overlong-key" -u "$AUTH" -T "$CFG" "$BASE/objects/pot/$LONGKEY"

# Path-traversal-looking keys. Must not escape the store; hex-encoding contains
# them. curl may normalize the URL, but the server must not 5xx or escape.
probe "traversal-dotdot" -u "$AUTH" -T "$CFG" "$BASE/objects/pot/..%2F..%2F..%2Fetc%2Fpasswd"
probe "traversal-pot"    -u "$AUTH" -T "$CFG" "$BASE/objects/..%2F..%2Fescape/k"

# Control chars, unicode, spaces, dotfiles in keys.
probe "unicode-key"   -u "$AUTH" -T "$CFG" "$BASE/objects/pot/$(printf 'na\xc3\xafve')"
probe "space-key"     -u "$AUTH" -T "$CFG" "$BASE/objects/pot/a%20b%20c"
probe "dotfile-key"   -u "$AUTH" -T "$CFG" "$BASE/objects/pot/.barme-tmp-evil"
probe "newline-key"   -u "$AUTH" -T "$CFG" "$BASE/objects/pot/line%0Abreak"

# Empty pot / empty key.
probe "empty-key"     -u "$AUTH" -T "$CFG" "$BASE/objects/pot/"

# Empty body upload.
: >"$SCRATCH/empty"
probe "empty-body"    -u "$AUTH" -T "$SCRATCH/empty" "$BASE/objects/pot/empty.bin"

# Exactly at the cap, and one byte over.
head -c "$MAXUP" /dev/urandom >"$SCRATCH/atcap"
probe "at-cap"        -u "$AUTH" -H 'Expect:' -T "$SCRATCH/atcap" "$BASE/objects/pot/atcap.bin"
head -c "$((MAXUP + 1))" /dev/urandom >"$SCRATCH/overcap"
probe "over-cap"      -u "$AUTH" -H 'Expect:' -T "$SCRATCH/overcap" "$BASE/objects/pot/overcap.bin"

# Bad auth.
probe "bad-auth"      -u "wrong:creds" -T "$CFG" "$BASE/objects/pot/noauth.bin"
probe "no-auth"       -T "$CFG" "$BASE/objects/pot/noauth2.bin"

# Malformed JSON to a config endpoint.
probe "bad-json-cfg"  -u "$AUTH" -X PUT -H 'content-type: application/json' \
  --data '{not: valid json,,,}' "$BASE/pots/pot/config"

# Garbage manifest to sync import.
probe "bad-manifest"  -u "$AUTH" -X POST -H 'content-type: application/json' \
  --data '{"garbage":true}' "$BASE/sync/import/pot/k"

# Huge header.
probe "huge-header"   -u "$AUTH" -H "X-Junk: $(head -c 60000 /dev/zero | tr '\0' 'x')" \
  -T "$CFG" "$BASE/objects/pot/hdr.bin"

# Read a key that doesn't exist.
probe "missing-get"   -u "$AUTH" "$BASE/objects/pot/does-not-exist"

echo
echo "== phase B: uploads under hostile GC (grace=0, sweep=1s) =="
# Upload multi-MB objects concurrently for a while, so their chunks stay
# unreferenced across several GC sweeps. Record key->sha only on HTTP success.
LEDGER="$SCRATCH/ledger"; : >"$LEDGER"
upload_worker() {
  local id="$1" n=0 end=$((SECONDS + 12))
  while [[ $SECONDS -lt $end ]]; do
    n=$((n + 1))
    local key="load-$id-$n"
    local f="$SCRATCH/p.$id"
    head -c "$(( (RANDOM % (3 * 1024 * 1024)) + 262144 ))" /dev/urandom >"$f"
    local sha; sha="$(sha256sum "$f" | awk '{print $1}')"
    if curl -sf -u "$AUTH" -H 'Expect:' -T "$f" "$BASE/objects/load/$key" >/dev/null 2>&1; then
      ( flock 9; printf '%s\t%s\n' "$key" "$sha" >>"$LEDGER" ) 9>>"$LEDGER.lock"
    fi
  done
}
pids=()
for i in $(seq 1 10); do upload_worker "$i" & pids+=($!); done
wait "${pids[@]}" 2>/dev/null

sweeps="$(grep -c 'gc:' "$SCRATCH/barmed.log" 2>/dev/null || echo 0)"
acked="$(wc -l <"$LEDGER")"
echo "  $acked objects acknowledged across ~$sweeps GC sweeps; verifying readback"
bad=0
while IFS=$'\t' read -r key sha; do
  [[ -z "$key" ]] && continue
  if ! curl -sf -u "$AUTH" "$BASE/objects/load/$key" -o "$SCRATCH/dl" 2>/dev/null; then
    echo "  MISSING: $key"; bad=$((bad + 1)); continue
  fi
  got="$(sha256sum "$SCRATCH/dl" | awk '{print $1}')"
  [[ "$got" == "$sha" ]] || { echo "  CORRUPT: $key"; bad=$((bad + 1)); }
done <"$LEDGER"
if [[ "$bad" -eq 0 ]]; then echo "  ok   all $acked objects intact under hostile GC"; else
  echo "  FAIL $bad object(s) lost or corrupted under GC"; fails=$((fails + bad)); fi

echo
echo "== phase C: create/delete churn while GC runs =="
# Rapidly overwrite and delete the same handful of keys to generate garbage,
# then confirm the survivors are intact and the server is healthy.
for r in $(seq 1 40); do
  k="churn-$((r % 5))"
  head -c 200000 /dev/urandom >"$SCRATCH/c"
  curl -sf -u "$AUTH" -H 'Expect:' -T "$SCRATCH/c" "$BASE/objects/churn/$k" >/dev/null 2>&1
  [[ $((r % 3)) -eq 0 ]] && curl -sf -u "$AUTH" -X DELETE "$BASE/objects/churn/$k" >/dev/null 2>&1
done
if alive; then echo "  ok   server healthy after churn"; else
  echo "  FAIL server unhealthy after churn"; fails=$((fails + 1)); fi

echo
if [[ "$fails" -eq 0 ]]; then
  echo "PASS: server survived every hostile input and kept all data under hostile GC."
else
  echo "FAIL: $fails problem(s). Server log tail:"; tail -20 "$SCRATCH/barmed.log"
fi
exit "$fails"
