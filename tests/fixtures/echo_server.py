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
paginate_tools = "--paginate-tools" in sys.argv
respond_after_cancel = "--respond-after-cancel" in sys.argv
wrong_progress_token = "--wrong-progress-token" in sys.argv
refresh_error_prompts = "--refresh-error-prompts" in sys.argv
tools_list_count = 0
prompts_list_count = 0

cancel_marker = None
if "--cancel-marker" in sys.argv:
    cancel_marker_index = sys.argv.index("--cancel-marker") + 1
    cancel_marker = sys.argv[cancel_marker_index]

request_marker = None
if "--request-marker" in sys.argv:
    request_marker_index = sys.argv.index("--request-marker") + 1
    request_marker = sys.argv[request_marker_index]

peer_progress_marker = None
if "--peer-progress-marker" in sys.argv:
    peer_progress_marker_index = sys.argv.index("--peer-progress-marker") + 1
    peer_progress_marker = sys.argv[peer_progress_marker_index]

sleep_call = 0.0
if "--sleep-call" in sys.argv:
    sleep_call_index = sys.argv.index("--sleep-call") + 1
    sleep_call = float(sys.argv[sleep_call_index])

exit_after_call_marker = None
if "--exit-after-call-once" in sys.argv:
    exit_index = sys.argv.index("--exit-after-call-once") + 1
    exit_after_call_marker = sys.argv[exit_index]

duplicate_initialized_marker = None
if "--duplicate-initialized-marker" in sys.argv:
    initialized_marker_index = sys.argv.index("--duplicate-initialized-marker") + 1
    duplicate_initialized_marker = sys.argv[initialized_marker_index]

initialized_count = 0

init_protocol_marker = None
if "--init-protocol-marker" in sys.argv:
    init_protocol_marker_index = sys.argv.index("--init-protocol-marker") + 1
    init_protocol_marker = sys.argv[init_protocol_marker_index]


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
        if init_protocol_marker:
            with open(init_protocol_marker, "w", encoding="utf-8") as marker:
                marker.write(json.dumps(msg.get("params", {}).get("protocolVersion")))
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": "2025-03-26",
            "capabilities": {"tools": {}, **({"prompts": {}} if refresh_error_prompts else {})},
            "serverInfo": {"name": "echo-server", "version": version}
        }})
    elif method == "tools/list":
        tools_list_count += 1
        cursor = msg.get("params", {}).get("cursor")
        if paginate_tools and cursor is None:
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "tools": [{
                    "name": "echo-v2" if refresh_error_prompts and tools_list_count > 1 else "echo",
                    "description": "Echo input back",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"msg": {"type": "string"}}
                    }
                }],
                "nextCursor": "page-2"
            }})
        elif paginate_tools and cursor == "page-2":
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "tools": [{
                    "name": "paged",
                    "description": "Second page tool",
                    "inputSchema": {"type": "object"}
                }]
            }})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "tools": [{
                    "name": "echo-v2" if refresh_error_prompts and tools_list_count > 1 else "echo",
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
            peer_params = {
                "messages": [{"role": "user", "content": {"type": "text", "text": "ping-client"}}],
                "maxTokens": 8
            }
            if peer_progress_marker:
                peer_params["_meta"] = {"progressToken": "peer-progress-token-1"}
            send({"jsonrpc": "2.0", "id": peer_id, "method": "sampling/createMessage", "params": peer_params})
            for response_line in sys.stdin:
                response_line = response_line.strip()
                if not response_line:
                    continue
                response = json.loads(response_line)
                if response.get("method") == "notifications/progress":
                    if peer_progress_marker:
                        with open(peer_progress_marker, "w", encoding="utf-8") as marker:
                            marker.write(json.dumps(response.get("params", {}).get("progressToken")))
                    continue
                if response.get("id") == peer_id:
                    send({"jsonrpc": "2.0", "id": mid, "result": {
                        "content": [{"type": "text", "text": response.get("result", {}).get("content", {}).get("text", "")}]
                    }})
                    break
            continue
        if params.get("name") == "wait-cancel":
            if request_marker:
                with open(request_marker, "w", encoding="utf-8") as marker:
                    marker.write(json.dumps(mid))
            for cancel_line in sys.stdin:
                cancel_line = cancel_line.strip()
                if not cancel_line:
                    continue
                cancel = json.loads(cancel_line)
                if cancel.get("method") == "notifications/cancelled":
                    cancel_id = cancel.get("params", {}).get("requestId")
                    if cancel_marker:
                        with open(cancel_marker, "w", encoding="utf-8") as marker:
                            marker.write(json.dumps(cancel_id))
                    if respond_after_cancel:
                        send({"jsonrpc": "2.0", "id": mid, "result": {
                            "content": [{"type": "text", "text": "late-cancel-response"}]
                        }})
                    break
            continue
        if params.get("name") == "progress":
            requested_token = params.get("_meta", {}).get("progressToken")
            progress_token = "wrong-token" if wrong_progress_token else requested_token
            send({"jsonrpc": "2.0", "method": "notifications/progress", "params": {
                "progressToken": progress_token,
                "progress": 1,
                "total": 2
            }})
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
        prompts_list_count += 1
        if refresh_error_prompts and prompts_list_count > 1:
            send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32001, "message": "refresh failed"}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {"prompts": [{"name": "p1", "description": "stable"}] if refresh_error_prompts else []}})
    elif method == "resources/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"resources": []}})
    elif method == "resources/templates/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"resourceTemplates": []}})
    elif method == "ping":
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
    elif method == "notifications/initialized":
        initialized_count += 1
        if duplicate_initialized_marker and initialized_count > 1:
            with open(duplicate_initialized_marker, "w", encoding="utf-8") as marker:
                marker.write(str(os.getpid()))
        pass  # notification, no response
    elif mid is not None:
        # Unknown method with id: return empty result
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
