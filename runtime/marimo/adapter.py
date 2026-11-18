"""SPIKE, throwaway: marimo's adapter for the runtime core.

Speaks the protocol R's adapter speaks (runtime/r/adapter.R, notebooks/r.rs):
`POST /adapter` {"method", "params"} -> {"result"} | {"error"}, and
`GET /notifications?after=<seq>` long-polled, both behind `Authorization:
Bearer <engines' key>`. So the core's `R` upstream drives it as it is.

Inside: one `marimo edit <folder> --headless --sandbox --watch` child on a
private port and token, with a private marimo config (lazy, no autorun, no AI).
Per notebook: a kiosk WebSocket that mirrors marimo's state (reads never touch
the kernel, so they answer while a cell runs), and writes through code mode
(`POST /api/kernel/execute`), the one write path marimo has.

Run: uv run --no-project --with marimo==0.25.1 --with websockets --with httpx \
       python adapter.py <folder> <bridge_port> <marimo_port>   (key on stdin)
"""

import asyncio, json, os, secrets, subprocess, sys, tempfile, time
from pathlib import Path

import httpx, uvicorn, websockets
from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import JSONResponse
from starlette.routing import Route

MARIMO_CONFIG = """[runtime]
on_cell_change = "lazy"
auto_instantiate = false
watcher_on_save = "lazy"
[completion]
copilot = false
[save]
autosave = "after_delay"
"""


class Engine:
    def __init__(self, folder: Path, port: int):
        self.folder, self.port = folder.resolve(), port
        self.token = secrets.token_hex(16)
        self.base = f"http://127.0.0.1:{port}"
        self.books: dict[str, "Book"] = {}  # notebook id (= path relative to folder) -> book
        self.seq = 0
        self.notes: list[dict] = []
        self.changed = asyncio.Event()

    def start(self):
        cfg = Path(tempfile.mkdtemp(prefix="endeavor-marimo-"))
        (cfg / "marimo").mkdir()
        (cfg / "marimo" / "marimo.toml").write_text(MARIMO_CONFIG)
        tok = cfg / "token"
        tok.write_text(self.token)
        env = {**os.environ, "XDG_CONFIG_HOME": str(cfg)}
        self.child = subprocess.Popen(
            ["marimo", "-q", "edit", str(self.folder), "--headless", "--sandbox", "--watch", "--skip-update-check",
             "--host", "127.0.0.1", "--port", str(self.port), "--token-password-file", str(tok)], env=env)

    async def ready(self, timeout=120):
        end = time.time() + timeout
        async with httpx.AsyncClient() as c:
            while time.time() < end:
                try:
                    if (await c.get(self.base + "/health")).status_code == 200:
                        return
                except httpx.HTTPError:
                    pass
                await asyncio.sleep(0.3)
        raise RuntimeError("marimo didn't start")

    def note(self, method, **params):
        self.seq += 1
        self.notes.append({"method": method, "params": params, "seq": self.seq})
        del self.notes[:-500]
        self.changed.set()

    async def sessions(self) -> dict:
        async with httpx.AsyncClient() as c:
            r = await c.get(self.base + "/api/sessions", headers={"Authorization": f"Bearer {self.token}"})
            return r.json()

    async def execute(self, book: "Book", code: str, timeout=None) -> tuple[bool, str]:
        """Run Python in the notebook's kernel (code mode). Whether it succeeded, and what it printed."""
        sid = await book.session_id()
        out, ok = [], False
        headers = {"Authorization": f"Bearer {self.token}", "Marimo-Session-Id": sid}
        async with httpx.AsyncClient(timeout=timeout) as c:
            async with c.stream("POST", self.base + "/api/kernel/execute", headers=headers, json={"code": code}) as r:
                event = None
                async for line in r.aiter_lines():
                    if line.startswith("event: "):
                        event = line[7:]
                    elif line.startswith("data: "):
                        data = json.loads(line[6:])
                        if event in ("stdout", "stderr"):
                            out.append(data["data"])
                        elif event == "done":
                            ok = data.get("success", False)
        return ok, "".join(out)


class Book:
    """One open notebook: marimo's state as its kiosk socket tells it."""

    def __init__(self, engine: Engine, rel: str):
        self.engine, self.rel = engine, rel
        self.order: list[str] = []
        self.cells: dict[str, dict] = {}
        self.ready = asyncio.Event()

    async def session_id(self) -> str:
        # The session is re-keyed when a page (the app's pane) takes it over, so ask each time.
        path = os.path.realpath(self.engine.folder / self.rel)
        sessions = await self.engine.sessions()
        for sid, s in sessions.items():
            if os.path.realpath(s.get("path") or s.get("filename") or "") == path:
                return sid
        raise RuntimeError(f"notebook_closed::marimo has no session for this notebook ({sessions})")

    async def open(self):
        e = self.engine
        url = f"ws://127.0.0.1:{e.port}/ws?file={self.rel}&access_token={e.token}"
        # Make the session as its editor, then leave, so a page that opens later resumes it as the editor
        # (marimo has one editor per notebook) while we watch as a kiosk.
        maker = await websockets.connect(url + f"&session_id=endeavor-{secrets.token_hex(4)}", max_size=None)
        # A sandboxed notebook first builds its environment (startup-progress), then says kernel-ready.
        first = {}
        while first.get("op") != "kernel-ready":
            first = json.loads(await asyncio.wait_for(maker.recv(), 300))
        await maker.close()
        sid = await self.session_id()
        self.ws = await websockets.connect(url + f"&session_id={sid}&kiosk=true", max_size=None)
        if first.get("op") == "kernel-ready":
            self.kernel_ready(first["data"])
        asyncio.create_task(self.follow())
        await asyncio.wait_for(self.ready.wait(), 300)

    def kernel_ready(self, d):
        self.order = list(d["cell_ids"])
        for cid, code, name in zip(d["cell_ids"], d["codes"], d["names"]):
            self.cells.setdefault(cid, {"code": code, "name": name, "status": "idle", "stale": True, "output": None, "console": [], "errored": False})
        self.ready.set()

    async def follow(self):
        try:
            async for raw in self.ws:
                self.on(json.loads(raw))
        finally:
            self.engine.note("notebook_shut_down", notebook_id=self.rel)

    def on(self, m):
        op, d = m.get("op"), m.get("data") or {}
        if op == "kernel-ready":
            self.kernel_ready(d)
        elif op == "notebook-document-transaction":
            for ch in d["transaction"]["changes"]:
                t, cid = ch["type"], ch.get("cellId")
                if t == "create-cell":
                    self.cells[cid] = {"code": ch["code"], "name": ch.get("name", ""), "status": "idle", "stale": True, "output": None, "console": [], "errored": False}
                    at = self.order.index(ch["after"]) + 1 if ch.get("after") in self.order else (self.order.index(ch["before"]) if ch.get("before") in self.order else len(self.order))
                    self.order.insert(at, cid)
                elif t == "set-code" and cid in self.cells:
                    self.cells[cid]["code"] = ch["code"]
                elif t == "delete-cell" and cid in self.cells:
                    self.order.remove(cid)
                    del self.cells[cid]
                elif t == "reorder-cells":
                    self.order = list(ch["cellIds"])
            self.engine.note("topology_changed", notebook_id=self.rel)
            self.engine.note("file_saved", notebook_id=self.rel)
        elif op == "cell-op" and d.get("cell_id") in self.cells:
            c = self.cells[d["cell_id"]]
            if d.get("status"):
                c["status"] = d["status"]
                if d["status"] == "running":
                    c["console"], c["stale"] = [], False
            if d.get("stale_inputs"):
                c["stale"] = True
            if d.get("output") is not None:
                c["output"] = d["output"]
                c["errored"] = d["output"].get("channel") == "marimo-error"
            if d.get("console"):
                c["console"] += d["console"] if isinstance(d["console"], list) else [d["console"]]
            self.engine.note("cell_state", notebook_id=self.rel, cells=[self.cell_state(d["cell_id"])])
        elif op == "completed-run":
            self.engine.note("execution_done", notebook_id=self.rel)

    def cell_state(self, cid):
        c = self.cells[cid]
        return {"cell_id": cid, "code": c["code"], "running": c["status"] == "running", "queued": c["status"] == "queued", "errored": c["errored"]}

    def snapshot(self):
        cells = []
        for cid in self.order:
            c = self.cells[cid]
            out = c["output"] or {}
            cells.append({"cell_id": cid, "code": c["code"], "running": c["status"] == "running", "queued": c["status"] == "queued",
                          "errored": c["errored"], "unrun": c["stale"], "output": summary(out),
                          "error": out.get("data") if c["errored"] else None,
                          "stdout": "".join(x.get("data", "") for x in c["console"] if x.get("channel") in ("stdout", "stderr"))[-4000:]})
        return {"notebook_id": self.rel, "path": str(self.engine.folder / self.rel), "order": self.order, "cells": cells,
                "execution_allowed": True, "safe_preview": False}


def summary(out):
    mime, data = out.get("mimetype"), out.get("data")
    if not mime or data in (None, ""):
        return None
    if mime == "application/vnd.marimo+mimebundle":
        bundle = data if isinstance(data, dict) else json.loads(data)
        return {"mime": sorted(bundle), "text": bundle.get("text/plain")}
    return {"mime": [mime], "text": data[:2000] if isinstance(data, str) else None}


# ---- calls -------------------------------------------------------------------

CM = "import marimo._code_mode as cm, json\nctx = cm.get_context(skip_staleness_check=True)\n"


async def call(e: Engine, method: str, p: dict):
    if method == "status":
        return {"seq": e.seq, "notebooks": len(e.books), "marimo": "0.25.1"}
    if method == "open":
        rel = str(Path(p["path"]).resolve().relative_to(e.folder))
        if rel not in e.books:
            book = Book(e, rel)
            e.books[rel] = book
            await book.open()
            e.note("notebook_opened", notebook_id=rel, path=str(e.folder / rel))
        return {"notebook_id": rel, "path": str(e.folder / rel), "status": "ready"}
    if method == "snapshot" and not p.get("notebook_id"):
        return {"notebooks": [b.snapshot() for b in e.books.values()], "seq": e.seq}
    book = e.books.get(p.get("notebook_id", ""))
    if book is None:
        raise KeyError("no_notebook::That notebook isn't open")
    if method == "snapshot":
        return {**book.snapshot(), "seq": e.seq}
    if method == "graph":
        ok, out = await e.execute(book, CM + "g = ctx.graph\nprint(json.dumps({c: {'defs': sorted(v.defs), 'refs': sorted(v.refs), 'up': sorted(g.parents.get(c, ())), 'down': sorted(g.children.get(c, ()))} for c, v in g.cells.items()}))\n")
        return {"cells": json.loads(out.strip().splitlines()[-1]), "order": book.order} if ok else err(out)
    if method == "apply":
        lines = [CM, "async with ctx:\n", "    new = []\n"]
        for op in p["ops"]:
            if op["op"] == "set":
                cur = book.cells.get(op["cell_id"], {}).get("code")
                if "expected" in op and cur != op["expected"]:
                    raise ValueError(f"code_changed::Cell {op['cell_id']} changed since it was read")
                lines.append(f"    ctx.edit_cell({op['cell_id']!r}, code={op['code']!r})\n")
            elif op["op"] == "insert":
                i = op.get("index", len(book.order))
                where = f"after={book.order[i - 1]!r}" if 0 < i <= len(book.order) else (f"before={book.order[0]!r}" if book.order else "")
                lines.append(f"    new.append(ctx.create_cell({op['code']!r}, hide_code=False{', ' + where if where else ''}))\n")
            elif op["op"] == "delete":
                lines.append(f"    ctx.delete_cell({op['cell_id']!r})\n")
            elif op["op"] == "move":
                i = op["index"]
                rest = [c for c in book.order if c != op["cell_id"]]
                where = f"after={rest[i - 1]!r}" if i > 0 else f"before={rest[0]!r}"
                lines.append(f"    ctx.move_cell({op['cell_id']!r}, {where})\n")
        lines.append("print(json.dumps(new))\n")
        ok, out = await e.execute(book, "".join(lines))
        return {"inserted": json.loads(out.strip().splitlines()[-1]), "seq": e.seq} if ok else err(out)
    if method == "run":
        # Code mode's run_cell runs the cell's stale ancestors too, as Endeavor's rule asks.
        code = CM + "async with ctx:\n" + "".join(f"    ctx.run_cell({c!r})\n" for c in p["cells"])
        task = asyncio.create_task(e.execute(book, code))
        if not p.get("wait"):
            return {"accepted": True}
        done, _ = await asyncio.wait([task], timeout=p.get("timeout") or 60)
        running = [c for c in book.order if book.cells[c]["status"] in ("running", "queued")]
        return {"accepted": True, "finished": [c for c in p["cells"] if c not in running], "timed_out": running if not done else []}
    if method == "interrupt":
        async with httpx.AsyncClient() as c:
            await c.post(e.base + "/api/kernel/interrupt", headers={"Authorization": f"Bearer {e.token}", "Marimo-Session-Id": await book.session_id()})
        return {}
    if method == "render_png":
        out = book.cells[p["cell_id"]]["output"] or {}
        if out.get("mimetype") == "application/vnd.marimo+mimebundle":
            bundle = out["data"] if isinstance(out["data"], dict) else json.loads(out["data"])
            png = bundle.get("image/png", "")
            return {"png": png.split(",", 1)[-1] if png else None, "mime": "image/png" if png else sorted(bundle)}
        if out.get("mimetype") == "image/png":
            return {"png": out["data"].split(",", 1)[-1], "mime": "image/png"}
        return {"png": None, "mime": out.get("mimetype")}
    if method == "validate":
        import ast
        try:
            ast.parse(p["code"])
            return {"errors": []}
        except SyntaxError as x:
            return {"errors": [{"line": x.lineno, "message": x.msg}]}
    raise KeyError(f"unknown_method::Unknown adapter method: '{method}'")


def err(out):
    raise RuntimeError("marimo_error::" + out.strip()[-2000:])


def make_app(e: Engine, key: str):
    def authorized(req):
        return secrets.compare_digest(req.headers.get("authorization", ""), f"Bearer {key}")

    async def adapter(req: Request):
        if not authorized(req):
            return JSONResponse({"error": "unauthorized"}, 401)
        msg = await req.json()
        try:
            return JSONResponse({"result": await call(e, msg.get("method", ""), msg.get("params") or {})})
        except Exception as x:  # noqa: BLE001 - every failure is an answer
            text = str(x).strip("'\"")
            return JSONResponse({"error": text if "::" in text else f"python_error::{text}"})

    async def notifications(req: Request):
        if not authorized(req):
            return JSONResponse({"error": "unauthorized"}, 401)
        after = int(req.query_params.get("after", "0"))
        end = time.time() + 25
        while time.time() < end:
            new = [n for n in e.notes if n["seq"] > after]
            if new:
                return JSONResponse(new)
            e.changed.clear()
            try:
                await asyncio.wait_for(e.changed.wait(), end - time.time())
            except asyncio.TimeoutError:
                pass
        return JSONResponse([])

    async def health(req):
        return JSONResponse({"ok": True})

    return Starlette(routes=[Route("/adapter", adapter, methods=["POST"]), Route("/notifications", notifications), Route("/health", health)])


async def main():
    folder, bridge_port, marimo_port = Path(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3])
    key = sys.stdin.readline().strip()
    e = Engine(folder, marimo_port)
    e.start()
    await e.ready()
    server = uvicorn.Server(uvicorn.Config(make_app(e, key), host="127.0.0.1", port=bridge_port, log_level="warning"))
    try:
        await server.serve()
    finally:
        e.child.terminate()


if __name__ == "__main__":
    asyncio.run(main())
