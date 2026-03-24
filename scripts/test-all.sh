#!/usr/bin/env bash
# mcp-wrapper-rs full test suite
# Usage: ./scripts/test-all.sh [--quick]
#
# Layers:
#   1. cargo test     — unit + conformance + integration (fast, always runs)
#   2. manual pipe    — stdin/stdout JSON-RPC smoke tests
#   3. mcp-validator  — Janix STDIO compliance (requires Python + clone)
#
# --quick: skip layer 3 (external validator)

set -euo pipefail
cd "$(dirname "$0")/.."

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'
PASS=0
FAIL=0
SKIP=0

pass() { echo -e "  ${GREEN}PASS${NC} $1"; PASS=$((PASS + 1)); }
fail() { echo -e "  ${RED}FAIL${NC} $1: $2"; FAIL=$((FAIL + 1)); }
skip() { echo -e "  ${YELLOW}SKIP${NC} $1"; SKIP=$((SKIP + 1)); }

QUICK=false
[[ "${1:-}" == "--quick" ]] && QUICK=true

WRAPPER="./target/release/mcp-wrapper-rs"
ECHO_SERVER="python3 tests/fixtures/echo_server.py"

# ── Layer 1: cargo test ──────────────────────────────────────────────

echo "=== Layer 1: cargo test ==="
cargo build --release 2>&1 | grep -v '^warning' || true
if cargo test 2>&1 | tee /tmp/mcp-wrapper-test.log | tail -5; then
    pass "cargo test"
else
    fail "cargo test" "see /tmp/mcp-wrapper-test.log"
fi

# ── Layer 2: manual pipe smoke tests ─────────────────────────────────

echo ""
echo "=== Layer 2: pipe smoke tests ==="

# Helper: send JSON-RPC lines, capture output
pipe_test() {
    local name="$1"
    local input="$2"
    local expect_pattern="$3"

    local output
    output=$(echo "$input" | timeout 15 $WRAPPER $ECHO_SERVER 2>/dev/null) || true

    if echo "$output" | grep -q "$expect_pattern"; then
        pass "$name"
    else
        fail "$name" "expected pattern '$expect_pattern' not found"
        echo "    output: $(echo "$output" | head -3)"
    fi
}

# Test: initialize returns server info
pipe_test "initialize" \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}' \
    '"protocolVersion"'

# Test: tools/list returns cached tools
pipe_test "tools/list" \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}
{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
    '"echo"'

# Test: tools/call pass-through
pipe_test "tools/call echo" \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"echo","arguments":{"msg":"smoke-test"}}}' \
    'smoke-test'

# Test: ping local handler
pipe_test "ping" \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}
{"jsonrpc":"2.0","id":99,"method":"ping"}' \
    '"id":99'

# Test: prompts/list from cache
pipe_test "prompts/list" \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}
{"jsonrpc":"2.0","id":3,"method":"prompts/list"}' \
    '"prompts"'

# Test: resources/list from cache
pipe_test "resources/list" \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}
{"jsonrpc":"2.0","id":4,"method":"resources/list"}' \
    '"resources"'

# Test: unknown method pass-through
pipe_test "unknown method" \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}
{"jsonrpc":"2.0","id":42,"method":"some/future/method","params":{}}' \
    '"id":42'

# Test: multiple consecutive calls (ID correctness)
pipe_test "consecutive calls IDs" \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}
{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"echo","arguments":{"msg":"first"}}}
{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"echo","arguments":{"msg":"second"}}}' \
    '"id":20'

# Test: --version flag
version_output=$($WRAPPER --version 2>&1)
if echo "$version_output" | grep -q "0.3.0"; then
    pass "--version"
else
    fail "--version" "got: $version_output"
fi

# Test: --help flag
if $WRAPPER --help 2>&1 | grep -q "Usage:"; then
    pass "--help"
else
    fail "--help" "no Usage: in output"
fi

# Test: no args exits with error
noargs_output=$($WRAPPER 2>&1 || true)
if echo "$noargs_output" | grep -q "Error:"; then
    pass "no-args error"
else
    fail "no-args error" "no Error: in output"
fi

# Test: unknown flag exits with error
bogus_output=$($WRAPPER --bogus 2>&1 || true)
if echo "$bogus_output" | grep -q "Error:"; then
    pass "unknown flag error"
else
    fail "unknown flag error" "no Error: in output"
fi

# Test: no orphan processes
pipe_test "no orphan (basic)" \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"echo","arguments":{"msg":"orphan-check"}}}' \
    'orphan-check'
sleep 1
orphans=$(pgrep -f "echo_server.py" 2>/dev/null | wc -l || true)
if [ "$orphans" -eq 0 ]; then
    pass "no orphan processes"
else
    fail "no orphan processes" "$orphans echo_server.py still running"
    pkill -f "echo_server.py" 2>/dev/null || true
fi

# ── Layer 3: Janix mcp-validator (STDIO compliance) ──────────────────

echo ""
echo "=== Layer 3: mcp-validator STDIO compliance ==="

if $QUICK; then
    skip "mcp-validator (--quick mode)"
else
    VALIDATOR_DIR="/tmp/mcp-validator"
    if [ ! -d "$VALIDATOR_DIR" ]; then
        echo "  Cloning mcp-validator..."
        git clone --depth 1 https://github.com/Janix-ai/mcp-validator.git "$VALIDATOR_DIR" 2>/dev/null
    fi

    if [ -f "$VALIDATOR_DIR/mcp_testing/scripts/compliance_report.py" ]; then
        # Setup venv and install deps if needed
        VENV_DIR="/tmp/mcp-validator-venv"
        if [ ! -f "$VENV_DIR/bin/python3" ]; then
            python3 -m venv "$VENV_DIR"
        fi
        if ! "$VENV_DIR/bin/python3" -c "import mcp_testing" 2>/dev/null; then
            echo "  Installing validator dependencies..."
            "$VENV_DIR/bin/pip" install -r "$VALIDATOR_DIR/requirements.txt" -q 2>/dev/null || true
        fi

        echo "  Running STDIO compliance tests..."
        ABS_WRAPPER="$(realpath "$WRAPPER")"
        ABS_ECHO="$(realpath tests/fixtures/echo_server.py)"
        WRAPPER_CMD="$ABS_WRAPPER python3 $ABS_ECHO"
        output=$(cd "$VALIDATOR_DIR" && PYTHONPATH="$VALIDATOR_DIR" \
            MCP_SKIP_SHUTDOWN=true \
            timeout 300 "$VENV_DIR/bin/python3" \
            -m mcp_testing.scripts.compliance_report \
            --server-command "$WRAPPER_CMD" \
            --protocol-version 2025-03-26 \
            --skip-shutdown \
            --test-mode core 2>&1) || true

        # Show summary lines (filter out verbose env dumps)
        echo "$output" | grep -E '^\[|Passed|Failed|PASS|FAIL|passed|failed|Total|Summary' | tail -30

        passed_count=$(echo "$output" | grep -c '✅' || true)
        failed_count=$(echo "$output" | grep -c '❌' || true)
        echo "  Validator: $passed_count passed, $failed_count failed"

        if [ "$failed_count" -eq 0 ] && [ "$passed_count" -gt 0 ]; then
            pass "mcp-validator STDIO compliance ($passed_count tests)"
        elif [ "$passed_count" -gt 0 ]; then
            fail "mcp-validator STDIO compliance" "$failed_count of $((passed_count + failed_count)) tests failed"
        else
            skip "mcp-validator (no results)"
        fi
    else
        skip "mcp-validator (clone failed)"
    fi
fi

# ── Summary ──────────────────────────────────────────────────────────

echo ""
echo "========================================"
echo -e "  ${GREEN}PASS: $PASS${NC}  ${RED}FAIL: $FAIL${NC}  ${YELLOW}SKIP: $SKIP${NC}"
echo "========================================"

if [ $FAIL -gt 0 ]; then
    exit 1
fi
