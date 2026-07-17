#!/usr/bin/env python3
"""Minimal MCP stdio server for testing pirs-mcp: echo + add + fail tools."""
import json, sys

TOOLS = [
    {"name": "echo", "description": "Echo text back", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
    {"name": "add", "description": "Add two numbers", "inputSchema": {"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}, "required": ["a", "b"]}},
    {"name": "fail", "description": "Always returns an error result", "inputSchema": {"type": "object", "properties": {}}},
]

def handle(req):
    mid = req.get("id")
    method = req.get("method")
    if method == "initialize":
        return {"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": "2025-03-26", "capabilities": {"tools": {}}, "serverInfo": {"name": "echo-mcp", "version": "0.1"}}}
    if method == "notifications/initialized":
        return None
    if method == "tools/list":
        return {"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS}}
    if method == "tools/call":
        name = req["params"]["name"]
        args = req["params"].get("arguments", {})
        if name == "echo":
            return {"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": f"echo: {args.get('text','')}"}], "isError": False}}
        if name == "add":
            return {"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": str(args.get('a',0)+args.get('b',0))}], "isError": False}}
        if name == "fail":
            return {"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "intentional failure"}], "isError": True}}
        return {"jsonrpc": "2.0", "id": mid, "error": {"code": -32602, "message": f"unknown tool {name}"}}
    if method == "shutdown":
        return {"jsonrpc": "2.0", "id": mid, "result": {}}
    return {"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": f"unknown method {method}"}}

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    resp = handle(json.loads(line))
    if resp is not None:
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
