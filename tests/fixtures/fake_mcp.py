"""A tiny MCP stdio server for bhai's tests: initialize, two tools, and an echo call.

`fake_mcp.py hang` never answers; `fake_mcp.py exit` quits at once.
"""

import json
import sys
import time

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


def reply(id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": id, "result": result}) + "\n")
    sys.stdout.flush()


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else ""
    if mode == "exit":
        sys.exit(1)
    print("fake_mcp started", file=sys.stderr, flush=True)
    for line in sys.stdin:
        if mode == "hang":
            time.sleep(60)
        msg = json.loads(line)
        method, id = msg.get("method"), msg.get("id")
        if id is None:
            continue
        if method == "initialize":
            reply(id, {
                "protocolVersion": msg["params"]["protocolVersion"],
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fake", "version": "0.0.1"},
            })
        elif method == "tools/list":
            reply(id, {"tools": TOOLS})
        elif method == "tools/call":
            name = msg["params"]["name"]
            args = msg["params"].get("arguments") or {}
            if name == "echo":
                reply(id, {"content": [{"type": "text", "text": "echo: " + args.get("message", "")}]})
            else:
                reply(id, {"content": [{"type": "text", "text": "it failed"}], "isError": True})
        else:
            sys.stdout.write(json.dumps({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": "method not found"},
            }) + "\n")
            sys.stdout.flush()


main()
