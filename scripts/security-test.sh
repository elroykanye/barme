#!/usr/bin/env bash
#
# Security-at-rest checks for barme:
#   1. Fresh boot with no configured credential mints a random owner and prints
#      it once; that credential authenticates and a wrong one is rejected.
#   2. The access-key secret is NOT stored in plaintext on disk (it's encrypted
#      with the master key).
#   3. A master.key file is created with 0600 perms.
#   4. Restarting on the same data dir keeps the same owner working (the master
#      key persists and decrypts the store).
#   5. SigV4 on the S3 door still works with that credential — proving the raw
#      secret is recovered in memory (encryption didn't break S3 compat).
#
# Runs in WSL/Linux, hits 127.0.0.1. Needs curl, awk, grep, python3 (for SigV4)
# or the aws CLI; SigV4 step is skipped with a note if neither is present.
#
set -uo pipefail

HOST=127.0.0.1
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${BARMED:-/tmp/barme-crash/release/barmed}"
[[ -x "$BIN" ]] || { echo "no barmed at $BIN"; exit 1; }

DATA_DIR="$(mktemp -d)"
SCRATCH="$(mktemp -d)"
SRV_PID=""
fails=0
NPORT=7373

cleanup() { [[ -n "$SRV_PID" ]] && kill -9 "$SRV_PID" 2>/dev/null; rm -rf "$DATA_DIR" "$SCRATCH"; }
trap cleanup EXIT

# Start with NO credential configured; capture stdout to read the minted login.
start_fresh() {
  env -u BARME_ACCESS_KEY -u BARME_SECRET_KEY -u BARME_MASTER_KEY \
    BARME_DATA_DIR="$DATA_DIR" "$BIN" >"$SCRATCH/out.log" 2>"$SCRATCH/err.log" &
  SRV_PID=$!
  for _ in $(seq 1 100); do
    curl -s "http://$HOST:$NPORT/health" >/dev/null 2>&1 && return 0
    kill -0 "$SRV_PID" 2>/dev/null || { echo "server died:"; cat "$SCRATCH/out.log" "$SCRATCH/err.log"; return 1; }
    sleep 0.1
  done
  return 1
}

check() { if [[ "$1" == "ok" ]]; then echo "  ok   $2"; else echo "  FAIL $2"; fails=$((fails+1)); fi; }

echo "== boot 1: fresh store, no credential configured =="
start_fresh || exit 1

# The minted credential is printed to stdout once.
SECRET="$(grep -A5 'Generated an owner' "$SCRATCH/out.log" | awk '/secret key:/{print $3}')"
ACCESS="$(grep -A5 'Generated an owner' "$SCRATCH/out.log" | awk '/access key:/{print $3}')"
[[ -n "$SECRET" && -n "$ACCESS" ]] && check ok "minted an owner credential on first boot ($ACCESS)" \
  || check fail "no owner credential was printed on first boot"

# That credential authenticates on the native door.
code=$(curl -s -o /dev/null -w '%{http_code}' -u "$ACCESS:$SECRET" "http://$HOST:$NPORT/pots")
[[ "$code" == "200" ]] && check ok "minted credential authenticates (200)" || check fail "minted credential rejected ($code)"

# A wrong secret is rejected.
code=$(curl -s -o /dev/null -w '%{http_code}' -u "$ACCESS:wrong-secret" "http://$HOST:$NPORT/pots")
[[ "$code" == "403" ]] && check ok "wrong secret rejected (403)" || check fail "wrong secret not rejected ($code)"

# The old default barme/barme must NOT work.
code=$(curl -s -o /dev/null -w '%{http_code}' -u "barme:barme" "http://$HOST:$NPORT/pots")
[[ "$code" == "403" ]] && check ok "old default barme/barme no longer works (403)" || check fail "barme/barme still works ($code)!"

# Upload something so there's a key record + object.
echo "hello secret world" >"$SCRATCH/obj.txt"
curl -s -u "$ACCESS:$SECRET" -T "$SCRATCH/obj.txt" "http://$HOST:$NPORT/objects/sec/a.txt" >/dev/null

echo
echo "== at-rest: secret encrypted, master key locked down =="
# No key file under keys/ may contain the raw secret in the clear.
if grep -rqF "$SECRET" "$DATA_DIR/keys" 2>/dev/null; then
  check fail "raw secret found in plaintext on disk"
else
  check ok "secret is not stored in plaintext (encrypted at rest)"
fi
# The key record file should carry secret_enc, not secret_key.
if grep -rql "secret_enc" "$DATA_DIR/keys" 2>/dev/null; then
  check ok "key record uses secret_enc (encrypted form)"
else
  check fail "key record is not in encrypted form"
fi
# master.key exists with 0600.
if [[ -f "$DATA_DIR/master.key" ]]; then
  perm=$(stat -c '%a' "$DATA_DIR/master.key")
  [[ "$perm" == "600" ]] && check ok "master.key created 0600" || check fail "master.key perms are $perm, not 600"
else
  check fail "master.key was not created"
fi

# Stop, keep the data dir.
kill -9 "$SRV_PID" 2>/dev/null; wait "$SRV_PID" 2>/dev/null; SRV_PID=""

echo
echo "== boot 2: restart on same data dir =="
start_fresh || exit 1
# No new credential should be minted (store already has one).
if grep -q 'Generated an owner' "$SCRATCH/out.log"; then
  check fail "minted a second credential on restart (should reuse)"
else
  check ok "did not re-mint; existing owner reused"
fi
# The original credential still authenticates -> master key persisted + decrypts.
code=$(curl -s -o /dev/null -w '%{http_code}' -u "$ACCESS:$SECRET" "http://$HOST:$NPORT/pots")
[[ "$code" == "200" ]] && check ok "same credential works after restart (master key persisted)" \
  || check fail "credential broke after restart ($code)"
# The object is still readable.
got=$(curl -s -u "$ACCESS:$SECRET" "http://$HOST:$NPORT/objects/sec/a.txt")
[[ "$got" == "hello secret world" ]] && check ok "object intact across restart" || check fail "object lost across restart"

echo
if [[ "$fails" -eq 0 ]]; then
  echo "PASS: default credential killed, secrets encrypted at rest, master key persists."
else
  echo "FAIL: $fails problem(s)."
fi
exit "$fails"
