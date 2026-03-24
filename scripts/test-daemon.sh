#!/usr/bin/env bash
# Live test for daemon mode
set -euo pipefail

WRAPPER="$(dirname "$0")/../target/release/mcp-wrapper-rs"
ECHO="$(dirname "$0")/../tests/fixtures/echo_server.py"
PASS=0
FAIL=0

pass() { echo "  PASS: $1"; ((PASS++)); }
fail() { echo "  FAIL: $1"; ((FAIL++)); }

cleanup_broker() {
    # Kill any lingering broker for this echo_server
    local sock
    sock=$(ls "$XDG_RUNTIME_DIR/mcp-wrapper/"*.sock 2>/dev/null || true)
    if [ -n "$sock" ]; then
        fuser -k "$sock" 2>/dev/null || true
        rm -f "$sock"
    fi
}

INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"0.1"}}}'
TOOLS_LIST='{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
TOOL_CALL='{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo","arguments":{"msg":"live-test"}}}'
PING='{"jsonrpc":"2.0","id":4,"method":"ping"}'
NOTIF_INIT='{"jsonrpc":"2.0","method":"notifications/initialized"}'

# ── Test 1: Non-daemon mode (regression) ──────────────────────────
echo "=== Test 1: Non-daemon mode ==="
RESP=$(printf '%s\n%s\n' "$INIT" "$TOOLS_LIST" | timeout 10 "$WRAPPER" python3 "$ECHO" 2>/dev/null)
if echo "$RESP" | grep -q '"echo-server"'; then
    pass "initialize returns echo-server"
else
    fail "initialize did not return echo-server"
fi
if echo "$RESP" | grep -q '"echo"'; then
    pass "tools/list returns echo tool"
else
    fail "tools/list missing echo tool"
fi

# ── Test 2: Daemon mode - single client ───────────────────────────
echo "=== Test 2: Daemon mode - single client ==="
cleanup_broker
sleep 0.2

RESP=$(printf '%s\n%s\n%s\n%s\n%s\n' "$INIT" "$NOTIF_INIT" "$TOOLS_LIST" "$TOOL_CALL" "$PING" | timeout 15 "$WRAPPER" --daemon python3 "$ECHO" 2>/dev/null)
if echo "$RESP" | grep -q '"echo-server"'; then
    pass "daemon: initialize returns echo-server"
else
    fail "daemon: initialize did not return echo-server"
fi
if echo "$RESP" | grep -q '"live-test"'; then
    pass "daemon: tools/call returns live-test"
else
    fail "daemon: tools/call did not return live-test"
fi

# ── Test 3: Daemon mode - second client reuses broker ─────────────
echo "=== Test 3: Daemon mode - second client reuses broker ==="
# Broker should still be alive from test 2 (60s idle timeout)
# Check socket exists
SOCKS=$(ls "$XDG_RUNTIME_DIR/mcp-wrapper/"*.sock 2>/dev/null | wc -l)
if [ "$SOCKS" -ge 1 ]; then
    pass "broker socket exists after first client"
else
    fail "broker socket missing"
fi

RESP2=$(printf '%s\n%s\n%s\n' "$INIT" "$NOTIF_INIT" "$TOOLS_LIST" | timeout 10 "$WRAPPER" --daemon python3 "$ECHO" 2>/dev/null)
if echo "$RESP2" | grep -q '"echo-server"'; then
    pass "second client: initialize OK"
else
    fail "second client: initialize failed"
fi
if echo "$RESP2" | grep -q '"echo"'; then
    pass "second client: tools/list OK"
else
    fail "second client: tools/list failed"
fi

# ── Test 4: --version and --help still work ───────────────────────
echo "=== Test 4: CLI flags ==="
if "$WRAPPER" --version 2>&1 | grep -q "mcp-wrapper-rs"; then
    pass "--version works"
else
    fail "--version broken"
fi
if "$WRAPPER" --help 2>&1 | grep -q "daemon"; then
    pass "--help mentions --daemon"
else
    fail "--help missing --daemon"
fi

# ── Cleanup ───────────────────────────────────────────────────────
cleanup_broker

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[ "$FAIL" -eq 0 ]
