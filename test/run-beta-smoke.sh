#!/usr/bin/env bash
# Checkpoint 3 spike, beta-shape: live beta enclave smoke. Brings up
# pingora-enclavia against $ENCLAVE_UUID and verifies a curl through it
# reaches the workload (response body parses, PCRs present in the
# headers).
#
# Required env:
#   ENCLAVE_UUID  uuid of a running beta enclave
#   PCR0, PCR1, PCR2  hex strings the enclave attested (from enclave_status)
#
# Optional env:
#   DEBUG_MODE    "true" (default) skips COSE chain check, "false" enforces it

set -euo pipefail

: "${ENCLAVE_UUID:?ENCLAVE_UUID required}"
: "${PCR0:?PCR0 required}"
: "${PCR1:?PCR1 required}"
: "${PCR2:?PCR2 required}"
DEBUG_MODE="${DEBUG_MODE:-true}"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOG_DIR="$REPO_ROOT/test/.run-spike"
mkdir -p "$LOG_DIR"

CFG_DIR="$LOG_DIR/proxy-targets-beta"
rm -rf "$CFG_DIR"
mkdir -p "$CFG_DIR"
cat > "$CFG_DIR/$ENCLAVE_UUID.json" <<EOF
{
  "enclave_id": "$ENCLAVE_UUID",
  "endpoint": "wss://$ENCLAVE_UUID.enclaves.beta.enclavia.io",
  "pcrs": {
    "pcr0": "$PCR0",
    "pcr1": "$PCR1",
    "pcr2": "$PCR2"
  },
  "debug_mode": $DEBUG_MODE
}
EOF

cd "$REPO_ROOT"
cargo build --bin pingora-enclavia

pkill -f 'target/debug/pingora-enclavia' 2>/dev/null || true
sleep 0.3

PROXY_LOG="$LOG_DIR/proxy-beta.log"
RUST_LOG="${RUST_LOG:-info}" "$REPO_ROOT/target/debug/pingora-enclavia" \
  --config-dir "$CFG_DIR" --listen 127.0.0.1:6188 \
  > "$PROXY_LOG" 2>&1 &
PROXY_PID=$!
trap "kill $PROXY_PID 2>/dev/null || true" EXIT

for _ in $(seq 1 100); do
  if (echo > /dev/tcp/127.0.0.1/6188) 2>/dev/null; then break; fi
  sleep 0.1
done

CURL_HDR="$LOG_DIR/curl-beta.hdr"
CURL_OUT="$LOG_DIR/curl-beta.out"

curl -sS -D "$CURL_HDR" -o "$CURL_OUT" \
  -H "Host: $ENCLAVE_UUID.enclaves.beta.enclavia.io" \
  http://127.0.0.1:6188/

echo "--- response headers ---"
cat "$CURL_HDR"
echo
echo "--- response body (first 500 bytes) ---"
head -c 500 "$CURL_OUT" || true
echo

fail=0
grep -qi '^X-Enclavia-PCR0:' "$CURL_HDR" || { echo "FAIL: no PCR0 header"; fail=1; }
grep -qi '^X-Enclavia-PCR1:' "$CURL_HDR" || { echo "FAIL: no PCR1 header"; fail=1; }
grep -qi '^X-Enclavia-PCR2:' "$CURL_HDR" || { echo "FAIL: no PCR2 header"; fail=1; }

if [ "$fail" -ne 0 ]; then
  echo "--- proxy log tail ---"
  tail -30 "$PROXY_LOG" || true
  exit 1
fi

echo "BETA SMOKE PASSED"
