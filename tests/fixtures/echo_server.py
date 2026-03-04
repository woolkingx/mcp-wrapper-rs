"""Minimal echo MCP server for integration tests.

Reads JSON-RPC from stdin, responds on stdout.
Handles: initialize, tools/list, tools/call (echo), prompts/list,
resources/list, resources/templates/list, ping, notifications/initialized.
"""
import sys
import json
import os


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
            "serverInfo": {"name": "echo-server", "version": "0.1.0"}
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
        arg_msg = params.get("arguments", {}).get("msg", "")
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "content": [{"type": "text", "text": arg_msg}]
        }})
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
