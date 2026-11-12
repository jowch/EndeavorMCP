#!/usr/bin/env python3
"""A throwaway MCP server for one question: does Claude's desktop app draw an MCP App view
without its inset, bordered box ("Widget from <server>") for some kind of server?

It has one tool, `show_probe`, whose view is a full-bleed coloured page that prints what the
host sent it (theme, sizes, the frame's own size). No notebook, no Julia, no files, nothing
else. Standard library only.

    python3 probe_server.py stdio                 # stdio, as a local server
    python3 probe_server.py http 127.0.0.1 8765   # streamable HTTP on loopback only

The HTTP mode binds 127.0.0.1 only and refuses any other address.
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

URI = "ui://frame-probe/view"
MIME = "text/html;profile=mcp-app"
UI = {"prefersBorder": False, "preferredDisplayMode": "fullscreen"}

# The SDK isn't loaded: the view speaks the protocol's few messages itself (JSON-RPC over postMessage).
PAGE = """<!DOCTYPE html><html><head><meta charset="utf-8"><meta name="color-scheme" content="light dark">
<style>html,body{margin:0;height:100%;background:#CC3F00;color:#fff;font:14px system-ui}
#out{padding:12px;white-space:pre-wrap;word-break:break-all}
button{font:inherit;margin:12px;padding:8px 14px}</style></head><body>
<button id="go">Open in the panel</button><div id="out">waiting for the host…</div>
<script>
const out = document.getElementById("out");
let id = 0; const waits = {};
const send = (method, params) => new Promise(ok => { const i = ++id; waits[i] = ok; parent.postMessage({ jsonrpc: "2.0", id: i, method, params }, "*"); });
const show = ctx => { out.textContent = "frame-probe view\\n\\nwindow: " + innerWidth + "x" + innerHeight + "\\n\\nhost context:\\n" + JSON.stringify(ctx, (k, v) => k === "styles" ? "(" + Object.keys(v?.variables || {}).length + " variables)" : v, 1); };
let ctx = {};
addEventListener("message", e => {
  const m = e.data || {};
  if (m.id && waits[m.id]) { waits[m.id](m.result); delete waits[m.id]; return; }
  if (m.method === "ui/notifications/host-context-changed") { ctx = Object.assign(ctx, m.params); show(ctx); }
});
document.getElementById("go").onclick = () => send("ui/request-display-mode", { mode: "fullscreen" }).then(r => { out.textContent += "\\n\\nrequest-display-mode: " + JSON.stringify(r); });
send("ui/initialize", { protocolVersion: "2026-01-26", appInfo: { name: "frame-probe", version: "1" }, appCapabilities: { availableDisplayModes: ["inline", "fullscreen"] } }).then(r => {
  ctx = (r && r.hostContext) || {}; show(ctx);
  parent.postMessage({ jsonrpc: "2.0", method: "ui/notifications/initialized", params: {} }, "*");
});
</script></body></html>"""

TOOL = {
    "name": "show_probe",
    "title": "Frame probe",
    "description": "Show the frame probe view. Call it when the user asks for the frame probe.",
    "inputSchema": {"type": "object", "properties": {}},
    "_meta": {"ui": {"resourceUri": URI, "prefersBorder": False}, "ui/resourceUri": URI},
}


def answer(msg):
    """The reply to one JSON-RPC message, or None for a notification."""
    if "id" not in msg or msg.get("id") is None:
        return None
    method, params = msg.get("method"), msg.get("params") or {}
    if method == "initialize":
        result = {"protocolVersion": params.get("protocolVersion", "2025-06-18"),
                  "capabilities": {"tools": {}, "resources": {}},
                  "serverInfo": {"name": "frame-probe", "title": "Frame probe", "version": "1"}}
    elif method == "ping":
        result = {}
    elif method == "tools/list":
        result = {"tools": [TOOL]}
    elif method == "tools/call":
        result = {"content": [{"type": "text", "text": "The frame probe is showing."}], "structuredContent": {"shown": True}}
    elif method == "resources/list":
        result = {"resources": [{"uri": URI, "name": "probe", "title": "Frame probe", "mimeType": MIME, "_meta": {"ui": UI}}]}
    elif method == "resources/read":
        if params.get("uri") != URI:
            return {"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32002, "message": "no such resource"}}
        result = {"contents": [{"uri": URI, "mimeType": MIME, "text": PAGE, "_meta": {"ui": UI}}]}
    else:
        return {"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": "Method not found"}}
    return {"jsonrpc": "2.0", "id": msg["id"], "result": result}


def stdio():
    for line in sys.stdin:
        if not line.strip():
            continue
        reply = answer(json.loads(line))
        if reply is not None:
            sys.stdout.write(json.dumps(reply) + "\n")
            sys.stdout.flush()


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path != "/mcp":
            self.send_error(404)
            return
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")
        replies = [r for r in (answer(m) for m in (body if isinstance(body, list) else [body])) if r is not None]
        if not replies:
            self.send_response(202)
            self.end_headers()
            return
        data = json.dumps(replies if isinstance(body, list) else replies[0]).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        # No server-to-client stream.
        self.send_error(405)

    def do_DELETE(self):
        self.send_response(200)
        self.end_headers()

    def log_message(self, *args):
        sys.stderr.write("frame-probe: " + (args[0] % args[1:]) + "\n")


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "http":
        host = sys.argv[2] if len(sys.argv) > 2 else "127.0.0.1"
        if host != "127.0.0.1":
            sys.exit("frame-probe: loopback only")
        port = int(sys.argv[3]) if len(sys.argv) > 3 else 8765
        HTTPServer((host, port), Handler).serve_forever()
    else:
        stdio()
