#!/usr/bin/env python3
"""Mock MCP HTTP server: streamable HTTP on /mcp, legacy SSE on /sse + /messages."""
import json, threading, time, queue
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TOOLS = [{"name": "echo", "description": "Echo text", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}]
sse_clients = []
flaky_drops = 0

def rpc(method, params, mid):
    if method == "initialize":
        return {"protocolVersion": "2025-03-26", "capabilities": {"tools": {}}, "serverInfo": {"name": "mock-http", "version": "0.1"}}
    if method == "tools/list":
        return {"tools": TOOLS}
    if method == "tools/call":
        text = params.get("arguments", {}).get("text", "")
        return {"content": [{"type": "text", "text": f"echo: {text}"}], "isError": False}
    if method == "shutdown":
        return {}
    return None

class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _sse_headers(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()

    def do_GET(self):
        if self.path == "/sse":
            self._sse_headers()
            q = queue.Queue()
            sse_clients.append(q)
            try:
                self.wfile.write(b"event: endpoint\ndata: /messages\n\n")
                self.wfile.flush()
                while True:
                    data = q.get(timeout=300)
                    self.wfile.write(f"event: message\ndata: {data}\n\n".encode())
                    self.wfile.flush()
            except Exception:
                pass
            finally:
                if q in sse_clients:
                    sse_clients.remove(q)
        elif self.path == "/sse-flaky":
            # Relays messages like /sse, but the *first* connection ever made
            # drops itself right after relaying one message — simulating a
            # server restart / network blip mid-session. Every later GET
            # (i.e. the client's reconnect) behaves like a normal, stable
            # stream. This lets a test prove: the call in flight when the
            # drop happens still completes (bytes were already flushed), and
            # the *next* call also succeeds once the client reconnects.
            global flaky_drops
            self._sse_headers()
            q = queue.Queue()
            sse_clients.append(q)
            self.wfile.write(b"event: endpoint\ndata: /messages\n\n")
            self.wfile.flush()
            drop_after_one = flaky_drops < 1
            try:
                while True:
                    data = q.get(timeout=300)
                    self.wfile.write(f"event: message\ndata: {data}\n\n".encode())
                    self.wfile.flush()
                    if drop_after_one:
                        flaky_drops += 1
                        return
            except Exception:
                pass
            finally:
                if q in sse_clients:
                    sse_clients.remove(q)
        else:
            self.send_error(404)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        req = json.loads(self.rfile.read(length))
        mid = req.get("id")
        method = req.get("method", "")

        if self.path == "/mcp":
            result = rpc(method, req.get("params", {}), mid)
            if mid is None:
                self.send_response(202)
                self.end_headers()
                return
            payload = json.dumps({"jsonrpc": "2.0", "id": mid, "result": result})
            mode = self.headers.get("x-test-mode", "json")
            if mode == "sse":
                self._sse_headers()
                self.wfile.write(f"data: {payload}\n\n".encode())
                self.wfile.flush()
            else:
                body = payload.encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.send_header("mcp-session-id", "sess-123")
                self.end_headers()
                self.wfile.write(body)
        elif self.path == "/messages":
            result = rpc(method, req.get("params", {}), mid)
            if mid is None:
                self.send_response(202)
                self.end_headers()
                return
            payload = json.dumps({"jsonrpc": "2.0", "id": mid, "result": result})
            for q in list(sse_clients):
                q.put(payload)
            self.send_response(202)
            self.end_headers()
        else:
            self.send_error(404)

def main(port):
    server = ThreadingHTTPServer(("127.0.0.1", port), H)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    print("ready", flush=True)
    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        pass

if __name__ == "__main__":
    import sys
    main(int(sys.argv[1]))
