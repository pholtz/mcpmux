#!/usr/bin/env python3
"""Fake stdio MCP server for exercising mcpmux: concurrency, progress, cancellation,
crashes and process-group cleanup. Spawns a `sleep` grandchild like npx/uvx would."""
import json, subprocess, sys, threading, time

subprocess.Popen(["sleep", "3600"])  # grandchild that must die with the group
lock = threading.Lock()
cancelled = set()

def send(msg):
    with lock:
        sys.stdout.write(json.dumps(msg) + "\n")
        sys.stdout.flush()

def call(req):
    p = req["params"]; name = p["name"]; args = p.get("arguments", {})
    if name == "crash":
        sys.stderr.write("crashing on purpose\n"); sys.stderr.flush()
        import os; os._exit(3)
    if name == "sleep":
        token = p.get("_meta", {}).get("progressToken")
        secs = float(args.get("secs", 1))
        steps = 4
        for i in range(steps):
            time.sleep(secs / steps)
            if req["id"] in cancelled:
                sys.stderr.write(f"cancelled {req['id']}\n"); sys.stderr.flush()
                return
            if token is not None:
                send({"jsonrpc": "2.0", "method": "notifications/progress",
                      "params": {"progressToken": token, "progress": i + 1, "total": steps}})
    send({"jsonrpc": "2.0", "id": req["id"],
          "result": {"content": [{"type": "text", "text": f"{name} {json.dumps(args)}"}]}})

tools = [{"name": n, "inputSchema": {"type": "object"}} for n in ("echo", "sleep", "crash", "connect")]
for line in sys.stdin:
    req = json.loads(line)
    m = req.get("method")
    if m == "initialize":
        send({"jsonrpc": "2.0", "id": req["id"], "result": {
            "protocolVersion": req["params"]["protocolVersion"],
            "capabilities": {"tools": {"listChanged": True}},
            "serverInfo": {"name": "fake", "version": "1"}, "instructions": "fake server"}})
    elif m == "tools/list":
        send({"jsonrpc": "2.0", "id": req["id"], "result": {"tools": tools}})
    elif m == "tools/call":
        threading.Thread(target=call, args=(req,), daemon=True).start()
    elif m == "notifications/cancelled":
        cancelled.add(req["params"]["requestId"])
    elif "id" in req and m:
        send({"jsonrpc": "2.0", "id": req["id"], "error": {"code": -32601, "message": m}})
