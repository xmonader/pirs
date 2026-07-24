#!/usr/bin/env python3
"""Mock MCP stdio server: email + calendar list/read tools (no live credentials).

Used for product proof that pirs loads connector-style MCP tools into the agent
tool set. Configure via .mcp.json or ~/.pirs/mcp.json — see docs/mcp-email-calendar.md.
"""
import json
import sys

TOOLS = [
    {
        "name": "email_list",
        "description": "List recent inbox messages (mock)",
        "inputSchema": {
            "type": "object",
            "properties": {
                "limit": {"type": "integer", "description": "Max messages (default 10)"}
            },
        },
    },
    {
        "name": "email_read",
        "description": "Read one message by id (mock)",
        "inputSchema": {
            "type": "object",
            "properties": {"id": {"type": "string"}},
            "required": ["id"],
        },
    },
    {
        "name": "calendar_list",
        "description": "List upcoming calendar events (mock)",
        "inputSchema": {
            "type": "object",
            "properties": {
                "days": {"type": "integer", "description": "Lookahead days (default 7)"}
            },
        },
    },
    {
        "name": "calendar_get",
        "description": "Get one calendar event by id (mock)",
        "inputSchema": {
            "type": "object",
            "properties": {"id": {"type": "string"}},
            "required": ["id"],
        },
    },
]

INBOX = {
    "m1": {
        "id": "m1",
        "from": "alice@example.com",
        "subject": "Q3 plan",
        "body": "Please review the attached Q3 plan before Friday.",
    },
    "m2": {
        "id": "m2",
        "from": "bob@example.com",
        "subject": "Lunch?",
        "body": "Are you free Thursday noon?",
    },
}

EVENTS = {
    "e1": {
        "id": "e1",
        "title": "Standup",
        "when": "2026-07-25T09:00:00Z",
        "where": "Meet",
    },
    "e2": {
        "id": "e2",
        "title": "Design review",
        "when": "2026-07-25T15:00:00Z",
        "where": "Room B",
    },
}


def handle(req):
    mid = req.get("id")
    method = req.get("method")
    if method == "initialize":
        return {
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "protocolVersion": "2025-03-26",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "email-calendar-mcp", "version": "0.1"},
            },
        }
    if method == "notifications/initialized":
        return None
    if method == "tools/list":
        return {"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS}}
    if method == "tools/call":
        name = req["params"]["name"]
        args = req["params"].get("arguments") or {}
        if name == "email_list":
            limit = int(args.get("limit") or 10)
            items = list(INBOX.values())[:limit]
            text = json.dumps({"messages": items}, indent=2)
            return {
                "jsonrpc": "2.0",
                "id": mid,
                "result": {"content": [{"type": "text", "text": text}], "isError": False},
            }
        if name == "email_read":
            msg = INBOX.get(args.get("id", ""))
            if not msg:
                return {
                    "jsonrpc": "2.0",
                    "id": mid,
                    "result": {
                        "content": [{"type": "text", "text": "message not found"}],
                        "isError": True,
                    },
                }
            return {
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "content": [{"type": "text", "text": json.dumps(msg, indent=2)}],
                    "isError": False,
                },
            }
        if name == "calendar_list":
            days = int(args.get("days") or 7)
            items = list(EVENTS.values())
            text = json.dumps({"days": days, "events": items}, indent=2)
            return {
                "jsonrpc": "2.0",
                "id": mid,
                "result": {"content": [{"type": "text", "text": text}], "isError": False},
            }
        if name == "calendar_get":
            ev = EVENTS.get(args.get("id", ""))
            if not ev:
                return {
                    "jsonrpc": "2.0",
                    "id": mid,
                    "result": {
                        "content": [{"type": "text", "text": "event not found"}],
                        "isError": True,
                    },
                }
            return {
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "content": [{"type": "text", "text": json.dumps(ev, indent=2)}],
                    "isError": False,
                },
            }
        return {
            "jsonrpc": "2.0",
            "id": mid,
            "error": {"code": -32602, "message": f"unknown tool {name}"},
        }
    if method == "shutdown":
        return {"jsonrpc": "2.0", "id": mid, "result": {}}
    return {
        "jsonrpc": "2.0",
        "id": mid,
        "error": {"code": -32601, "message": f"unknown method {method}"},
    }


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    resp = handle(json.loads(line))
    if resp is not None:
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
