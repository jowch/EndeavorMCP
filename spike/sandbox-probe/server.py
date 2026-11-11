#!/usr/bin/env python3
# Endeavor sandbox probe: a throwaway stdio MCP server whose tools open MCP App
# views that report what the host's sandbox allows. Standard library only.
import json, os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import wsrelay

HERE = os.path.dirname(os.path.abspath(__file__))
LOG = os.path.join(os.environ.get("PROBE_LOG_DIR", HERE), "report.log")
VIEW = "ui://endeavor-probe/view"
PLUTO = "ui://endeavor-probe/pluto"
MIME = "text/html;profile=mcp-app"
SIZE_MB = 1.0
MEM = {}

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
    {"name": "probe_loader", "title": "Open the notebook through the loader",
     "description": "Open the demo Pluto notebook through a small view that loads the page in parts.",
     "inputSchema": {"type": "object", "properties": {}},
     "_meta": {"ui": {"resourceUri": "ui://endeavor-probe/loader"}, "ui/resourceUri": "ui://endeavor-probe/loader"}},
    {"name": "probe_cdn", "title": "Open the notebook from Pluto's CDN",
     "description": "Open the demo Pluto notebook in a view that loads Pluto's page code from Pluto's public CDN.",
     "inputSchema": {"type": "object", "properties": {}},
     "_meta": {"ui": {"resourceUri": "ui://endeavor-probe/cdn"}, "ui/resourceUri": "ui://endeavor-probe/cdn"}},
] + [
    {"name": f"probe_size_{mb}", "title": f"Test a {mb} MB view",
     "description": f"Open the probe view padded to {mb} MB, to find the largest view that renders.",
     "inputSchema": {"type": "object", "properties": {}},
     "_meta": {"ui": {"resourceUri": f"ui://endeavor-probe/size/{mb}"}, "ui/resourceUri": f"ui://endeavor-probe/size/{mb}"}}
    for mb in (2, 4, 6)] + [
    {"name": "page_part", "title": "Page part",
     "description": "Used by the loader view to read the notebook page in parts.",
     "inputSchema": {"type": "object", "properties": {"offset": {"type": "number"}, "size": {"type": "number"}}, "required": ["offset", "size"]},
     "_meta": {"ui": {"visibility": ["app"]}}},
    {"name": "probe_live", "title": "Open the live notebook",
     "description": "Open the demo Pluto notebook live, connected to its running Pluto.",
     "inputSchema": {"type": "object", "properties": {}},
     "_meta": {"ui": {"resourceUri": "ui://endeavor-probe/live"}, "ui/resourceUri": "ui://endeavor-probe/live"}},
    {"name": "ws_open", "title": "Relay open", "description": "Used by the live view.", "inputSchema": {"type": "object", "properties": {}}, "_meta": {"ui": {"visibility": ["app"]}}},
    {"name": "ws_send", "title": "Relay send", "description": "Used by the live view.", "inputSchema": {"type": "object", "properties": {"id": {"type": "number"}, "data": {"type": "string"}}, "required": ["id", "data"]}, "_meta": {"ui": {"visibility": ["app"]}}},
    {"name": "ws_recv", "title": "Relay receive", "description": "Used by the live view.", "inputSchema": {"type": "object", "properties": {"id": {"type": "number"}, "wait_ms": {"type": "number"}}, "required": ["id"]}, "_meta": {"ui": {"visibility": ["app"]}}},
    {"name": "probe_relay", "title": "Test the tool relay",
     "description": "Open a view that times calls back to this server and checks whether one server process answers them.",
     "inputSchema": {"type": "object", "properties": {}},
     "_meta": {"ui": {"resourceUri": "ui://endeavor-probe/relay"}, "ui/resourceUri": "ui://endeavor-probe/relay"}},
] + [{"name": n, "title": n, "description": "Used by the relay probe view.", "inputSchema": {"type": "object", "properties": {}}, "_meta": {"ui": {"visibility": ["app"]}}}
     for n in ("relay_echo", "relay_set", "relay_get", "relay_hold", "relay_big")] + [
] + [
    {"name": f"probe_panel_{v}", "title": f"Test opening the side panel ({v})",
     "description": f"Open a small view ({v}) that asks to open in the side panel and reports where it opened.",
     "inputSchema": {"type": "object", "properties": {}},
     "_meta": {"ui": {"resourceUri": f"ui://endeavor-probe/panel-{v}"}, "ui/resourceUri": f"ui://endeavor-probe/panel-{v}"}}
    for v in ("both", "panelonly", "nolist", "nopref")] + [
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
    global SIZE_MB
    if method == "initialize":
        log(f"initialize pid={os.getpid()} " + json.dumps(params)[:1500])
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
        if uri == VIEW: text = page("view.built.html")
        elif uri == PLUTO: text = page("pluto.built.html")
        elif uri.endswith("/loader"): text = page("loader.built.html")
        elif uri.endswith("/cdn"): text = page("cdn.built.html")
        elif uri.endswith("/live"): text = page("live.built.html")
        elif uri.endswith("/relay"): text = page("relay.built.html")
        elif "/panel-" in uri:
            v = uri.rsplit("-", 1)[1]
            text = page(f"panel-{v}.built.html")
        elif "/size/" in uri:
            base = page("view.built.html"); pad = max(0, int(float(uri.rsplit("/", 1)[1]) * 1e6) - len(base))
            text = base.replace("</body>", "<!--" + "x" * pad + "--></body>")
        else: raise KeyError(uri)
        csp = {"resourceDomains": ["https://cdn.jsdelivr.net"], "connectDomains": ["https://cdn.jsdelivr.net"]} if uri.endswith("/cdn") else None
        log(f"resources/read {uri} {len(text)} chars")
        return {"contents": [{"uri": uri, "mimeType": MIME, "text": text,
                              "_meta": {"ui": dict({"prefersBorder": True}, **({"csp": csp} if csp else {}),
                                                    # ask Claude to open the notebook views in the side panel
                                                    **({"preferredDisplayMode": "fullscreen"} if (uri.rsplit("/", 1)[-1] in ("loader", "live", "cdn") or ("/panel-" in uri and not uri.endswith("-nopref"))) else {}))}}]}
    if method == "tools/call":
        name, args = params.get("name"), params.get("arguments") or {}
        log(f"tools/call {name} pid={os.getpid()} {json.dumps(args)[:4000]}")
        if name == "ws_open":
            info = json.load(open(os.path.join(HERE, "live.json")))
            i = wsrelay.ws_open("127.0.0.1", info["port"], "/?secret=" + info["secret"])
            return {"content": [{"type": "text", "text": "open"}], "structuredContent": {"id": i}}
        if name == "ws_send":
            wsrelay.ws_send(int(args["id"]), args["data"])
            return {"content": [{"type": "text", "text": "sent"}], "structuredContent": {"ok": True}}
        if name == "ws_recv":
            msgs, closed = wsrelay.ws_recv(int(args["id"]), int(args.get("wait_ms", 1500)))
            return {"content": [{"type": "text", "text": f"{len(msgs)} messages"}], "structuredContent": {"messages": msgs, "closed": closed}}
        if name == "page_part":
            full = page(args.get("page", "pluto.inner.html")); o, n = int(args["offset"]), int(args["size"])
            part = full[o:o + n]
            # offsets count Python characters; the view only follows "next" (its own string length counts UTF-16 units)
            return {"content": [{"type": "text", "text": f"part at {o}"}], "structuredContent": {"total": len(full), "next": o + len(part), "text": part}}
        if name.startswith("relay_"):
            sc = {"pid": os.getpid()}
            if name == "relay_set": MEM[args["key"]] = args["value"]
            if name == "relay_get": sc["value"] = MEM.get(args["key"])
            if name == "relay_hold": time.sleep(int(args.get("ms", 2000)) / 1000)
            if name == "relay_big": sc["text"] = "x" * int(args.get("n", 1000000))
            return {"content": [{"type": "text", "text": "ok"}], "structuredContent": sc}
        if name == "probe_report":
            return {"content": [{"type": "text", "text": "saved"}]}
        return {"content": [{"type": "text", "text": f"Opened the {name} view."}],
                "structuredContent": {"opened": name}}
    if method and method.startswith("notifications/"):
        return None
    raise KeyError(method)

import threading
OUT = threading.Lock()

def serve(msg):
    try:
        result = handle(msg)
        reply = None if "id" not in msg else {"jsonrpc": "2.0", "id": msg["id"], "result": result}
    except Exception as e:
        reply = None if "id" not in msg else {"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": str(e)}}
    if reply is not None:
        with OUT:
            sys.stdout.write(json.dumps(reply) + "\n")
            sys.stdout.flush()

if __name__ == "__main__":
    for raw in sys.stdin:
        raw = raw.strip()
        if raw:
            threading.Thread(target=serve, args=(json.loads(raw),), daemon=True).start()
