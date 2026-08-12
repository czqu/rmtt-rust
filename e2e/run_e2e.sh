#!/usr/bin/env bash
#
# Cross-language E2E harness for the rmtt-rust repository.
#
# Starts the Java rmtt server (e2e/java-server, built from the published
# net.czqu.rmtt:rmtt-java-parent:1.0.1 artifacts on Maven Central), waits for
# its READY line, then runs the requested client against it and verifies the
# exit code plus the PASS marker.
#
# Usage: run_e2e.sh <client>
#   client: go | rust
#
# Build artifacts (produced by CI / the remote build step) are expected at:
#   e2e/java-server/target/rmtt-e2e-java-server.jar   (java server, port arg)
#   e2e/bin/go-client                                 (go client, port arg)
#   e2e/rust-client/target/release/rmtt-rust-e2e      (rust client, port arg)
set -u

E2E=$(cd "$(dirname "$0")" && pwd)
JAVA_SERVER_JAR="$E2E/java-server/target/rmtt-e2e-java-server.jar"
GO_CLIENT="$E2E/bin/go-client"
RUST_CLIENT="$E2E/rust-client/target/release/rmtt-rust-e2e"

CLIENT=${1:-}
PORT=${PORT:-19990}
LOG=/tmp/rmtt-e2e-server.log

if [ -z "$CLIENT" ]; then
  echo "usage: run_e2e.sh <go|rust>" >&2
  exit 2
fi

rm -f "$LOG"

fail() {
  echo "E2E FAILED: $1" >&2
  if [ -f "$LOG" ]; then
    echo "---- server log ----" >&2
    tail -50 "$LOG" >&2
  fi
  exit 1
}

wait_ready() { # marker
  local marker=$1
  for _ in $(seq 1 100); do
    if grep -q "$marker" "$LOG" 2>/dev/null; then
      return 0
    fi
    if ! kill -0 "$SRV_PID" 2>/dev/null; then
      fail "server exited before printing $marker"
    fi
    sleep 0.2
  done
  fail "timed out waiting for $marker"
}

echo "==> starting java server on tcp port ${PORT}"
java -jar "$JAVA_SERVER_JAR" "$PORT" >"$LOG" 2>&1 &
SRV_PID=$!
wait_ready E2E_SERVER_READY
echo "==> java server ready (pid $SRV_PID)"

echo "==> running ${CLIENT} client against java server"
case "$CLIENT" in
  go)
    out=$("$GO_CLIENT" "$PORT" 2>&1)
    rc=$?
    marker="GO_E2E_PASS"
    ;;
  rust)
    out=$("$RUST_CLIENT" "$PORT" 2>&1)
    rc=$?
    marker="RUST_CLIENT_E2E_PASS"
    ;;
  *)
    kill "$SRV_PID" 2>/dev/null
    fail "unknown client: $CLIENT (supported: go, rust)"
    ;;
esac

kill "$SRV_PID" 2>/dev/null
wait "$SRV_PID" 2>/dev/null

echo "$out"
if [ "$rc" -ne 0 ]; then
  fail "client exited with $rc"
fi
if ! echo "$out" | grep -q "$marker"; then
  fail "client output missing $marker"
fi

echo "E2E PASS: java server <-> ${CLIENT} client"
