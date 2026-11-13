import json
import os
import sys
import time

PAGE_ONE = [
    {
        "name": "echo",
        "description": "Say back the text, and the folder the client calls its root.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "What to say"},
                "loud": {"type": "boolean"},
            },
            "required": ["text"],
        },
    }
]
PAGE_TWO = [
    {"name": "shot", "description": "A tiny picture.", "inputSchema": {"type": "object"}},
    {"name": "fail", "description": "Always fails.", "inputSchema": {"type": "object"}},
    {"name": "slow", "description": "Takes a minute.", "inputSchema": {"type": "object"}},
    {"name": "die", "description": "Exits at once.", "inputSchema": {"type": "object"}},
    {"name": "env", "description": "Says FAKE_MCP_VAR.", "inputSchema": {"type": "object"}},
]


def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def read():
    while True:
        line = sys.stdin.readline()
        if not line:
            sys.exit(0)
        line = line.strip()
        if line:
            return json.loads(line)


def answer(id, result):
    send({"jsonrpc": "2.0", "id": id, "result": result})


def text(t):
    return {"content": [{"type": "text", "text": t}]}


while True:
    msg = read()
    method = msg.get("method")
    id = msg.get("id")
    if id is None:
        continue
    if method == "initialize":
        answer(id, {
            "protocolVersion": msg["params"]["protocolVersion"],
            "capabilities": {"tools": {"listChanged": True}},
            "serverInfo": {"name": "fake", "version": "1.2"},
            "instructions": "Use echo to test.",
        })
    elif method == "tools/list":
        if msg["params"].get("cursor") == "two":
            answer(id, {"tools": PAGE_TWO})
        else:
            answer(id, {"tools": PAGE_ONE, "nextCursor": "two"})
    elif method == "tools/call":
        name = msg["params"]["name"]
        args = msg["params"].get("arguments", {})
        if name == "echo":
            send({"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info", "data": "hi"}})
            send({"jsonrpc": "2.0", "id": "r1", "method": "roots/list"})
            roots = read()
            root = roots["result"]["roots"][0]["uri"]
            answer(id, text(args["text"] + " from " + root))
        elif name == "shot":
            answer(id, {"content": [
                {"type": "text", "text": "here it is"},
                {"type": "image", "mimeType": "image/png", "data": "iVBORw0KGgo="},
            ]})
        elif name == "fail":
            answer(id, {"content": [{"type": "text", "text": "the board is missing"}], "isError": True})
        elif name == "slow":
            time.sleep(60)
            answer(id, text("late"))
        elif name == "die":
            sys.stderr.write("fake: out of memory\n")
            sys.stderr.flush()
            os._exit(3)
        elif name == "env":
            answer(id, text(os.environ.get("FAKE_MCP_VAR", "unset") + " in " + os.getcwd()))
        else:
            send({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": "unknown tool " + name}})
    else:
        send({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "no " + method}})
