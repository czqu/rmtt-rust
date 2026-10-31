#!/usr/bin/env bash
#
# Cross-language E2E harness for the rmtt-rust repository.
#
# Starts the requested rmtt server, waits for its READY line, then runs the
# requested client against it and verifies the exit code plus the client's
# PASS marker.
#
# Usage: run_e2e.sh [server] <client>
#   server: java | rust        (default: java)
#     java : e2e/java-server, a real rmtt-java 1.0.1 server (Maven Central)
#     rust : e2e/rust-server, this repository's `server` feature
#   client: go | rust
#     go   : e2e/bin/go-client, an rmtt-go v1.0.2 client
#     rust : e2e/rust-client, this repository's crate
#
# Build artifacts (produced by CI / the remote build step) are expected at:
#   e2e/java-server/target/rmtt-e2e-java-server.jar        (java server)
#   e2e/rust-server/target/release/rmtt-rust-e2e-server    (rust server)
#   e2e/bin/go-client                                      (go client)
#   e2e/rust-client/target/release/rmtt-rust-e2e           (rust client)
set -u

E2E=$(cd "$(dirname "$0")" && pwd)
JAVA_SERVER_JAR="$E2E/java-server/target/rmtt-e2e-java-server.jar"
RUST_SERVER="$E2E/rust-server/target/release/rmtt-rust-e2e-server"
GO_CLIENT="$E2E/bin/go-client"
RUST_CLIENT="$E2E/rust-client/target/release/rmtt-rust-e2e"

if [ "$#" -eq 1 ]; then
  SERVER=java
  CLIENT=$1
elif [ "$#" -eq 2 ]; then
  SERVER=$1
  CLIENT=$2
else
  echo "usage: run_e2e.sh [java|rust] <go|rust>" >&2
  exit 2
fi

PORT=${PORT:-19990}
LOG=/tmp/rmtt-e2e-server.log

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

case "$SERVER" in
  java)
    [ -f "$JAVA_SERVER_JAR" ] || fail "missing $JAVA_SERVER_JAR (build e2e/java-server first)"
    echo "==> starting java server on tcp port ${PORT}"
    java -jar "$JAVA_SERVER_JAR" "$PORT" >"$LOG" 2>&1 &
    ;;
  rust)
    [ -x "$RUST_SERVER" ] || fail "missing $RUST_SERVER (build e2e/rust-server first)"
    echo "==> starting rust server on tcp port ${PORT}"
    "$RUST_SERVER" "$PORT" >"$LOG" 2>&1 &
    ;;
  *)
    echo "usage: run_e2e.sh [java|rust] <go|rust>" >&2
    exit 2
    ;;
esac
SRV_PID=$!
wait_ready E2E_SERVER_READY
echo "==> $SERVER server ready (pid $SRV_PID)"

echo "==> running ${CLIENT} client against ${SERVER} server"
case "$CLIENT" in
  go)
    [ -x "$GO_CLIENT" ] || fail "missing $GO_CLIENT (build e2e/go-client first)"
    out=$("$GO_CLIENT" "$PORT" 2>&1)
    rc=$?
    marker="GO_E2E_PASS"
    ;;
  rust)
    [ -x "$RUST_CLIENT" ] || fail "missing $RUST_CLIENT (build e2e/rust-client first)"
    out=$("$RUST_CLIENT" "$PORT" 2>&1)
    rc=$?
    marker="RUST_CLIENT_E2E_PASS"
    ;;
  *)
    kill "$SRV_PID" 2>/dev/null
    echo "usage: run_e2e.sh [java|rust] <go|rust>" >&2
    exit 2
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

echo "E2E PASS: $SERVER server <-> $CLIENT client"
