"""Minimal echo MCP server for integration tests.

Reads JSON-RPC from stdin, responds on stdout.
Handles: initialize, tools/list, tools/call (echo), prompts/list,
resources/list, resources/templates/list, ping, notifications/initialized.
"""
import sys
import json
import os
import time

if "--fail-after-first" in sys.argv:
    marker_index = sys.argv.index("--fail-after-first") + 1
    marker_path = sys.argv[marker_index]
    if os.path.exists(marker_path):
        sys.exit(2)
    with open(marker_path, "w", encoding="utf-8") as marker:
        marker.write(str(os.getpid()))

version = "0.1.0"
if "--version-file" in sys.argv:
    version_index = sys.argv.index("--version-file") + 1
    with open(sys.argv[version_index], "r", encoding="utf-8") as version_file:
        version = version_file.read().strip() or version

if "--sleep-init" in sys.argv:
    sleep_index = sys.argv.index("--sleep-init") + 1
    time.sleep(float(sys.argv[sleep_index]))

peer_id_collides = "--peer-id-collides" in sys.argv

sleep_call = 0.0
if "--sleep-call" in sys.argv:
    sleep_call_index = sys.argv.index("--sleep-call") + 1
    sleep_call = float(sys.argv[sleep_call_index])

exit_after_call_marker = None
if "--exit-after-call-once" in sys.argv:
    exit_index = sys.argv.index("--exit-after-call-once") + 1
    exit_after_call_marker = sys.argv[exit_index]


def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except Exception:
        continue

    method = msg.get("method", "")
    mid = msg.get("id")

    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": "2025-03-26",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "echo-server", "version": version}
        }})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "tools": [{
                "name": "echo",
                "description": "Echo input back",
                "inputSchema": {
                    "type": "object",
                    "properties": {"msg": {"type": "string"}}
                }
            }]
        }})
    elif method == "tools/call":
        params = msg.get("params", {})
        if params.get("name") == "ask-client":
            peer_id = mid if peer_id_collides else "server-ask-1"
            send({"jsonrpc": "2.0", "id": peer_id, "method": "sampling/createMessage", "params": {
                "messages": [{"role": "user", "content": {"type": "text", "text": "ping-client"}}],
                "maxTokens": 8
            }})
            for response_line in sys.stdin:
                response_line = response_line.strip()
                if not response_line:
                    continue
                response = json.loads(response_line)
                if response.get("id") == peer_id:
                    send({"jsonrpc": "2.0", "id": mid, "result": {
                        "content": [{"type": "text", "text": response.get("result", {}).get("content", {}).get("text", "")}]
                    }})
                    break
            continue
        arg_msg = params.get("arguments", {}).get("msg", "")
        if sleep_call > 0:
            time.sleep(sleep_call)
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "content": [{"type": "text", "text": arg_msg}]
        }})
        if exit_after_call_marker and not os.path.exists(exit_after_call_marker):
            with open(exit_after_call_marker, "w", encoding="utf-8") as marker:
                marker.write(str(os.getpid()))
            sys.exit(0)
    elif method == "prompts/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"prompts": []}})
    elif method == "resources/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"resources": []}})
    elif method == "resources/templates/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"resourceTemplates": []}})
    elif method == "ping":
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
    elif method == "notifications/initialized":
        pass  # notification, no response
    elif mid is not None:
        # Unknown method with id: return empty result
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
