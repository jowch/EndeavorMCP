#!/usr/bin/env python3
# Endeavor sandbox probe: a throwaway stdio MCP server whose tools open MCP App
# views that report what the host's sandbox allows. Standard library only.
import json, os, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
LOG = os.path.join(os.environ.get("PROBE_LOG_DIR", HERE), "report.log")
VIEW = "ui://endeavor-probe/view"
PLUTO = "ui://endeavor-probe/pluto"
MIME = "text/html;profile=mcp-app"

def page(name):
    return open(os.path.join(HERE, name), encoding="utf-8").read()

TOOLS = [
    {"name": "probe_sandbox", "title": "Probe the view sandbox",
     "description": "Open a small view that tests what this app's view sandbox allows.",
     "inputSchema": {"type": "object", "properties": {}},
     "_meta": {"ui": {"resourceUri": VIEW}, "ui/resourceUri": VIEW}},
    {"name": "probe_pluto", "title": "Open the Pluto test notebook",
     "description": "Open a demo Pluto notebook inside a view, to test whether Pluto's page runs there.",
     "inputSchema": {"type": "object", "properties": {}},
     "_meta": {"ui": {"resourceUri": PLUTO}, "ui/resourceUri": PLUTO}},
    {"name": "probe_report", "title": "Probe report",
     "description": "Used by the probe views to save what they found.",
     "inputSchema": {"type": "object", "properties": {"report": {"type": "string"}}, "required": ["report"]},
     "_meta": {"ui": {"visibility": ["app"]}}},
]

def log(line):
    with open(LOG, "a", encoding="utf-8") as f:
        f.write(time.strftime("%H:%M:%S ") + line + "\n")

def handle(msg):
    method, params = msg.get("method"), msg.get("params") or {}
    if method == "initialize":
        log("initialize " + json.dumps(params)[:1500])
        return {"protocolVersion": params.get("protocolVersion", "2025-06-18"),
                "capabilities": {"tools": {}, "resources": {}},
                "serverInfo": {"name": "endeavor-probe", "version": "0.0.1"}}
    if method == "tools/list":
        return {"tools": TOOLS}
    if method == "resources/list":
        return {"resources": [{"uri": VIEW, "name": "Probe view", "mimeType": MIME},
                              {"uri": PLUTO, "name": "Pluto probe", "mimeType": MIME}]}
    if method == "resources/read":
        uri = params.get("uri")
        text = page("view.built.html") if uri == VIEW else page("pluto.built.html")
        log(f"resources/read {uri} {len(text)} chars")
        return {"contents": [{"uri": uri, "mimeType": MIME, "text": text,
                              "_meta": {"ui": {"prefersBorder": True}}}]}
    if method == "tools/call":
        name, args = params.get("name"), params.get("arguments") or {}
        log(f"tools/call {name} {json.dumps(args)[:4000]}")
        if name == "probe_report":
            return {"content": [{"type": "text", "text": "saved"}]}
        return {"content": [{"type": "text", "text": f"Opened the {name} view."}],
                "structuredContent": {"opened": name}}
    if method and method.startswith("notifications/"):
        return None
    raise KeyError(method)

for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    msg = json.loads(raw)
    try:
        result = handle(msg)
        reply = None if "id" not in msg else {"jsonrpc": "2.0", "id": msg["id"], "result": result}
    except Exception as e:
        reply = None if "id" not in msg else {"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": str(e)}}
    if reply is not None:
        sys.stdout.write(json.dumps(reply) + "\n")
        sys.stdout.flush()
