#!/usr/bin/env python3
"""A minimal stdio MCP server with one echo tool, for verification."""
import json
import sys


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        message = json.loads(line)
    except ValueError:
        continue
    method = message.get("method")
    request_id = message.get("id")
    params = message.get("params") or {}
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": request_id, "result": {
            "protocolVersion": params.get("protocolVersion", "2025-06-18"),
            "capabilities": {"tools": {"listChanged": False}},
            "serverInfo": {"name": "verify-echo", "version": "1.0.0"},
        }})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": request_id, "result": {"tools": [{
            "name": "echo",
            "description": "Echo the text back.",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]},
        }]}})
    elif method == "tools/call":
        text = (params.get("arguments") or {}).get("text", "")
        send({"jsonrpc": "2.0", "id": request_id, "result": {
            "content": [{"type": "text", "text": "ECHO: " + str(text)}],
            "isError": False,
        }})
    elif method == "ping":
        send({"jsonrpc": "2.0", "id": request_id, "result": {}})
    elif request_id is not None:
        send({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32601, "message": "no method " + str(method)}})
