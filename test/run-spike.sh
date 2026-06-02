#!/usr/bin/env bash
# Checkpoint 2 spike, beta-shape: brings up the noise-echo responder +
# pingora-enclavia proxy, fires a curl through, validates the HTTP echo
# body and the X-Enclavia-PCR* headers come back.
#
# Differences from the original checkpoint 2: the proxy now reads its
# upstream + PCRs from a per-enclave JSON file in a watched directory,
# and dispatches based on the inbound Host header's leftmost label.
# The curl sends `Host: <fake-uuid>.enclaves.local` to match.
#
# Usage: nix develop -c bash test/run-spike.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

CARGO_BIN="${CARGO_BIN:-cargo}"
"$CARGO_BIN" build --bins

LOG_DIR="$REPO_ROOT/test/.run-spike"
mkdir -p "$LOG_DIR"
RESPONDER_LOG="$LOG_DIR/responder.log"
PROXY_LOG="$LOG_DIR/proxy.log"

CFG_DIR="$LOG_DIR/proxy-targets"
rm -rf "$CFG_DIR"
mkdir -p "$CFG_DIR"

# The noise-echo responder builds PCRs from a fixed seed (0x42) via
# FakeAttestation, which yields pcr0=0x42×48, pcr1=0x43×48, pcr2=0x44×48.
FAKE_UUID="00000000-0000-0000-0000-000000000001"
PCR0_HEX="$(printf '42%.0s' $(seq 1 48))"
PCR1_HEX="$(printf '43%.0s' $(seq 1 48))"
PCR2_HEX="$(printf '44%.0s' $(seq 1 48))"
cat > "$CFG_DIR/$FAKE_UUID.json" <<EOF
{
  "enclave_id": "$FAKE_UUID",
  "endpoint": "ws://127.0.0.1:9101/",
  "pcrs": {
    "pcr0": "$PCR0_HEX",
    "pcr1": "$PCR1_HEX",
    "pcr2": "$PCR2_HEX"
  },
  "debug_mode": true
}
EOF

pkill -f 'target/debug/noise-echo' 2>/dev/null || true
pkill -f 'target/debug/pingora-enclavia' 2>/dev/null || true
sleep 0.2

LISTEN=127.0.0.1:9101 MODE=http "$REPO_ROOT/target/debug/noise-echo" \
  > "$RESPONDER_LOG" 2>&1 &
RESPONDER_PID=$!

cleanup() {
  kill "$RESPONDER_PID" 2>/dev/null || true
  if [ -n "${PROXY_PID:-}" ]; then
    kill "$PROXY_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

for _ in $(seq 1 50); do
  if (echo > /dev/tcp/127.0.0.1/9101) 2>/dev/null; then
    break
  fi
  sleep 0.1
done

RUST_LOG="${RUST_LOG:-info}" "$REPO_ROOT/target/debug/pingora-enclavia" \
  --config-dir "$CFG_DIR" --listen 127.0.0.1:6188 \
  > "$PROXY_LOG" 2>&1 &
PROXY_PID=$!

for _ in $(seq 1 100); do
  if (echo > /dev/tcp/127.0.0.1/6188) 2>/dev/null; then
    break
  fi
  sleep 0.1
done

CURL_OUT="$LOG_DIR/curl.out"
CURL_HDR="$LOG_DIR/curl.hdr"

curl -sS -D "$CURL_HDR" -o "$CURL_OUT" \
  -H "Host: $FAKE_UUID.enclaves.local" \
  -X POST -d 'hello from the spike' \
  http://127.0.0.1:6188/foo

echo
echo "--- response headers ---"
cat "$CURL_HDR"
echo
echo "--- response body ---"
cat "$CURL_OUT"
echo

fail=0
grep -qi '^HTTP/1.1 200' "$CURL_HDR" || { echo "FAIL: not 200"; fail=1; }
grep -qi '^X-Enclavia-PCR0:' "$CURL_HDR" || { echo "FAIL: no PCR0 header"; fail=1; }
grep -qi '^X-Enclavia-PCR1:' "$CURL_HDR" || { echo "FAIL: no PCR1 header"; fail=1; }
grep -qi '^X-Enclavia-PCR2:' "$CURL_HDR" || { echo "FAIL: no PCR2 header"; fail=1; }
grep -qi '^Hello-Method: POST' "$CURL_HDR" || { echo "FAIL: missing Hello-Method"; fail=1; }
grep -q 'hello from the spike' "$CURL_OUT" || { echo "FAIL: body not echoed"; fail=1; }

if [ "$fail" -ne 0 ]; then
  echo
  echo "--- proxy log tail ---"
  tail -50 "$PROXY_LOG" || true
  echo
  echo "--- responder log tail ---"
  tail -50 "$RESPONDER_LOG" || true
  exit 1
fi

echo "SPIKE CHECKPOINT 2 PASSED"
