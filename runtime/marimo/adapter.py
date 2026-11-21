"""marimo's adapter: Python notebooks for the runtime core (docs/runtime-core.md).

It answers the calls R's adapter answers, the same way: `POST /adapter`
{"method", "params"} -> {"result"} | {"error": "<Kind>: <kind>::<text>"}, and
`GET /notifications?after=<seq>` held up to 25 s, both behind
`Authorization: Bearer <engines' key>` (ENDEAVOR_TOKEN). So the core drives it
with the same upstream as R (notebooks/r.rs).

One `marimo edit` server serves every notebook, on a private loopback port
and token, with a private marimo config (lazy, nothing runs on open, no AI)
and `--sandbox`: each notebook gets its own uv environment from its PEP 723
header. Per notebook:

- In safe preview (opened from disk, not yet allowed to run) marimo has no
  session for it: building the sandbox installs packages, which runs code.
  The cells come from marimo's own parser, reading the file, and an edit is
  written back with marimo's own save (its file manager, with no session or
  kernel), so nothing runs and nothing installs (#351). marimo keeps no cell
  ids in the file, so when the notebook goes live the adapter gives marimo's
  session the ids the preview handed out.
- Once it may run, the adapter makes marimo's session as its editor, leaves,
  and watches it as a read-only (kiosk) connection. marimo allows one editor
  per notebook, so a page opened later takes the session over as the editor.
  The adapter's copy of the cells comes from that connection, so reads answer
  while a cell runs. Writes go through the endpoints marimo's own page uses
  (document transactions, save, run, interrupt), which don't wait for a run.

Every call into marimo's internals is in this file; marimo is pinned
(requirements.txt) and tested against the pin (tests/e2e_marimo.rs).

    uv run --no-project --with-requirements requirements.txt python adapter.py
    env: ENDEAVOR_TOKEN, ENDEAVOR_MARIMO_STATE (where to write the ports)
"""

import asyncio
import collections
import ast
import json
import html as html_module
import os
import re
import secrets
import shutil
import signal
import socket
import sys
import tempfile
import time
import urllib.parse
import uuid
from pathlib import Path

import httpx
import uvicorn
import websockets
from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import JSONResponse
from starlette.routing import Route

import marimo
from marimo._ast.load import load_app

TOKEN = os.environ.get("ENDEAVOR_TOKEN", "")
STATE_FILE = os.environ.get("ENDEAVOR_MARIMO_STATE", "")
# The core's /marimo/ route forwards to marimo under this prefix.
BASE_URL = "/marimo"
POLL_HOLD = 25
NOTES_KEPT = 500
TEXT_MAX = 20_000

# Ours, not the user's: nothing runs by itself (an edit marks its readers stale), opening runs
# nothing, a file edited elsewhere marks cells stale, and marimo's own AI is off (one agent).
MARIMO_CONFIG = """[runtime]
on_cell_change = "lazy"
auto_instantiate = false
watcher_on_save = "lazy"
[save]
autosave = "after_delay"
[completion]
copilot = false
[package_management]
manager = "uv"
"""

# A new notebook, as `marimo new` writes one (no cells yet but an empty one), with a PEP 723 header: its
# sandbox's packages. marimo is pinned to the server's version (else marimo adds `marimo>=<newest>`, and
# the kernel can come up newer than the server), and `exclude-newer` holds every package, dependencies
# included, to what was published by the time the notebook was made, as an R notebook's package date
# does. Python is held to the minor version it was made with, since packages from that date may have no
# wheels for a later one. Packages the notebook imports are added with their exact versions
# (`Notebook.install`).
NEW_HEADER = """# /// script
# requires-python = "=={python}.*"
# dependencies = [
#     "marimo=={version}",
# ]
#
# [tool.uv]
# exclude-newer = "{date}"
# ///
"""

NEW_NOTEBOOK = f'''import marimo

__generated_with = "{marimo.__version__}"
app = marimo.App()


@app.cell
def _():
    return


if __name__ == "__main__":
    app.run()
'''


class AdapterError(Exception):
    """An error the core passes on: "ArgumentError: kind::text"."""


def argument_error(kind, text):
    return AdapterError(f"ArgumentError: {kind}::{text}")


def key_error(kind, text):
    # Julia's KeyError form, which the core unwraps (mcp.rs, unwrap_key_error).
    key = f"{kind}::{text}".replace("\\", "\\\\").replace('"', '\\"').replace("$", "\\$")
    return AdapterError(f'KeyError: key "{key}" not found')


def cap(text):
    data = text.encode()
    if len(data) <= TEXT_MAX:
        return text
    return data[:TEXT_MAX].decode(errors="ignore") + f"\n... (cut at {TEXT_MAX} bytes of {len(data)})"


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def real(path):
    return os.path.realpath(os.path.expanduser(path))


# ---- marimo's server ---------------------------------------------------------


class Marimo:
    def __init__(self):
        self.port = free_port()
        # The core makes it (as Ember's secret): it goes into the requests the core's /marimo/ route forwards.
        self.token = os.environ.get("ENDEAVOR_MARIMO_TOKEN") or secrets.token_hex(32)
        self.base = f"http://127.0.0.1:{self.port}{BASE_URL}"
        self.config = Path(tempfile.mkdtemp(prefix="endeavor-marimo-"))
        self.server_token = None

    def start(self):
        (self.config / "marimo").mkdir()
        (self.config / "marimo" / "marimo.toml").write_text(MARIMO_CONFIG)
        token_file = self.config / "token"
        token_file.write_text(self.token)
        token_file.chmod(0o600)
        env = {**os.environ, "XDG_CONFIG_HOME": str(self.config), "MARIMO_SKIP_UPDATE_CHECK": "1"}
        env.pop("ENDEAVOR_TOKEN", None)
        env.pop("ENDEAVOR_MARIMO_TOKEN", None)
        # The whole filesystem is the workspace: notebooks open by absolute path, anywhere. The server
        # listens on loopback only, behind its token, which only the core's /marimo/ route adds.
        root = os.path.abspath(os.sep)
        self.child = asyncio.subprocess.create_subprocess_exec(
            sys.executable, "-m", "marimo", "-q", "edit", root, "--headless", "--sandbox", "--watch",
            "--skip-update-check", "--host", "127.0.0.1", "--port", str(self.port), "--base-url", BASE_URL,
            "--token-password-file", str(token_file), env=env, stdin=asyncio.subprocess.DEVNULL)

    async def wait_ready(self, timeout=120):
        self.child = await self.child
        end = time.time() + timeout
        async with httpx.AsyncClient() as c:
            while time.time() < end:
                if self.child.returncode is not None:
                    raise RuntimeError(f"marimo's server ended ({self.child.returncode})")
                try:
                    if (await c.get(self.base + "/health")).status_code == 200:
                        return
                except httpx.HTTPError:
                    pass
                await asyncio.sleep(0.2)
        raise RuntimeError("marimo's server didn't start within 120 seconds")

    def headers(self, session_id=None):
        h = {"Authorization": f"Bearer {self.token}"}
        if session_id:
            h["Marimo-Session-Id"] = session_id
        if self.server_token:
            h["Marimo-Server-Token"] = self.server_token
        return h

    async def page_token(self, path):
        """The token marimo's page sends with every write (skew protection), from the page itself."""
        if self.server_token is None:
            async with httpx.AsyncClient() as c:
                page = (await c.get(self.base + "/", params={"file": path}, headers=self.headers())).text
            m = re.search(r'"serverToken":\s*"([^"]+)"', page)
            if not m:
                raise RuntimeError("marimo's page has no server token")
            self.server_token = m.group(1)
        return self.server_token

    async def sessions(self):
        async with httpx.AsyncClient() as c:
            r = await c.get(self.base + "/api/sessions", headers=self.headers())
            r.raise_for_status()
            return r.json()

    async def execute(self, session_id, code, timeout=None):
        """Python run in the notebook's kernel by marimo's code mode (what `marimo pair` uses): whether it
        succeeded, and what it printed. It waits in the kernel's queue behind a running cell, so only runs
        and installs go this way. marimo's own run endpoint would refuse us: it lets only the notebook's one
        editor run cells, and the adapter is never the editor."""
        out, ok, event = [], False, None
        async with httpx.AsyncClient(timeout=timeout) as c:
            async with c.stream("POST", self.base + "/api/kernel/execute", headers=self.headers(session_id), json={"code": code}) as r:
                if r.status_code != 200:
                    raise RuntimeError(f"marimo's code mode answered {r.status_code}: {(await r.aread())[:300]!r}")
                async for line in r.aiter_lines():
                    if line.startswith("event: "):
                        event = line[7:]
                    elif line.startswith("data: "):
                        data = json.loads(line[6:])
                        if event in ("stdout", "stderr"):
                            out.append(data.get("data", ""))
                        elif event == "done":
                            ok = bool(data.get("success"))
        return ok, "".join(out)

    async def post(self, path, session_id, body, timeout=30):
        async with httpx.AsyncClient(timeout=timeout) as c:
            r = await c.post(self.base + path, headers=self.headers(session_id), json=body)
        if r.status_code != 200:
            raise RuntimeError(f"marimo's {path} answered {r.status_code}: {r.text[:300]}")
        return r


# ---- Notebooks ---------------------------------------------------------------


def file_form(code):
    """A cell's code as marimo's file gives it back, when that means the same code: a `mo.md("...")` cell is
    written with triple quotes, and quotes, a trailing newline or indentation may change. Our copy and marimo's
    session keep this form, not the agent's, so that they match the file. When they don't, marimo's reload after
    the file changes under it (a package added to its header) pairs cells by similar code, and can give one
    heading's code to another (L1 on Haiku, 11-20). marimo writes a markdown string's value into a plain
    `\"\"\"`, so a backslash in it (LaTeX's `\\alpha`) would become an escape (#433): such a cell takes a raw
    `r\"\"\"`, which marimo keeps, and is kept as sent only when that can't hold its value either. A cell kept
    as sent can still change in the file, since marimo's save writes it its own way (#449)."""
    try:
        kept = _marimo_writes(code)
        if kept == code or _meaning(kept) == _meaning(code):
            return kept
        raw = _raw_markdown(code)
        if raw is not None:
            kept = _marimo_writes(raw)
            if _meaning(kept) == _meaning(code):
                return kept
        return code
    except Exception:  # noqa: BLE001 - code marimo can't write, or that doesn't parse, is kept as it came
        return code


def _marimo_writes(code):
    """A cell's code as marimo writes it to the file and reads it back."""
    from marimo._ast import codegen
    from marimo._ast.cell import CellConfig
    from marimo._ast.parse import parse_notebook
    cells = parse_notebook(codegen.generate_filecontents([code], ["_"], [CellConfig()])).cells
    return cells[0].code if len(cells) == 1 else code


def _raw_markdown(code):
    """A `mo.md("...")` cell whose text has a backslash, written as `mo.md(r\"\"\"...\"\"\")` with the same
    text, or None when a raw string can't hold it (it has `\"\"\"` or ends in a backslash)."""
    tree = ast.parse(code)
    if len(tree.body) != 1 or not isinstance(tree.body[0], ast.Expr):
        return None
    call = tree.body[0].value
    if not (isinstance(call, ast.Call) and ast.unparse(call.func) == "mo.md" and len(call.args) == 1 and not call.keywords):
        return None
    text = call.args[0]
    if not (isinstance(text, ast.Constant) and isinstance(text.value, str)):
        return None
    value = text.value.strip()
    if "\\" not in value or '"""' in value or value.endswith("\\") or "\r" in value:
        return None
    return f'mo.md(r"""\n{value}\n""")'


def _meaning(code):
    """What code does, as Python's syntax tree, but for the spaces and newlines around a string's text,
    which marimo adds to a markdown cell's."""
    import warnings
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        tree = ast.parse(code)
    for node in ast.walk(tree):
        if isinstance(node, ast.Constant) and isinstance(node.value, str):
            node.value = node.value.strip()
    return ast.dump(tree)


def new_cell(code, name=""):
    return {"code": code, "name": name, "status": "idle", "ran_code": None, "stale": False, "output": None,
            "console": [], "errored": False, "started": 0.0, "last_run": 0.0, "runtime": 0.0, "hide_code": False}


class Notebook:
    def __init__(self, adapter, path, allowed):
        self.adapter, self.m = adapter, adapter.marimo
        self.id = str(uuid.uuid4())
        self.path = path
        self.allowed = allowed
        self.live = False
        self.order = []
        self.cells = {}
        self.missing = set()
        # An agent's run's cells (`watch_run`), and the install under way for some of them: a cell that
        # failed to import a package we'll install reads as queued until it runs again.
        self.watched, self.installing = collections.Counter(), None
        # A run's import of a package new to the notebook waits for the user's OK (#355), as a Julia
        # notebook's does (#397): the modules to install, their PyPI names, and the cells that wait.
        # `approve_install` is the OK; a yes holds for those modules while the notebook is open.
        self.asking, self.approved = None, set()
        self.ws = None
        # The watch's own consumer id, which edits are sent as (`go_live`).
        self.watcher = None
        self.closing = False
        self.watch = None
        # Whether safe preview wrote an edit, so the ids it handed out may not be the ones marimo gives the
        # cells when it loads the file (`rekey`).
        self.edited_in_preview = False

    # -- reading the file (safe preview) --

    def load_file(self):
        app = load_app(self.path)
        if app is None:
            raise argument_error("not_a_notebook", f"'{self.path}' isn't a marimo notebook")
        manager = app._cell_manager
        old = self.cells
        self.order, self.cells = [], {}
        for cid in manager.cell_ids():
            data = manager.get_cell_data(cid)
            code = file_form(data.code)
            cell = old.get(cid) or new_cell(code, data.name)
            cell["code"], cell["name"] = code, data.name
            cell["hide_code"] = bool(data.config.hide_code)
            # The rest of the cell's config (disabled, column), for writing an edit back.
            cell["config"] = data.config.asdict()
            self.order.append(cid)
            self.cells[cid] = cell
        self.mtime = os.stat(self.path).st_mtime

    def write_file(self):
        """Write the cells to the file in safe preview, through marimo's own save with no session."""
        from marimo._ast.cell import CellConfig
        from marimo._server.models.models import SaveNotebookRequest
        from marimo._session.notebook.file_manager import AppFileManager

        cells = [self.cells[c] for c in self.order]
        configs = [CellConfig.from_dict({**(c.get("config") or {}), "hide_code": c["hide_code"]}) for c in cells]
        AppFileManager(self.path).save(SaveNotebookRequest(
            cell_ids=list(self.order), codes=[c["code"] for c in cells], names=[c["name"] or "_" for c in cells],
            configs=configs, filename=self.path, persist=True))
        self.mtime = os.stat(self.path).st_mtime
        self.edited_in_preview = True

    def reload_if_changed(self):
        if not self.live and os.stat(self.path).st_mtime != self.mtime:
            self.load_file()

    # -- marimo's session --

    async def session_id(self):
        # Re-keyed when a page takes the session over, so asked each time.
        for sid, s in (await self.m.sessions()).items():
            if real(s.get("path") or s.get("filename") or "") == self.path:
                return sid
        raise key_error("notebook_closed", "marimo has no session for this notebook any more; open it again")

    async def go_live(self):
        """Make marimo's session for the notebook (its sandbox builds now), then watch it."""
        url = f"ws://127.0.0.1:{self.m.port}{BASE_URL}/ws"
        query = urllib.parse.urlencode({"file": self.path, "access_token": self.m.token})
        maker = await websockets.connect(f"{url}?session_id=endeavor-{secrets.token_hex(6)}&{query}", max_size=None)
        try:
            # A sandboxed notebook first builds its environment (startup-progress), then says kernel-ready.
            while True:
                msg = json.loads(await asyncio.wait_for(maker.recv(), 900))
                if msg.get("op") == "kernel-ready":
                    break
        finally:
            await maker.close()
        await self.m.page_token(self.path)
        # The watch is a consumer of its own, not the session's id: marimo sends an edit to every consumer but
        # the one it came from, and an edit sent with the session's id comes from the page that holds it, which
        # then never sees it, and its next save puts back what it had.
        self.watcher = f"endeavor-watch-{secrets.token_hex(6)}"
        self.ws = await websockets.connect(f"{url}?session_id={self.watcher}&kiosk=true&{query}", max_size=None)
        # The watch's own kernel-ready, taken here and not in `follow`, where it would come after `reform` and put
        # back the code as the file had it.
        try:
            while True:
                first = json.loads(await asyncio.wait_for(self.ws.recv(), 30))
                if first.get("op") == "kernel-ready":
                    msg = {**first, "data": {**first["data"], "resumed": msg["data"].get("resumed")}}
                    break
        except asyncio.TimeoutError:
            pass
        # Copies: kernel_ready reuses a cell's dict under the same id, and writes marimo's code into it.
        preview = [(c, dict(self.cells[c])) for c in self.order] if self.edited_in_preview else None
        # A new session's cells come from the file as written, which may not be how marimo's file writes them
        # (a hand-written `mo.md("## A")`). Its first save would then make the file and the session differ, and a
        # change to the file under marimo (a package install) has marimo pair them by similar code, which can give
        # headings each other's code (#442, as #431 for the agent's own cells). So they take the file's form now,
        # before anything runs. A resumed session keeps its code: its cells may have run.
        reformed = self.kernel_ready(msg["data"], reform=not msg["data"].get("resumed"))
        self.live = True
        self.watch = asyncio.create_task(self.follow())
        if preview:
            await self.rekey(preview)
        if reformed:
            await self.reform(reformed)

    async def rekey(self, preview):
        """Give marimo's session the cell ids safe preview handed out.

        marimo numbers a file's cells by position when it loads it, so after a preview edit that added,
        deleted or moved a cell, its ids differ from the ones the agent holds (and the core's staged
        runs name). One transaction puts them back: a cell id only marimo has is deleted, one only the
        preview has is created, one both have takes the preview's code, name and config, and the order
        becomes the preview's. No id is deleted and created again, because marimo drops a create whose
        id the same transaction deleted. Nothing runs, since nothing has. If the file changed since the
        preview wrote it, or marimo refuses, marimo's ids stay and the tools hear the topology changed.
        """
        ids = list(self.order)
        if [c for c, _ in preview] == ids:
            return
        try:
            if [cell["code"] for _, cell in preview] != [self.cells[c]["code"] for c in ids]:
                raise RuntimeError("the file changed on disk since safe preview wrote it")
            ours = {c for c, _ in preview}
            changes = [{"type": "delete-cell", "cellId": c} for c in ids if c not in ours]
            for cid, cell in preview:
                config = {**(cell.get("config") or {}), "hide_code": bool(cell["hide_code"])}
                camel = {"column": config.get("column"), "disabled": bool(config.get("disabled")),
                         "hideCode": config["hide_code"], "expandOutput": bool(config.get("expand_output"))}
                if cid not in self.cells:
                    changes.append({"type": "create-cell", "cellId": cid, "code": cell["code"], "name": cell["name"] or "_",
                                    "config": {"column": camel["column"], "disabled": camel["disabled"], "hide_code": camel["hideCode"], "expand_output": camel["expandOutput"]}})
                    continue
                if self.cells[cid]["code"] != cell["code"]:
                    changes.append({"type": "set-code", "cellId": cid, "code": cell["code"]})
                if (self.cells[cid]["name"] or "_") != (cell["name"] or "_"):
                    changes.append({"type": "set-name", "cellId": cid, "name": cell["name"] or "_"})
                changes.append({"type": "set-config", "cellId": cid, **camel})
            changes.append({"type": "reorder-cells", "cellIds": [c for c, _ in preview]})
            await self.m.post("/api/document/transaction", self.watcher, {"changes": changes})
            for ch in changes:
                self.document_change(ch)
            for cid, cell in preview:
                self.cells[cid]["config"] = cell.get("config")
            await self.adapter.save(self, self.watcher)
        except Exception as e:  # noqa: BLE001 - the notebook is live either way; only the ids differ
            print(f"endeavor marimo: kept marimo's cell ids for {self.path}: {e}", file=sys.stderr)
            self.adapter.notify("topology_changed", notebook_id=self.id)

    async def reform(self, cells):
        """Give marimo's session the code `kernel_ready` kept in file form for `cells`, and save, so the session and
        the file agree. The meaning is the same (`file_form`), and nothing has run."""
        changes = [{"type": "set-code", "cellId": c, "code": self.cells[c]["code"]} for c in cells if c in self.cells]
        try:
            await self.m.post("/api/document/transaction", self.watcher, {"changes": changes})
            await self.adapter.save(self, self.watcher)
        except Exception as e:  # noqa: BLE001 - the notebook is live either way; only the file's form differs
            print(f"endeavor marimo: kept the file's form of {len(changes)} cells of {self.path}: {e}", file=sys.stderr)

    def kernel_ready(self, d, reform=False):
        """Take marimo's cells. With `reform`, each cell's code is kept in the form marimo's file gives it back
        (`file_form`), and the cells whose code that changes are returned, for marimo's session to take too."""
        old = self.cells
        self.order, self.cells = [], {}
        reformed = []
        for i, cid in enumerate(d["cell_ids"]):
            code = file_form(d["codes"][i]) if reform else d["codes"][i]
            if code != d["codes"][i]:
                reformed.append(cid)
            cell = old.get(cid) or new_cell(code, d["names"][i])
            cell["code"], cell["name"] = code, d["names"][i]
            config = (d.get("configs") or [{}] * len(d["cell_ids"]))[i] or {}
            cell["hide_code"] = bool(config.get("hide_code"))
            self.order.append(cid)
            self.cells[cid] = cell
        return reformed

    async def follow(self):
        try:
            async for raw in self.ws:
                try:
                    self.on(json.loads(raw))
                except Exception as e:  # noqa: BLE001 - one bad message mustn't end the watch
                    print(f"endeavor marimo: couldn't read marimo's message: {e}", file=sys.stderr)
        except websockets.ConnectionClosed:
            pass
        if not self.closing:
            # marimo ended the session (its kernel died, or someone shut it down there).
            self.adapter.forget(self)
            self.adapter.notify("notebook_shut_down", notebook_id=self.id)

    def on(self, m):
        op, d = m.get("op"), m.get("data") or {}
        notify = self.adapter.notify
        if op == "kernel-ready":
            self.kernel_ready(d)
            notify("topology_changed", notebook_id=self.id)
        elif op == "notebook-document-transaction":
            for ch in d["transaction"]["changes"]:
                self.document_change(ch)
            notify("cell_state", notebook_id=self.id, cells=[self.cell_state(c) for c in self.order])
            notify("topology_changed", notebook_id=self.id)
            notify("file_saved", notebook_id=self.id)
        elif op == "cell-op" and d.get("cell_id") in self.cells:
            self.cell_op(d)
            notify("cell_state", notebook_id=self.id, cells=[self.cell_state(d["cell_id"])])
        elif op == "completed-run":
            notify("execution_done", notebook_id=self.id)

    def document_change(self, ch):
        t, cid = ch["type"], ch.get("cellId")
        if t == "create-cell" and cid not in self.cells:
            self.cells[cid] = new_cell(ch.get("code", ""), ch.get("name", ""))
            config = dict(ch.get("config") or {})
            self.cells[cid]["hide_code"] = bool(config.get("hide_code"))
            # Keep the rest (disabled, column) for `save`, as set-config does (#425).
            self.cells[cid]["config"] = {k: config[k] for k in ("disabled", "column", "expand_output") if k in config}
            if ch.get("after") in self.order:
                self.order.insert(self.order.index(ch["after"]) + 1, cid)
            elif ch.get("before") in self.order:
                self.order.insert(self.order.index(ch["before"]), cid)
            else:
                self.order.append(cid)
        elif t == "set-code" and cid in self.cells:
            self.cells[cid]["code"] = ch["code"]
        elif t == "set-name" and cid in self.cells:
            self.cells[cid]["name"] = ch.get("name", "")
        elif t == "set-config" and cid in self.cells:
            if ch.get("hideCode") is not None:
                self.cells[cid]["hide_code"] = bool(ch["hideCode"])
            # The rest of the config is the adapter's copy that `save` writes back: a cell disabled or
            # moved to another column on the page stays so after the next save (#425).
            config = dict(self.cells[cid].get("config") or {})
            for key, name in (("disabled", "disabled"), ("column", "column"), ("expandOutput", "expand_output")):
                if key in ch:
                    config[name] = ch[key] if key == "column" else bool(ch[key])
            self.cells[cid]["config"] = config
        elif t == "delete-cell" and cid in self.cells:
            self.order.remove(cid)
            del self.cells[cid]
        elif t == "move-cell" and cid in self.cells:
            self.order.remove(cid)
            if ch.get("after") in self.order:
                self.order.insert(self.order.index(ch["after"]) + 1, cid)
            elif ch.get("before") in self.order:
                self.order.insert(self.order.index(ch["before"]), cid)
            else:
                self.order.append(cid)
        elif t == "reorder-cells":
            ids = [i for i in ch.get("cellIds", []) if i in self.cells]
            self.order = ids + [i for i in self.order if i not in ids]

    def cell_op(self, d):
        c = self.cells[d["cell_id"]]
        status = d.get("status")
        if status:
            if status == "running" and c["status"] != "running":
                # A cell that waits for an OK and runs anyway (from marimo's page) waits no more.
                if self.asking and d["cell_id"] in self.asking["cells"]:
                    self.asking["cells"].remove(d["cell_id"])
                    if not self.asking["cells"]:
                        self.asking = None
                c["started"] = d.get("timestamp") or time.time()
                c["console"] = []
                c["ran_code"] = c["code"]
                c["stale"] = False
                c["awaiting_install"] = False
            elif status == "idle" and c["status"] == "running":
                c["last_run"] = d.get("timestamp") or time.time()
                c["runtime"] = max(0.0, c["last_run"] - c["started"])
            c["status"] = status
            c["queued_by_us"] = False
        if d.get("stale_inputs") is not None:
            c["stale"] = bool(d["stale_inputs"])
        if d.get("output") is not None:
            c["output"] = d["output"]
            c["errored"] = d["output"].get("channel") == "marimo-error"
            # Marked here, as marimo reports the error, so no read sees it between the run and the install.
            if c["errored"] and d["cell_id"] in self.watched and (self.missing_modules([d["cell_id"]]) or self.raised_by_awaiting(d["cell_id"])):
                c["awaiting_install"] = True
        console = d.get("console")
        if console:
            c["console"] += console if isinstance(console, list) else [console]

    def error_data(self, cid):
        """The errors marimo reported for the cell's last output, as a list."""
        out = self.cells.get(cid, {}).get("output") or {}
        return out.get("data") if isinstance(out.get("data"), list) else []

    def raised_by_awaiting(self, cid):
        """Whether the cell failed only because an ancestor did, and that ancestor waits for its package."""
        raised = [e.get("raising_cell") for e in self.error_data(cid)]
        return bool(raised) and all(r in self.cells and self.cells[r].get("awaiting_install") for r in raised)

    def missing_modules(self, cells, tried=False):
        """The modules the cells failed to import, which nobody tried to install yet (with `tried`,
        those too)."""
        found = {}
        for c in cells:
            for e in self.error_data(c):
                m = re.match(r"No module named '([^'.]+)", e.get("msg", ""))
                if e.get("exception_type") == "ModuleNotFoundError" and m and (tried or m.group(1) not in self.missing):
                    found.setdefault(m.group(1), []).append(c)
        return found

    async def install(self, cells, ask_new=False):
        """Cells of an agent's run failed to import a package the notebook's environment lacks: add it
        (marimo puts it in the file's header with its exact version) and run them again, as a Julia
        notebook installs a package on `using`. Each module is tried once. With `ask_new`, a package
        new to the notebook (not in its header, and not approved yet) waits for the user's OK instead:
        PyPI isn't curated, so a typo such as `import pandsa` would install whatever has that name."""
        modules = self.missing_modules(cells)
        if not modules:
            return False
        rerun = self.rerun_for(modules, cells)
        if ask_new:
            have = header_packages(self.path)
            fresh = [m for m in modules if m not in self.approved and canonical(pypi_name(m)) not in have]
            new = sorted({pypi_name(m) for m in fresh})
            if new:
                held = [c for c in (self.asking or {}).get("cells", []) if c in self.cells]
                modules = sorted(set(modules) | set((self.asking or {}).get("modules", [])))
                # A name a letter or two from a widely used package's: likely a typo (#416).
                near = {pypi_name(m): n for m in fresh if (n := near_miss(m))}
                self.asking = {"modules": modules, "packages": new, "near": near, "cells": self.in_order(set(held) | set(rerun))}
                return False
        return await self.install_modules(modules, rerun)

    def rerun_for(self, modules, cells):
        """The importers, and the run's cells that failed only because an importer did: marimo's lazy mode
        wouldn't run those again with the importer, and they'd keep showing its error."""
        rerun = {c for cs in modules.values() for c in cs}
        rerun |= {c for c in cells if c in self.cells and self.cells[c].get("awaiting_install")}
        return self.in_order(rerun)

    def in_order(self, cells):
        order = graph(self)["order"]
        return sorted(cells, key=lambda c: (order.index(c) if c in order else len(order), self.order.index(c)))

    async def install_modules(self, modules, rerun):
        """Install `modules` into the notebook's environment, with `rerun` marked as waiting for them."""
        self.missing |= set(modules)
        # marimo 0.25.1 adds a sandboxed notebook's package with `uv add --script`, which keeps to the
        # header's date (e2e_marimo checks it). Its other way in, `uv pip install`, reads no header and
        # would take today's version, which a header dated before it can't build again; the date is set
        # for that one too.
        date = package_date(self.path)
        code = CODE_MODE + "import os\nfrom marimo._runtime.packages.module_name_to_pypi_name import module_name_to_pypi_name\n" \
            "names = module_name_to_pypi_name()\n" + \
            (f"os.environ['UV_EXCLUDE_NEWER'] = {date!r}\n" if date else "") + \
            "try:\n    async with ctx:\n" + \
            "".join(f"        ctx.packages.add(names.get({m!r}, {m!r}))\n" for m in sorted(modules)) + \
            "finally:\n    os.environ.pop('UV_EXCLUDE_NEWER', None)\n"
        rerun = [c for c in rerun if c in self.cells]
        for c in rerun:
            self.cells[c]["status"], self.cells[c]["queued_by_us"], self.cells[c]["awaiting_install"] = "queued", True, True
        self.installing = {"packages": sorted(modules), "started": time.time()}
        try:
            # Set by e2e_marimo, in a debug build only, so its read lands during the install.
            delay = float(os.environ.get("ENDEAVOR_TEST_MARIMO_INSTALL_DELAY_SECS") or 0)
            if delay:
                await asyncio.sleep(delay)
            ok, out = await self.m.execute(await self.session_id(), code, timeout=1800)
        except Exception as e:  # noqa: BLE001
            ok, out = False, str(e)
        finally:
            self.installing = None
        if not ok:
            print(f"endeavor marimo: couldn't install {sorted(modules)}: {out[-500:]}", file=sys.stderr)
            for c in rerun:
                self.cells[c]["status"], self.cells[c]["awaiting_install"] = "idle", False
            return False
        return rerun

    async def close(self):
        self.closing = True
        if self.live:
            try:
                sid = await self.session_id()
                await self.m.post("/api/home/shutdown_session", sid, {"sessionId": sid})
            except Exception:  # noqa: BLE001 - it's going anyway
                pass
            if self.ws:
                await self.ws.close()

    # -- what the core reads --

    def cell_state(self, cid):
        c = self.cells[cid]
        waiting = self.waits_for_install(cid)
        return {"cell_id": cid, "code": c["code"], "running": c["status"] == "running",
                "queued": c["status"] == "queued" or waiting, "errored": c["errored"] and not waiting}

    def waits_for_install(self, cid):
        """The cell failed to import a package that is being, or is about to be, installed for it, or
        that waits for the user's OK."""
        c = self.cells[cid]
        asked = bool(self.asking) and cid in self.asking["cells"]
        return (bool(c.get("awaiting_install")) or asked) and c["status"] != "running"

    def process_status(self):
        if not self.allowed:
            return "waiting_for_permission"
        return "ready" if self.live else "starting"

    def snapshot_cell(self, cid):
        c = self.cells[cid]
        out = c["output"] or {}
        never = c["ran_code"] is None and c["last_run"] == 0.0
        waiting = self.waits_for_install(cid)
        d = {"cell_id": cid, "code": c["code"], "folded": c["hide_code"],
             "running": c["status"] == "running", "queued": c["status"] == "queued" or waiting,
             "errored": c["errored"] and not waiting, "last_run": c["last_run"], "runtime": c["runtime"] * 1e9,
             "output": "", "hidden": False, "markdown": is_markdown(c["code"]),
             # marimo's lazy mode marks a cell whose inputs changed; a cell edited since its run is
             # stale for the tools too.
             "stale": bool(c["stale"]) or (c["ran_code"] is not None and c["ran_code"] != c["code"]),
             "not_run": never and not c["errored"], "off": False, "notes": [], "inputs": []}
        if waiting:
            pass
        elif c["errored"]:
            d["error"] = error_of(out, c["console"])
            d["output"] = d["error"]["msg"]
        else:
            d["output"] = output_text(out, c["console"])
        return d

    def snapshot(self):
        self.reload_if_changed()
        return {"notebook_id": self.id, "path": self.path, "cell_order": list(self.order),
                "process_status": self.process_status(), "execution_allowed": self.allowed,
                "safe_preview": not self.allowed, "lazy": True,
                "packages": self.install_state(),
                "cells": [self.snapshot_cell(cid) for cid in self.order]}

    def install_state(self):
        """The package install under way, as the tools show a Julia or R one, or None."""
        if self.asking and not self.installing:
            self.asking["cells"] = [c for c in self.asking["cells"] if c in self.cells]
            if not self.asking["cells"]:
                self.asking = None
        if self.asking and not self.installing:
            # As a Julia notebook's new packages ask (#397): the front asks the user with these names.
            packages, near = self.asking["packages"], self.asking.get("near") or {}
            return {"step": "asking", "new": True, "packages": packages, "seconds": 0, "last_line": None,
                    "download": {"bytes": None, "packages": [{"package": p, **({"close_to": near[p]} if p in near else {})} for p in packages]}}
        if not self.installing and not any(self.waits_for_install(c) for c in self.order):
            return None
        started = (self.installing or {}).get("started", time.time())
        waiting = [c for c in self.order if self.waits_for_install(c)]
        # Before the install starts (or after it ends) only the waiting cells' errors name it, tried or not.
        # A cell that waits because its ancestor raised names nothing itself: the ancestor's import error
        # does, and that ancestor may still say it's running (#439).
        raising = [e.get("raising_cell") for c in waiting for e in self.error_data(c)]
        named = self.missing_modules(waiting + [r for r in raising if r in self.cells], tried=True)
        return {"step": "installing", "packages": (self.installing or {}).get("packages") or sorted(named),
                "seconds": int(time.time() - started), "last_line": None}

    def check_cell(self, cid):
        if cid not in self.cells:
            raise key_error("cell_not_found", f"No cell with id '{cid}' in notebook")
        return cid


def is_markdown(code):
    return bool(re.fullmatch(r'\s*mo\.md\(\s*[rf]{0,2}("""|\'\'\'|"|\').*\1\s*\)\s*', code, re.S))


def bundle_of(out):
    data = out.get("data")
    if out.get("mimetype") == "application/vnd.marimo+mimebundle":
        return data if isinstance(data, dict) else json.loads(data)
    if out.get("mimetype"):
        return {out["mimetype"]: data}
    return {}


def html_text(html):
    """HTML output as text, for read_cell: tags dropped, entities kept simple."""
    text = re.sub(r"<(script|style)\b.*?</\1>", "", html, flags=re.S | re.I)
    text = re.sub(r"<br\s*/?>|</(p|div|tr|li|h\d|pre)>", "\n", text, flags=re.I)
    text = re.sub(r"<[^>]+>", "", text)
    for a, b in (("&lt;", "<"), ("&gt;", ">"), ("&quot;", '"'), ("&#x27;", "'"), ("&#39;", "'"), ("&amp;", "&")):
        text = text.replace(a, b)
    return re.sub(r"\n{3,}", "\n\n", text).strip()


def table_text(html):
    """A table (a DataFrame's output) as text: its first page of rows, as marimo's page shows them, and how many rows it has in all."""
    def attr(name):
        found = re.search(name + r"='([^']*)'", html)
        return html_module.unescape(found.group(1)) if found else None
    try:
        rows = json.loads(attr("data-data") or "null")
        rows = json.loads(rows) if isinstance(rows, str) and rows[:1] in ("[", "{") else rows
        total = json.loads(attr("data-total-rows") or "null")
    except ValueError:
        return None
    if not isinstance(rows, list) or not all(isinstance(r, dict) for r in rows):
        shape = f", {total} rows" if isinstance(total, int) else ""
        return f"[a table{shape}; its rows aren't in the output: read them with code, as `df.head(20)` or `print(df)`]"
    columns = list(dict.fromkeys(k for r in rows for k in r if k != "_marimo_row_id"))
    text = lambda v: "" if v is None else str(v)
    grid = [columns] + [[text(r.get(c)) for c in columns] for r in rows]
    widths = [max(len(line[i]) for line in grid) for i in range(len(columns))]
    lines = ["  ".join(cell.rjust(w) for cell, w in zip(line, widths)) for line in grid]
    if isinstance(total, int) and total > len(rows):
        lines.append(f"[the first {len(rows)} of {total} rows]")
    return "\n".join(lines)


def output_text(out, console):
    parts = [x.get("data", "") for x in console if x.get("channel") in ("stdout", "stderr") and isinstance(x.get("data"), str)]
    bundle = bundle_of(out)
    image = next((k for k in ("image/png", "image/svg+xml", "image/jpeg") if k in bundle), None)
    if image:
        parts.append(f"[{image} output, {len(str(bundle[image]))} bytes; call view_cell_output to see it]")
    elif "text/plain" in bundle and bundle["text/plain"]:
        parts.append(str(bundle["text/plain"]))
    elif "application/json" in bundle and bundle["application/json"] not in (None, ""):
        parts.append(json_text(bundle["application/json"]))
    elif "text/markdown" in bundle:
        parts.append(html_text(str(bundle["text/markdown"])))
    elif "text/html" in bundle and bundle["text/html"]:
        html = str(bundle["text/html"])
        table = table_text(html) if "<marimo-table" in html else None
        if table:
            parts.append(table)
        elif "<marimo-ui-element" in html:
            parts.append("[a marimo UI element: " + (re.search(r"<marimo-([a-z-]+)", html.split("<marimo-ui-element", 1)[1]) or ["", "widget"])[1] + "]")
        else:
            parts.append(html_text(html))
    elif any(v not in (None, "") for v in bundle.values()):
        parts.append(f"[{', '.join(sorted(bundle))} output]")
    parts = [p.rstrip("\n") for p in parts if p]
    return cap("\n".join(parts))


def package_date(path):
    """The `exclude-newer` date in a notebook's PEP 723 header, or None."""
    try:
        text = Path(path).read_text()
    except OSError:
        return None
    block = re.search(r"^# /// script\s*$(.*?)^# ///\s*$", text, re.M | re.S)
    found = block and re.search(r'^#\s*exclude-newer\s*=\s*"([^"]+)"', block.group(1), re.M)
    return found.group(1) if found else None


def header_packages(path):
    """The package names a notebook's PEP 723 header lists in `dependencies`, canonical (`canonical`)."""
    try:
        text = Path(path).read_text()
    except OSError:
        return set()
    block = re.search(r"^# /// script\s*$(.*?)^# ///\s*$", text, re.M | re.S)
    if not block:
        return set()
    body = "\n".join(line[1:].lstrip(" ") if line.startswith("#") else line for line in block.group(1).splitlines())
    deps = re.search(r"^dependencies\s*=\s*\[(.*?)\]", body, re.M | re.S)
    names = re.findall(r'["\']\s*([A-Za-z0-9][A-Za-z0-9._-]*)', deps.group(1)) if deps else []
    return {canonical(n) for n in names}


def canonical(name):
    """A PyPI name as PEP 503 compares it."""
    return re.sub(r"[-_.]+", "-", name).lower()


# Import names of widely used packages, for `near_miss` (#416): a new name a letter or two from one of
# these is likely a typo, and PyPI has typosquats of exactly these. Kept small and shipped here, so
# the check needs no network. A name on the list is never flagged.
COMMON_MODULES = frozenset("""
aiohttp altair anndata anthropic anyio apache_beam arrow astropy attr attrs awkward
bcrypt biopython black bokeh boto3 botocore bs4 cachetools catboost certifi cffi chardet
charset_normalizer click cloudpickle colorama cryptography cupy cv2 cycler dash dask
datasets dateutil decorator deltalake diffusers dill distributed django docker docutils
duckdb einops elasticsearch emoji faker fastapi fastparquet filelock fiona flask
fsspec gensim geopandas google graphviz great_tables grpc gspread h5py httpx huggingface_hub
hvplot hypothesis idna imageio imblearn ipykernel ipython ipywidgets isodate jax jaxlib jinja2
jmespath joblib jsonschema jupyter keras kiwisolver langchain librosa lightgbm llvmlite lxml
markdown marimo matplotlib mlflow more_itertools mpmath msgpack multidict mypy narwhals
nbformat networkx nltk numba numexpr numpy oauthlib onnx openai openpyxl optuna orjson
packaging pandas pandera paramiko patsy pendulum pillow PIL platformdirs plotly plotnine
pluggy polars prophet protobuf psutil psycopg psycopg2 pyarrow pydantic pydeck pygments pyjanitor
pymc pymongo pyodbc pyogrio pyparsing pyproj pyspark pytest pytz pyvista pywt pyyaml qrcode
rasterio ray redis regex reportlab requests rich rpy2 ruamel s3fs scanpy scikit-image
scikit-learn scipy seaborn selenium setuptools shap shapely simplejson six skimage sklearn
slack_sdk snowflake soundfile spacy sqlalchemy sqlmodel statsmodels streamlit sympy tabulate
tenacity tensorboard tensorflow termcolor threadpoolctl tifffile tiktoken toml tomli toolz torch
torchaudio torchvision tornado tqdm transformers typer typing_extensions tzdata ujson umap
urllib3 uvicorn vega_datasets virtualenv wandb websockets werkzeug wheel xarray xgboost xlrd
xlsxwriter yaml yarl zarr
""".split())


def near_miss(module):
    """The widely used package `module` may be a typo of, or None. Only a hint, for the question and
    the agent: the user still decides. Kept narrow, since real packages sit close to common ones
    (pysam and pyyaml, nilearn and sklearn, boto and boto3): the same first letter, neither name the
    start of the other, and one edit for a name under 8 letters, two from 8 up; or the same name but
    for case, `-` or `_`."""
    if module in COMMON_MODULES:
        return None
    low = module.lower()
    plain = re.sub(r"[-_.]", "", low)
    best = None
    for c in COMMON_MODULES:
        other = c.lower()
        if plain == re.sub(r"[-_.]", "", other):
            d = 0
        elif low[:1] != other[:1] or low.startswith(other) or other.startswith(low):
            continue
        else:
            d = edits(low, other)
            if d > (1 if len(low) < 8 else 2):
                continue
        if best is None or (d, c) < best:
            best = (d, c)
    return best and best[1]


def edits(a, b):
    """The edit distance between two names, a swap of neighbours counting as one edit."""
    if abs(len(a) - len(b)) > 2:
        return 3
    prev2, prev = None, list(range(len(b) + 1))
    for i in range(1, len(a) + 1):
        cur = [i] + [0] * len(b)
        for j in range(1, len(b) + 1):
            cur[j] = min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (a[i - 1] != b[j - 1]))
            if prev2 is not None and i > 1 and j > 1 and a[i - 1] == b[j - 2] and a[i - 2] == b[j - 1]:
                cur[j] = min(cur[j], prev2[j - 2] + 1)
        prev2, prev = prev, cur
    return prev[len(b)]


def pypi_name(module):
    """The PyPI name marimo installs for an import of `module`: its own mapping, else the module's name."""
    from marimo._runtime.packages.module_name_to_pypi_name import module_name_to_pypi_name
    return module_name_to_pypi_name().get(module, module)


def json_text(data):
    """A list, tuple or dict output as text: marimo's JSON, with its typed leaves (`text/plain+float:2.5`) as plain values."""
    try:
        value = json.loads(data) if isinstance(data, str) else data
    except ValueError:
        return str(data)

    def plain(v):
        if isinstance(v, str):
            m = re.match(r"^[a-z]+/[a-z0-9.+-]+:(.*)$", v, re.S)
            return m.group(1) if m else json.dumps(v)
        if isinstance(v, list):
            return "[" + ", ".join(plain(x) for x in v) + "]"
        if isinstance(v, dict):
            return "{" + ", ".join(f"{json.dumps(k)}: {plain(x)}" for k, x in v.items()) + "}"
        return json.dumps(v)

    return plain(value)


# marimo's error types, by the names Ember's errors use for the same thing.
ERROR_KINDS = {"exception": "runtime", "multiple-defs": "multiple_definitions", "syntax": "parse", "ancestor-stopped": "upstream_error", "ancestor-prevented": "upstream_error"}


def error_of(out, console):
    errors = out.get("data") if isinstance(out.get("data"), list) else []
    e = errors[0] if errors else {}
    kind = e.get("type", "runtime")
    msg = e.get("msg", "")
    if e.get("exception_type"):
        msg = f"{e['exception_type']}: {msg}"
    names = [x.get("name") for x in errors if x.get("name")]
    if kind == "multiple-defs" and not msg:
        msg = f"Another cell also defines {', '.join(f'`{n}`' for n in names)}; only one cell may define a name. Rename it, or build it in one cell"
    elif kind == "cycle" and not msg:
        msg = "These cells depend on each other in a cycle"
    d = {"kind": ERROR_KINDS.get(kind, kind.replace("-", "_")), "msg": msg or "The cell failed."}
    tb = "".join(x.get("data", "") for x in console if x.get("channel") == "stderr" and isinstance(x.get("data"), str))
    if tb:
        d["traceback"] = cap(tb).splitlines()
    if e.get("raising_cell"):
        d["cells"] = [e["raising_cell"]]
    if names:
        d["names"] = names
    return d


# ---- The graph, from the code (no kernel) ------------------------------------


def analyse(code, cid):
    from marimo._ast.compiler import compile_cell
    try:
        impl = compile_cell(code, cell_id=cid)
        return set(impl.defs), set(impl.refs), {d for d, v in impl.variable_data.items() if v and v[-1].kind == "function"}
    except SyntaxError:
        return set(), set(), set()


def graph(nb, edges=False, packages=False):
    nb.reload_if_changed()
    info = {cid: analyse(nb.cells[cid]["code"], cid) for cid in nb.order}
    defined = {}
    for cid in nb.order:
        for name in info[cid][0]:
            defined.setdefault(name, []).append(cid)
    up = {cid: sorted({d for name in info[cid][1] for d in defined.get(name, []) if d != cid}, key=nb.order.index) for cid in nb.order}
    down = {cid: [] for cid in nb.order}
    for cid in nb.order:
        for parent in up[cid]:
            down[parent].append(cid)
    # A name defined in two cells is marimo's error: those cells don't run.
    clash = {c for cells in defined.values() if len(cells) > 1 for c in cells}
    # Run order: topological, page order among equals; cells in a cycle can't run.
    order, done, visiting, blocked = [], set(), set(), set(clash)

    def visit(cid):
        if cid in done:
            return cid not in blocked
        if cid in visiting:
            return False
        visiting.add(cid)
        ok = all([visit(p) for p in up[cid]])
        visiting.discard(cid)
        done.add(cid)
        if not ok or cid in blocked:
            blocked.add(cid)
            return False
        order.append(cid)
        return True

    for cid in nb.order:
        visit(cid)
    cells = []
    for cid in nb.order:
        defs, refs, fns = info[cid]
        d = {"cell_id": cid, "definitions": sorted(defs - fns), "functions": sorted(defs & fns),
             "references": sorted(n for n in refs if n in defined)}
        if edges:
            d["upstream"], d["downstream"] = up[cid], down[cid]
        if packages:
            d["packages"] = imports(nb.cells[cid]["code"])
        cells.append(d)
    return {"notebook_id": nb.id, "cells": cells, "order": order, "errable": [c for c in nb.order if c in blocked]}


def imports(code):
    try:
        tree = ast.parse(code)
    except SyntaxError:
        return []
    found = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            found |= {a.name.split(".")[0] for a in node.names}
        elif isinstance(node, ast.ImportFrom) and node.module and not node.level:
            found.add(node.module.split(".")[0])
    return sorted(found)


CODE_MODE = "import marimo._code_mode as cm\nctx = cm.get_context(skip_staleness_check=True)\n"


# ---- The adapter -------------------------------------------------------------


class Adapter:
    def __init__(self):
        self.marimo = Marimo()
        self.notebooks = {}
        self.seq = 0
        self.notes = []
        self.changed = asyncio.Event()
        self.runs = 0
        # Each run that's going on: set when it's over.
        self.done = {}

    def notify(self, method, **params):
        self.seq += 1
        self.notes.append({"method": method, "params": params, "seq": self.seq})
        del self.notes[:-NOTES_KEPT]
        self.changed.set()

    def forget(self, nb):
        self.notebooks.pop(nb.id, None)

    def get(self, nid):
        nb = self.notebooks.get(nid or "")
        if nb is None:
            raise key_error("notebook_not_found", f"No open notebook with id '{nid}'")
        return nb

    def by_path(self, path):
        return next((nb for nb in self.notebooks.values() if nb.path == path), None)

    async def open(self, path, run):
        path = real(path)
        if not os.path.isfile(path):
            raise argument_error("file_not_found", f"No file at '{path}'")
        if (nb := self.by_path(path)) is not None:
            raise argument_error("notebook_already_open", f"'{path}' is already open as notebook_id {nb.id}; use that id")
        nb = Notebook(self, path, allowed=bool(run))
        nb.load_file()
        self.notebooks[nb.id] = nb
        if nb.allowed:
            await self.start(nb)
        self.notify("notebook_opened", notebook_id=nb.id, path=nb.path)
        return {"notebook_id": nb.id, "path": nb.path, "process_status": nb.process_status(), "lazy": True}

    async def new(self, path, folder):
        if not path:
            folder = folder or os.getcwd()
            path, n = os.path.join(folder, "notebook.py"), 1
            while os.path.exists(path):
                n += 1
                path = os.path.join(folder, f"notebook {n}.py")
        path = os.path.abspath(os.path.expanduser(path))
        if os.path.exists(path):
            raise argument_error("file_exists", f"'{path}' already exists; use open_notebook to load it")
        os.makedirs(os.path.dirname(path), exist_ok=True)
        # The first write of a file no engine has yet: marimo's own empty notebook.
        date = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        Path(path).write_text(NEW_HEADER.format(python=f"{sys.version_info[0]}.{sys.version_info[1]}", version=marimo.__version__, date=date) + NEW_NOTEBOOK)
        r = await self.open(path, True)
        nb = self.notebooks[r["notebook_id"]]
        r["cells"] = [{"cell_id": c, "code": nb.cells[c]["code"]} for c in nb.order]
        return r

    async def start(self, nb):
        try:
            await nb.go_live()
        except Exception as e:
            nb.allowed = False
            raise argument_error("python_failed", f"marimo couldn't start this notebook: {e}") from e

    async def allow(self, nb, run_all, ask_new=False):
        if nb.allowed:
            return {"already_allowed": True, "ran": False, "process_status": nb.process_status()}
        nb.allowed = True
        await self.start(nb)
        result = {"already_allowed": False, "ran": bool(run_all), "process_status": nb.process_status()}
        if run_all:
            result["run"] = await self.start_run(nb, list(nb.order), ask_new=ask_new)
        return result

    async def apply(self, nb, ops):
        if nb.allowed and not nb.live:
            raise argument_error("execution_blocked", "marimo is still starting this notebook (building its environment); try again when it is ready")
        if not nb.live:
            # Safe preview: the edit goes to the file (`write_file`). A change made on disk since is read first.
            nb.reload_if_changed()
        for op in ops:
            if op.get("op") == "set_code" and op.get("expected") is not None:
                cid = nb.check_cell(op["cell_id"])
                if nb.cells[cid]["code"] != op["expected"]:
                    raise argument_error("stale_read", f"Cell {cid} changed since last read; call read_cell again")
        # The code each set or new cell is kept as (`file_form`), for the core to record as the agent's read.
        changes, inserted, order, kept = [], [], list(nb.order), {}
        for op in ops:
            kind = op.get("op", "")
            if kind == "set_code":
                cid = nb.check_cell(op["cell_id"])
                kept[cid] = file_form(str(op.get("code", "")))
                changes.append({"type": "set-code", "cellId": cid, "code": kept[cid]})
            elif kind == "insert":
                cid = new_id(nb, inserted)
                i = max(0, min(int(op.get("index", len(order))), len(order)))
                kept[cid] = file_form(str(op.get("code", "")))
                change = {"type": "create-cell", "cellId": cid, "code": kept[cid], "name": "_",
                          "config": {"column": None, "disabled": False, "hide_code": bool(op.get("folded")), "expand_output": False}}
                if i > 0:
                    change["after"] = order[i - 1]
                elif order:
                    change["before"] = order[0]
                changes.append(change)
                order.insert(i, cid)
                inserted.append(cid)
            elif kind == "delete":
                cid = nb.check_cell(op["cell_id"])
                changes.append({"type": "delete-cell", "cellId": cid})
                order.remove(cid)
            elif kind == "move":
                cid = nb.check_cell(op["cell_id"])
                order.remove(cid)
                i = max(0, min(int(op["index"]), len(order)))
                change = {"type": "move-cell", "cellId": cid}
                if i > 0:
                    change["after"] = order[i - 1]
                elif order:
                    change["before"] = order[0]
                changes.append(change)
                order.insert(i, cid)
            elif kind == "fold":
                # marimo replaces the whole config, and refuses one missing a field: the rest stays as the file had it.
                cid = nb.check_cell(op["cell_id"])
                config = nb.cells[cid].get("config") or {}
                changes.append({"type": "set-config", "cellId": cid, "column": config.get("column"), "disabled": bool(config.get("disabled")),
                                "hideCode": bool(op.get("folded")), "expandOutput": bool(config.get("expand_output"))})
            else:
                raise argument_error("unknown_op", f"Unknown op: '{kind}'")
        if changes and not nb.live:
            for ch in changes:
                nb.document_change(ch)
            nb.write_file()
        elif changes:
            sid = nb.watcher
            await self.marimo.post("/api/document/transaction", sid, {"changes": changes})
            # Our copy follows at once, not when marimo's message comes back: the next read is ours.
            for ch in changes:
                nb.document_change(ch)
            await self.save(nb, sid)
        self.seq += 1
        return {"inserted": inserted, "seq": self.seq, "codes": kept}

    async def save(self, nb, sid):
        cells = [nb.cells[c] for c in nb.order]
        await self.marimo.post("/api/kernel/save", sid, {
            "cellIds": list(nb.order), "codes": [c["code"] for c in cells], "names": [c["name"] or "_" for c in cells],
            # A cell's config as the file had it (disabled, column), where known.
            "configs": [{"column": (c.get("config") or {}).get("column"), "disabled": bool((c.get("config") or {}).get("disabled")),
                         "hide_code": c["hide_code"], "expand_output": bool((c.get("config") or {}).get("expand_output"))} for c in cells],
            "filename": nb.path, "persist": True})

    async def start_run(self, nb, cells, watch=True, ask_new=False):
        sid = await nb.session_id()
        for c in cells:
            # Marked now: a read straight after the call sees the run, not the cell as it was.
            if nb.cells[c]["status"] != "running":
                nb.cells[c]["status"] = "queued"
                nb.cells[c]["queued_by_us"] = True
        code = CODE_MODE + "async with ctx:\n" + "".join(f"    ctx.run_cell({c!r})\n" for c in cells)
        asyncio.create_task(self.background(nb, code, cells))
        return self.watch(nb, cells, ask_new) if watch else None

    def watch(self, nb, cells, ask_new, first=None):
        """A run of `cells` that `run_finished` reports the end of, after `first` where given."""
        self.runs += 1
        run = f"py-run-{self.runs}"
        nb.watched.update(cells)
        self.done[run] = asyncio.Event()
        asyncio.create_task(self.watch_run(nb, cells, run, ask_new, first))
        return run

    async def background(self, nb, code, cells):
        try:
            ok, out = await self.marimo.execute(await nb.session_id(), code)
            if not ok and "Traceback" in out and not any(nb.cells[c]["errored"] for c in cells if c in nb.cells):
                print(f"endeavor marimo: a run failed in code mode: {out[-500:]}", file=sys.stderr)
        except Exception as e:  # noqa: BLE001
            print(f"endeavor marimo: couldn't run {cells}: {e}", file=sys.stderr)
        finally:
            # Cells marked queued that marimo never took (it refused the run) mustn't look queued forever.
            await asyncio.sleep(0.5)
            for c in cells:
                if c in nb.cells and nb.cells[c]["status"] == "queued" and nb.cells[c].get("queued_by_us"):
                    nb.cells[c]["status"] = "idle"

    def unfinished(self, nb, cells):
        return [c for c in cells if c in nb.cells and nb.cells[c]["status"] in ("queued", "running")]

    async def watch_run(self, nb, cells, run, ask_new=False, first=None):
        try:
            if first is not None:
                await first
            await self.watch_cells(nb, cells, ask_new)
        finally:
            nb.watched.subtract(cells)
            for c in cells:
                # A cell another watched run still has keeps its mark until that run ends too.
                if nb.watched[c] <= 0:
                    del nb.watched[c]
                    if c in nb.cells:
                        nb.cells[c]["awaiting_install"] = False
        self.done.pop(run, asyncio.Event()).set()
        self.notify("run_finished", notebook_id=nb.id, run=run, cells=[c for c in cells if c in nb.cells])

    async def watch_cells(self, nb, cells, ask_new=False):
        while True:
            while self.unfinished(nb, cells) and nb.id in self.notebooks:
                await asyncio.sleep(0.2)
            rerun = nb.id in self.notebooks and await nb.install(cells, ask_new)
            if not rerun:
                break
            await self.start_run(nb, rerun, watch=False)
            await asyncio.sleep(0.3)

    async def run(self, nb, cells, wait, timeout, ask_new=False):
        cells = [nb.check_cell(c) for c in cells]
        result = {"accepted": nb.allowed and nb.live, "process_status": nb.process_status()}
        # The cells that wait for an OK run with these, and ask again for what is new then: an edit may
        # have dropped the import, or the user added the package another way.
        if result["accepted"] and nb.asking:
            cells = [c for c in nb.asking["cells"] if c in nb.cells and c not in cells] + cells
            nb.asking = None
        if not result["accepted"] or not cells:
            if result["accepted"] and wait:
                result.update(completed=[], timed_out=[])
            return result
        # marimo runs lazily here (no autorun, so opening a notebook runs nothing), which would leave the
        # cells below a run stale, or showing the error it fixed. Pluto reruns them, and the tools say a run
        # does: the run takes them too, in the graph's order, and the core is told which.
        g = graph(nb, edges=True)
        down = {d["cell_id"]: d["downstream"] for d in g["cells"]}
        dependents, todo = [], list(cells)
        while todo:
            for d in down.get(todo.pop(), []):
                if d not in cells and d not in dependents:
                    dependents.append(d)
                    todo.append(d)
        ran = sorted(cells + dependents, key=lambda c: (g["order"].index(c) if c in g["order"] else len(g["order"]), nb.order.index(c)))
        run = await self.start_run(nb, ran, ask_new=ask_new)
        result["dependents"] = [c for c in ran if c in dependents]
        if not wait:
            result["run"] = run
            return result
        # Over when its watch says so: that covers a package it installs and the cells it runs again.
        done = self.done.get(run)
        if done is not None:
            try:
                await asyncio.wait_for(done.wait(), float(timeout or 60))
            except asyncio.TimeoutError:
                pass
        left = self.unfinished(nb, ran) if done is not None and not done.is_set() else []
        if nb.asking:
            # As a Julia run that asks answers: its cells wait, so none finished.
            left = sorted(set(left) | {c for c in ran if c in nb.asking["cells"]}, key=ran.index)
            result["asking"] = nb.asking["packages"]
        result["process_status"] = nb.process_status()
        result["completed"] = [c for c in cells if c not in left]
        result["timed_out"] = sorted(set(self.unfinished(nb, list(nb.order))) | set(left), key=nb.order.index) if left else []
        if left:
            result["run"] = run
        return result

    async def interrupt(self, nb):
        busy = bool(self.unfinished(nb, list(nb.order))) or bool(nb.asking)
        # Cells that wait for an OK stop waiting: they show their import error again.
        nb.asking = None
        if nb.live:
            await self.marimo.post("/api/kernel/interrupt", await nb.session_id(), {})
        return {"interrupted": busy}

    async def restart(self, nb):
        if not nb.allowed:
            raise argument_error("execution_blocked", "The notebook is in safe preview; Run notebook starts it")
        # marimo's restart closes the session (its page then reloads, which starts a new one); we start it.
        nb.closing = True
        await self.marimo.post("/api/kernel/restart_session", await nb.session_id(), {}, timeout=300)
        if nb.ws:
            await nb.ws.close()
        if nb.watch:
            await nb.watch
        nb.closing, nb.live, nb.asking = False, False, None
        for c in nb.cells.values():
            c.update(status="idle", ran_code=None, stale=False, output=None, console=[], errored=False, last_run=0.0, runtime=0.0, awaiting_install=False)
        await self.start(nb)
        return {"restarted": True}

    async def approve(self, nb, packages=None):
        """The user's yes to the packages a notebook asks about (#355): they install, and the cells that
        waited run, under a run that `run_finished` reports the end of. `packages`, when given, are the
        PyPI names the user was shown: the yes covers only those (#418). A package added since keeps
        asking, and the cells wait for it too; a yes that names none of what waits approves nothing.
        Like a Julia notebook's reply, it says whether the install and run were started (`accepted`)
        and the notebook's `process_status` (#419)."""
        a = nb.asking
        if not a or not nb.live:
            raise argument_error("nothing_to_approve", "Nothing in this notebook waits for the user's OK")
        if packages is None:
            ok = list(a["packages"])
        else:
            want = {canonical(str(p)) for p in packages}
            ok = [p for p in a["packages"] if canonical(p) in want]
        left = [p for p in a["packages"] if p not in ok]
        cells = [c for c in a["cells"] if c in nb.cells]
        if not ok:
            return {"approved": False, "accepted": False, "packages": [], "asking": a["packages"], "cells": cells, "process_status": nb.process_status()}
        nb.approved |= {m for m in a["modules"] if pypi_name(m) not in left}
        if left:
            a["packages"] = left
            return {"approved": True, "accepted": False, "packages": ok, "asking": left, "cells": cells, "process_status": nb.process_status()}
        nb.asking = None
        # Marked now: a read straight after the OK sees them wait for the install, not their import error.
        for c in cells:
            nb.cells[c]["status"], nb.cells[c]["queued_by_us"], nb.cells[c]["awaiting_install"] = "queued", True, True

        async def install_and_run():
            if await nb.install_modules(a["modules"], cells):
                await self.start_run(nb, cells, watch=False)

        run = self.watch(nb, cells, True, install_and_run())
        return {"approved": True, "accepted": True, "packages": ok, "cells": cells, "run": run, "process_status": nb.process_status()}

    async def shutdown(self, nb):
        safe_preview = not nb.allowed
        self.forget(nb)
        await nb.close()
        self.notify("notebook_shut_down", notebook_id=nb.id)
        return {"safe_preview": safe_preview}

    async def move(self, nb, path):
        path = os.path.abspath(os.path.expanduser(path))
        if os.path.exists(path):
            raise argument_error("invalid_path", f"'{path}' already exists")
        if nb.live:
            await self.marimo.post("/api/kernel/rename", await nb.session_id(), {"filename": path})
        else:
            raise argument_error("execution_blocked", "The notebook is in safe preview; it can move once it may run")
        nb.path = real(path)
        return {"path": nb.path}

    def render_png(self, nb, cid):
        out = nb.cells[cid]["output"] or {}
        bundle = bundle_of(out)
        png = bundle.get("image/png")
        if png:
            return {"png": str(png).split(",", 1)[-1].replace("\n", ""), "mime": "image/png"}
        return {"png": None, "mime": next(iter(bundle), out.get("mimetype") or "")}

    def validate(self, nb, code):
        errors = []
        try:
            ast.parse(code)
        except SyntaxError as e:
            errors.append({"type": "syntax_error", "message": f"{e.msg} (line {e.lineno}, column {e.offset})"})
        return {"errors": errors}

    def status(self):
        return {"marimo": marimo.__version__, "marimo_port": self.marimo.port, "seq": self.seq,
                "notebooks": [{"notebook_id": nb.id, "path": nb.path, "cell_count": len(nb.order)} for nb in self.notebooks.values()]}

    async def call(self, method, p):
        if method == "status":
            return self.status()
        if method == "open":
            return await self.open(p.get("path", ""), p.get("run"))
        if method == "new":
            return await self.new(p.get("path"), p.get("folder"))
        if method == "snapshot" and not p.get("notebook_id"):
            seq = self.seq
            return {"notebooks": [nb.snapshot() for nb in list(self.notebooks.values())], "seq": seq}
        if method == "page_bundle":
            raise argument_error("not_supported", "Python notebooks can't show in this notebook view yet")
        nb = self.get(p.get("notebook_id"))
        if method == "snapshot":
            seq = self.seq
            return {**nb.snapshot(), "seq": seq}
        if method == "graph":
            return graph(nb, bool(p.get("edges")), bool(p.get("packages")))
        if method == "apply":
            return await self.apply(nb, p.get("ops") or [])
        if method == "run":
            return await self.run(nb, p.get("cells") or [], bool(p.get("wait")), p.get("timeout"), bool(p.get("ask_new")))
        if method == "allow_execution":
            return await self.allow(nb, bool(p.get("run")), bool(p.get("ask_new")))
        if method == "interrupt":
            return await self.interrupt(nb)
        if method == "restart":
            return await self.restart(nb)
        if method == "shutdown":
            return await self.shutdown(nb)
        if method == "move":
            return await self.move(nb, p.get("path", ""))
        if method == "render_text":
            return {"text": None}
        if method == "render_png":
            return self.render_png(nb, nb.check_cell(p.get("cell_id")))
        if method == "validate":
            nb.check_cell(p.get("cell_id"))
            return self.validate(nb, str(p.get("code") or ""))
        if method == "approve_install":
            packages = p.get("packages")
            return await self.approve(nb, packages if isinstance(packages, list) else None)
        if method in ("answer", "preview_packages", "set_package_date", "set_input"):
            raise argument_error("not_supported", "Python notebooks can't do this yet: it works in Julia and R notebooks")
        raise argument_error("unknown_method", f"Unknown adapter method: '{method}'")


def new_id(nb, taken):
    # marimo's own id form: four letters.
    while True:
        cid = "".join(secrets.choice("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ") for _ in range(4))
        if cid not in nb.cells and cid not in taken:
            return cid


def make_app(adapter):
    def authorized(req):
        return bool(TOKEN) and secrets.compare_digest(req.headers.get("authorization", ""), f"Bearer {TOKEN}")

    async def call(req: Request):
        if not authorized(req):
            return JSONResponse({"error": "unauthorized"}, 401)
        try:
            msg = await req.json()
            return JSONResponse({"result": await adapter.call(str(msg.get("method") or ""), msg.get("params") or {})})
        except AdapterError as e:
            return JSONResponse({"error": str(e)})
        except Exception as e:  # noqa: BLE001 - every failure is an answer
            return JSONResponse({"error": f"python_error::{type(e).__name__}: {e}"})

    async def notifications(req: Request):
        if not authorized(req):
            return JSONResponse({"error": "unauthorized"}, 401)
        after = int(req.query_params.get("after", "0"))
        end = time.time() + POLL_HOLD
        while True:
            found = [n for n in adapter.notes if n["seq"] > after]
            if found or time.time() >= end:
                return JSONResponse(found)
            adapter.changed.clear()
            try:
                await asyncio.wait_for(adapter.changed.wait(), max(0.0, end - time.time()))
            except asyncio.TimeoutError:
                pass

    async def health(req):
        return JSONResponse({"ok": True})

    return Starlette(routes=[Route("/adapter", call, methods=["POST"]), Route("/notifications", notifications), Route("/health", health)])


def write_state(bridge_port, server):
    """Written whole or not at all: the core polls for the file. marimo's token isn't in it: the core made it."""
    if not STATE_FILE:
        return
    tmp = f"{STATE_FILE}.tmp{os.getpid()}"
    Path(tmp).write_text(json.dumps({"pid": os.getpid(), "bridge_port": bridge_port, "marimo_port": server.port, "marimo": marimo.__version__}))
    os.replace(tmp, STATE_FILE)


async def main():
    if not TOKEN:
        sys.exit("ENDEAVOR_TOKEN is not set")
    adapter = Adapter()
    adapter.marimo.start()
    await adapter.marimo.wait_ready()
    port = free_port()
    server = uvicorn.Server(uvicorn.Config(make_app(adapter), host="127.0.0.1", port=port, log_level="warning", lifespan="off"))
    serving = asyncio.create_task(server.serve())
    while not server.started:
        await asyncio.sleep(0.05)
    write_state(port, adapter.marimo)
    loop = asyncio.get_running_loop()
    stop = asyncio.Event()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, stop.set)
    ended = asyncio.create_task(adapter.marimo.child.wait())
    await asyncio.wait([serving, ended, asyncio.create_task(stop.wait())], return_when=asyncio.FIRST_COMPLETED)
    server.should_exit = True
    if adapter.marimo.child.returncode is None:
        adapter.marimo.child.terminate()
        try:
            await asyncio.wait_for(adapter.marimo.child.wait(), 10)
        except asyncio.TimeoutError:
            adapter.marimo.child.kill()
    shutil.rmtree(adapter.marimo.config, ignore_errors=True)
    if ended.done():
        sys.exit(f"marimo's server ended ({adapter.marimo.child.returncode})")


if __name__ == "__main__":
    asyncio.run(main())
