"""A tiny MCP stdio server for bhai's tests: initialize, two tools, and an echo call.

`fake_mcp.py hang` never answers; `fake_mcp.py exit` quits at once; `fake_mcp.py http`
serves the same tools over streamable HTTP on a free port, printed as JSON on stdout.
"""

import json
import sys
import time

TOKEN = "s3cret"

TOOLS = [
    {
        "name": "echo",
        "description": "Echo the message back.\nSecond line.",
        "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}}},
    },
    {
        "name": "fail",
        "description": "Always returns an error result.",
        "inputSchema": {"type": "object"},
    },
]


def result(msg):
    """The JSON-RPC result for a request, or None for an unknown method."""
    method, params = msg.get("method"), msg.get("params") or {}
    if method == "initialize":
        return {
            "protocolVersion": params["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fake", "version": "0.0.1"},
        }
    if method == "tools/list":
        return {"tools": TOOLS}
    if method == "tools/call":
        args = params.get("arguments") or {}
        if params["name"] == "echo":
            return {"content": [{"type": "text", "text": "echo: " + args.get("message", "")}]}
        return {"content": [{"type": "text", "text": "it failed"}], "isError": True}
    return None


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def serve_stdio(mode):
    print("fake_mcp started", file=sys.stderr, flush=True)
    for line in sys.stdin:
        if mode == "hang":
            time.sleep(60)
        msg = json.loads(line)
        id = msg.get("id")
        if id is None:
            continue
        found = result(msg)
        if found is None:
            error = {"code": -32601, "message": "method not found"}
            send({"jsonrpc": "2.0", "id": id, "error": error})
        else:
            send({"jsonrpc": "2.0", "id": id, "result": found})


def serve_http():
    """Streamable HTTP: JSON-RPC over POST, no SSE, and TOKEN on every request."""
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            self.send_error(405)

        def do_DELETE(self):
            self.send_response(200)
            self.end_headers()

        def do_POST(self):
            if self.headers.get("Authorization") != "Bearer " + TOKEN:
                self.send_error(401)
                return
            length = int(self.headers.get("Content-Length", 0))
            msg = json.loads(self.rfile.read(length))
            id = msg.get("id")
            if id is None:
                self.send_response(202)
                self.end_headers()
                return
            body = json.dumps({"jsonrpc": "2.0", "id": id, "result": result(msg)}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Mcp-Session-Id", "fake-session")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    print("fake_mcp started", file=sys.stderr, flush=True)
    send({"port": server.server_port})
    server.serve_forever()


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else ""
    if mode == "exit":
        sys.exit(1)
    if mode == "http":
        serve_http()
    else:
        serve_stdio(mode)


main()
