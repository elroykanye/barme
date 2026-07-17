#!/usr/bin/env bash
#
# Crash-durability harness for barme.
#
# The contract under test: if a client received a success response for an upload,
# that object must survive a hard kill of the server, byte-for-byte, and the
# server must restart clean on the same data dir. This is what earns the v1
# durability claim.
#
# Each round: start barmed on a persistent data dir, hammer it with concurrent
# uploads, record only the uploads the server *acknowledged*, then `kill -9` it
# mid-write. On restart, every acknowledged object must download and match its
# recorded sha256, and the server must come up without tripping over any temp
# file a crashed write left behind.
#
# Runs in WSL/Linux (the deploy target). Hits 127.0.0.1, never localhost, because
# barmed binds IPv4 only. No deps beyond curl, sha256sum, awk, flock.
#
#   ROUNDS=20 ./scripts/crash-test.sh
#
set -uo pipefail

ROUNDS="${ROUNDS:-10}"
UPLOADERS="${UPLOADERS:-6}"
PORT="${PORT:-7373}"
HOST=127.0.0.1
BASE="http://$HOST:$PORT"
AUTH="barme:barme"
POT="crash"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Build once, off the /mnt/c mount so target IO stays on the Linux fs.
BIN="${BARMED:-}"
if [[ -z "$BIN" ]]; then
  echo "building barmed (release)..."
  CARGO_TARGET_DIR=/tmp/barme-crash cargo build --release -p barmed --manifest-path "$here/Cargo.toml" \
    || { echo "build failed"; exit 1; }
  BIN=/tmp/barme-crash/release/barmed
fi
[[ -x "$BIN" ]] || { echo "no barmed binary at $BIN"; exit 1; }

DATA_DIR="$(mktemp -d)"
LEDGER="$(mktemp)"     # acknowledged uploads: key<TAB>sha256
SCRATCH="$(mktemp -d)" # generated payloads and downloads
SRV_PID=""

# Optional: run GC hot (GC_GRACE=<secs>) so kills land while GC is sweeping and
# erasing — the crash-plus-collection combination. A committed object must still
# survive even if a crash interrupts a sweep mid-erase.
CFG_ARG=""
if [[ -n "${GC_GRACE:-}" ]]; then
  CFG="$SCRATCH/barme.toml"
  cat >"$CFG" <<EOF
data_dir = "$DATA_DIR"
gc_grace_secs = $GC_GRACE
gc_interval_secs = 1
[credentials]
access_key = "barme"
secret_key = "barme"
EOF
  CFG_ARG="$CFG"
  echo "GC hot: grace=${GC_GRACE}s, sweep=1s"
fi

cleanup() {
  [[ -n "$SRV_PID" ]] && kill -9 "$SRV_PID" 2>/dev/null
  pkill -9 -P $$ 2>/dev/null
  rm -rf "$DATA_DIR" "$SCRATCH" "$LEDGER"
}
trap cleanup EXIT

start_server() {
  if [[ -n "$CFG_ARG" ]]; then
    BARME_CONFIG="$CFG_ARG" "$BIN" >"$SCRATCH/barmed.log" 2>&1 &
  else
    BARME_DATA_DIR="$DATA_DIR" BARME_ACCESS_KEY=barme BARME_SECRET_KEY=barme \
      "$BIN" >"$SCRATCH/barmed.log" 2>&1 &
  fi
  SRV_PID=$!
  # Wait for readiness (up to ~10s).
  for _ in $(seq 1 100); do
    if curl -sf -u "$AUTH" "$BASE/pots" >/dev/null 2>&1; then return 0; fi
    if ! kill -0 "$SRV_PID" 2>/dev/null; then
      echo "server died on startup; log:"; cat "$SCRATCH/barmed.log"; return 1
    fi
    sleep 0.1
  done
  echo "server never became ready; log:"; cat "$SCRATCH/barmed.log"; return 1
}

# One uploader: forever, PUT a random-sized payload and, only on HTTP success,
# append its key+sha to the ledger. An upload cut off by the kill simply never
# reaches the ledger, so we never assert an unacknowledged write.
uploader() {
  local id="$1" n=0
  while true; do
    n=$((n + 1))
    local key="u${id}-${n}-$RANDOM"
    local f="$SCRATCH/payload.$id"
    # Sizes from a few KB to a few MB, to exercise single- and multi-chunk paths.
    local kb=$(( (RANDOM % 4096) + 4 ))
    head -c "$((kb * 1024))" /dev/urandom >"$f"
    local sha; sha="$(sha256sum "$f" | awk '{print $1}')"
    if curl -sf -u "$AUTH" -T "$f" "$BASE/objects/$POT/$key" >/dev/null 2>&1; then
      # PIPE_BUF-sized append under flock: never interleaves between uploaders.
      ( flock 9; printf '%s\t%s\n' "$key" "$sha" >>"$LEDGER" ) 9>>"$LEDGER.lock"
    fi
  done
}

verify_ledger() {
  local fail=0 total=0
  while IFS=$'\t' read -r key sha; do
    [[ -z "$key" ]] && continue
    total=$((total + 1))
    local out="$SCRATCH/dl"
    if ! curl -sf -u "$AUTH" "$BASE/objects/$POT/$key" -o "$out" 2>/dev/null; then
      echo "  MISSING after crash: $key"; fail=$((fail + 1)); continue
    fi
    local got; got="$(sha256sum "$out" | awk '{print $1}')"
    if [[ "$got" != "$sha" ]]; then
      echo "  CORRUPT after crash: $key (want $sha got $got)"; fail=$((fail + 1))
    fi
  done <"$LEDGER"
  echo "  verified $total acknowledged object(s), $fail bad"
  return $fail
}

echo "data dir: $DATA_DIR"
echo "rounds: $ROUNDS, uploaders: $UPLOADERS"
echo

overall=0
for round in $(seq 1 "$ROUNDS"); do
  echo "round $round/$ROUNDS"
  start_server || { overall=1; break; }

  # Recovery signal: after round 1 the log should note reaped temp files if the
  # previous kill stranded any. (Informational, not asserted — a kill between
  # rounds may land in a quiet moment.)
  if grep -q "recovered from unclean shutdown" "$SCRATCH/barmed.log"; then
    echo "  $(grep 'recovered from unclean shutdown' "$SCRATCH/barmed.log" | tail -1)"
  fi

  # Before hammering, every previously acknowledged object must still be intact.
  if [[ -s "$LEDGER" ]]; then
    verify_ledger || { echo "  DURABILITY FAILURE"; overall=1; break; }
  fi

  # Fan out uploaders, let them run briefly, then hard-kill everything.
  pids=()
  for i in $(seq 1 "$UPLOADERS"); do uploader "$i" & pids+=($!); done
  sleep "$(awk -v s="$RANDOM" 'BEGIN{srand(s); print 0.5 + rand()*2.0}')"
  for p in "${pids[@]}"; do kill -9 "$p" 2>/dev/null; done
  wait "${pids[@]}" 2>/dev/null

  # The crash itself: kill -9 the server, no chance to flush or clean up.
  kill -9 "$SRV_PID" 2>/dev/null; wait "$SRV_PID" 2>/dev/null; SRV_PID=""
  echo "  killed mid-write; $(wc -l <"$LEDGER") acknowledged so far"
done

# Final restart and full verification.
if [[ "$overall" -eq 0 ]]; then
  echo
  echo "final restart + full verify"
  if start_server; then
    if grep -q "recovered from unclean shutdown" "$SCRATCH/barmed.log"; then
      echo "  $(grep 'recovered from unclean shutdown' "$SCRATCH/barmed.log" | tail -1)"
    fi
    verify_ledger || overall=1
    # The server must also enumerate cleanly (no temp file breaking a walk).
    curl -sf -u "$AUTH" "$BASE/pots/$POT/objects" >/dev/null 2>&1 || {
      echo "  list failed after recovery"; overall=1; }
    kill -9 "$SRV_PID" 2>/dev/null; SRV_PID=""
  else
    overall=1
  fi
fi

echo
if [[ "$overall" -eq 0 ]]; then
  echo "PASS: every acknowledged object survived every crash."
else
  echo "FAIL: see above."
fi
exit "$overall"
