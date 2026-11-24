import asyncio
import base64
import collections
import functools
import ast
import json
import html as html_module
import os
import platform
import re
import secrets
import shutil
import signal
import socket
import subprocess
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
BASE_URL = "/marimo"
POLL_HOLD = 25
NOTES_KEPT = 500
TEXT_MAX = 20_000

MARIMO_ON_LOOPBACK = """
import functools, inspect, os, sys
from marimo._environments import backends
from marimo._server import lsp
from marimo._utils import net

def moved(what):
    sys.exit(f"Endeavor: marimo's {what} moved, so Endeavor can't keep marimo on 127.0.0.1")

if lsp.find_free_port is not net.find_free_port or "addr" not in inspect.signature(net.find_free_port).parameters:
    moved("language-server port search")
if next(iter(inspect.signature(backends.launch_server).parameters), None) != "args":
    moved("server relaunch")
lsp.find_free_port = functools.partial(net.find_free_port, addr="127.0.0.1")
launch_server = backends.launch_server

def relaunch_on_loopback(args, **options):
    if list(args[:2]) != ["-m", "marimo"]:
        moved("server relaunch")
    return launch_server(["-c", os.environ["ENDEAVOR_MARIMO_CLI"], *args[2:]], **options)

backends.launch_server = relaunch_on_loopback
"""

MARIMO_CLI = MARIMO_ON_LOOPBACK + """
from marimo._cli.cli import main
main(prog_name="marimo")
"""

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
[display]
default_table_page_size = 25
theme = "system"
[ai]
enabled = false
"""

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
    ...


def argument_error(kind, text):
    return AdapterError(f"ArgumentError: {kind}::{text}")


def key_error(kind, text):
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


def drive_root(path, splitdrive=os.path.splitdrive, sep=os.sep):
    return splitdrive(path)[0] + sep


ROOT = drive_root(real(os.environ.get("ENDEAVOR_MARIMO_ROOT") or os.sep))


def other_drive(path, root, splitdrive=os.path.splitdrive):
    drive = splitdrive(path)[0]
    return None if drive.lower() == splitdrive(root)[0].lower() else drive


def server_base(index):
    return BASE_URL if index == 0 else f"{BASE_URL}/_{index}"


class Marimo:
    def __init__(self, root, index):
        self.root, self.base_url = root, server_base(index)
        self.port = free_port()
        self.token = os.environ.get("ENDEAVOR_MARIMO_TOKEN") or secrets.token_hex(32)
        self.base = f"http://127.0.0.1:{self.port}{self.base_url}"
        self.config = Path(tempfile.mkdtemp(prefix="endeavor-marimo-"))
        self.server_token = None

    def start(self):
        (self.config / "marimo").mkdir()
        (self.config / "marimo" / "marimo.toml").write_text(MARIMO_CONFIG)
        token_file = self.config / "token"
        token_file.write_text(self.token)
        token_file.chmod(0o600)
        env = {**os.environ, "XDG_CONFIG_HOME": str(self.config), "MARIMO_SKIP_UPDATE_CHECK": "1", "ENDEAVOR_MARIMO_CLI": MARIMO_CLI}
        env.pop("ENDEAVOR_TOKEN", None)
        env.pop("ENDEAVOR_MARIMO_TOKEN", None)
        self.child = asyncio.subprocess.create_subprocess_exec(
            sys.executable, "-c", MARIMO_CLI, "-q", "edit", self.root, "--headless", "--sandbox", "--watch",
            "--skip-update-check", "--host", "127.0.0.1", "--port", str(self.port), "--base-url", self.base_url,
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


def file_form(code):
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
    except Exception:  # noqa: BLE001
        return code


FILE_FORM_NOTE = ("markdown_in_file::marimo writes this markdown cell to the file in another form, so reopening "
                  "the notebook changes it: a backslash at the end of its text, or \"\"\" together with a backslash, "
                  "becomes another escape, and keyword arguments to mo.md are dropped. To keep it as it is, write the "
                  "text so mo.md(r\"\"\"...\"\"\") can hold it (no \"\"\" in it, no backslash at its end) with "
                  "no keyword arguments, or build the text in a variable first and pass that to mo.md.")


@functools.lru_cache(maxsize=1024)
def changes_in_file(code):
    try:
        return _meaning(_marimo_writes(code)) != _meaning(code)
    except Exception:  # noqa: BLE001
        return False


def _marimo_writes(code):
    from marimo._ast import codegen
    from marimo._ast.cell import CellConfig
    from marimo._ast.parse import parse_notebook
    cells = parse_notebook(codegen.generate_filecontents([code], ["_"], [CellConfig()])).cells
    return cells[0].code if len(cells) == 1 else code


def _raw_markdown(code):
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
    import warnings
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        tree = ast.parse(code)
    for node in ast.walk(tree):
        if isinstance(node, ast.Constant) and isinstance(node.value, str):
            node.value = node.value.strip()
    return ast.dump(tree)


def kept_ids(ours, loaded):
    if not ours or not loaded:
        return {}
    from marimo._utils.cell_matching import match_cell_ids_by_similarity
    try:
        return {got: keep for keep, got in match_cell_ids_by_similarity(ours, loaded).items()}
    except Exception as e:  # noqa: BLE001
        print(f"endeavor marimo: couldn't pair the file's cells with ours: {e}", file=sys.stderr)
        return {}


def new_cell(code, name=""):
    return {"code": code, "name": name, "status": "idle", "ran_code": None, "stale": False, "output": None,
            "console": [], "errored": False, "started": 0.0, "last_run": 0.0, "runtime": 0.0, "hide_code": False}


class Notebook:
    def __init__(self, adapter, server, path, allowed):
        self.adapter, self.m = adapter, server
        self.id = str(uuid.uuid4())
        self.path = path
        self.allowed = allowed
        self.live = False
        self.order = []
        self.cells = {}
        self.missing = set()
        self.watched, self.installing = collections.Counter(), None
        self.asking, self.approved = None, set()
        self.failed = None
        self.python_found = None
        self.previewed = None
        self.ws = None
        self.watcher = None
        self.closing = False
        self.watch = None
        self.own_ids = False

    def load_file(self):
        app = load_app(self.path)
        if app is None:
            raise argument_error("not_a_notebook", f"'{self.path}' isn't a marimo notebook")
        manager = app._cell_manager
        old = self.cells
        loaded = [(cid, manager.get_cell_data(cid)) for cid in manager.cell_ids()]
        codes = {cid: file_form(data.code) for cid, data in loaded}
        ids = kept_ids({c: old[c]["code"] for c in self.order}, codes)
        self.order, self.cells = [], {}
        for got, data in loaded:
            cid, code = ids.get(got, got), codes[got]
            cell = old.get(cid) or new_cell(code, data.name)
            cell["code"], cell["name"] = code, data.name
            cell["hide_code"] = bool(data.config.hide_code)
            cell["config"] = data.config.asdict()
            self.order.append(cid)
            self.cells[cid] = cell
        if self.order != [got for got, _ in loaded]:
            self.own_ids = True
        self.mtime = os.stat(self.path).st_mtime

    def write_file(self):
        from marimo._ast.cell import CellConfig
        from marimo._server.models.models import SaveNotebookRequest
        from marimo._session.notebook.file_manager import AppFileManager

        cells = [self.cells[c] for c in self.order]
        configs = [CellConfig.from_dict({**(c.get("config") or {}), "hide_code": c["hide_code"]}) for c in cells]
        AppFileManager(self.path).save(SaveNotebookRequest(
            cell_ids=list(self.order), codes=[c["code"] for c in cells], names=[c["name"] or "_" for c in cells],
            configs=configs, filename=self.path, persist=True))
        self.mtime = os.stat(self.path).st_mtime
        self.own_ids = True

    def reload_if_changed(self):
        if not self.live and os.stat(self.path).st_mtime != self.mtime:
            self.load_file()

    async def session_id(self):
        for sid, s in (await self.m.sessions()).items():
            if real(s.get("path") or s.get("filename") or "") == self.path:
                return sid
        raise key_error("notebook_closed", "marimo has no session for this notebook any more; open it again")

    async def go_live(self):
        await asyncio.to_thread(pin_marimo, self.path)
        url = f"ws://127.0.0.1:{self.m.port}{self.m.base_url}/ws"
        query = urllib.parse.urlencode({"file": self.path, "access_token": self.m.token})
        maker = await websockets.connect(f"{url}?session_id=endeavor-{secrets.token_hex(6)}&{query}", max_size=None)
        try:
            while True:
                msg = json.loads(await asyncio.wait_for(maker.recv(), 900))
                if msg.get("op") == "kernel-ready":
                    break
        finally:
            await maker.close()
        await self.m.page_token(self.path)
        self.watcher = f"endeavor-watch-{secrets.token_hex(6)}"
        self.ws = await websockets.connect(f"{url}?session_id={self.watcher}&kiosk=true&{query}", max_size=None)
        try:
            while True:
                first = json.loads(await asyncio.wait_for(self.ws.recv(), 30))
                if first.get("op") == "kernel-ready":
                    msg = {**first, "data": {**first["data"], "resumed": msg["data"].get("resumed")}}
                    break
        except asyncio.TimeoutError:
            pass
        ours = {c: self.cells[c]["code"] for c in self.order} if self.order and (self.own_ids or not msg["data"].get("resumed")) else None
        reformed = self.kernel_ready(msg["data"], reform=not msg["data"].get("resumed"))
        self.live = True
        self.watch = asyncio.create_task(self.follow())
        if ours:
            ids = await self.rekey(ours)
            reformed = [ids.get(c, c) for c in reformed]
        if reformed:
            await self.reform(reformed)

    async def rekey(self, ours):
        ids = list(self.order)
        mine = kept_ids(ours, {c: self.cells[c]["code"] for c in ids})
        target = [mine.get(c, c) for c in ids]
        if target == ids:
            return {}
        try:
            theirs = {c: dict(self.cells[c]) for c in ids}
            changes = [{"type": "delete-cell", "cellId": c} for c in ids if c not in set(target)]
            for got, cid in zip(ids, target):
                cell = theirs[got]
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
            changes.append({"type": "reorder-cells", "cellIds": target})
            await self.m.post("/api/document/transaction", self.watcher, {"changes": changes})
            for ch in changes:
                self.document_change(ch)
            for got, cid in zip(ids, target):
                self.cells[cid]["config"] = theirs[got].get("config")
            await self.adapter.save(self, self.watcher)
            return dict(zip(ids, target))
        except Exception as e:  # noqa: BLE001
            print(f"endeavor marimo: kept marimo's cell ids for {self.path}: {e}", file=sys.stderr)
            self.adapter.notify("topology_changed", notebook_id=self.id)
            return {}

    async def reform(self, cells):
        changes = [{"type": "set-code", "cellId": c, "code": self.cells[c]["code"]} for c in cells if c in self.cells]
        try:
            await self.m.post("/api/document/transaction", self.watcher, {"changes": changes})
            await self.adapter.save(self, self.watcher)
        except Exception as e:  # noqa: BLE001
            print(f"endeavor marimo: kept the file's form of {len(changes)} cells of {self.path}: {e}", file=sys.stderr)

    def kernel_ready(self, d, reform=False):
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
            if config:
                cell["config"] = {k: config[k] for k in ("disabled", "column", "expand_output") if k in config}
            self.order.append(cid)
            self.cells[cid] = cell
        return reformed

    async def follow(self):
        try:
            async for raw in self.ws:
                try:
                    self.on(json.loads(raw))
                except Exception as e:  # noqa: BLE001
                    print(f"endeavor marimo: couldn't read marimo's message: {e}", file=sys.stderr)
        except websockets.ConnectionClosed:
            pass
        if not self.closing:
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
            if c["errored"] and d["cell_id"] in self.watched and (self.missing_modules([d["cell_id"]]) or self.raised_by_awaiting(d["cell_id"])):
                c["awaiting_install"] = True
        console = d.get("console")
        if console:
            c["console"] += console if isinstance(console, list) else [console]

    def error_data(self, cid):
        out = self.cells.get(cid, {}).get("output") or {}
        return out.get("data") if isinstance(out.get("data"), list) else []

    def raised_by_awaiting(self, cid):
        raised = [e.get("raising_cell") for e in self.error_data(cid)]
        return bool(raised) and all(r in self.cells and self.cells[r].get("awaiting_install") for r in raised)

    def missing_modules(self, cells, tried=False):
        found = {}
        for c in cells:
            for e in self.error_data(c):
                m = re.match(r"No module named '([^'.]+)", e.get("msg", ""))
                if e.get("exception_type") == "ModuleNotFoundError" and m and (tried or m.group(1) not in self.missing):
                    found.setdefault(m.group(1), []).append(c)
        return found

    async def install(self, cells, ask_new=False):
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
                near = {pypi_name(m): n for m in fresh if (n := near_miss(m))}
                self.asking = {"modules": modules, "packages": new, "near": near, "cells": self.in_order(set(held) | set(rerun))}
                return False
        return await self.install_modules(modules, rerun)

    def rerun_for(self, modules, cells):
        rerun = {c for cs in modules.values() for c in cs}
        rerun |= {c for c in cells if c in self.cells and self.cells[c].get("awaiting_install")}
        return self.in_order(rerun)

    def in_order(self, cells):
        order = graph(self)["order"]
        return sorted(cells, key=lambda c: (order.index(c) if c in order else len(order), self.order.index(c)))

    async def install_modules(self, modules, rerun):
        self.missing |= set(modules)
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
        self.installing = {"packages": sorted({pypi_name(m) for m in modules}), "started": time.time()}
        try:
            delay = float(os.environ.get("ENDEAVOR_TEST_MARIMO_INSTALL_DELAY_SECS") or 0)
            if delay:
                await asyncio.sleep(delay)
            ok, out = await self.m.execute(await self.session_id(), code, timeout=1800)
        except Exception as e:  # noqa: BLE001
            ok, out = False, str(e)
        finally:
            self.installing = None
        refused = re.findall(r"^failed to install (\S+)$", out or "", re.M)
        ok = ok and not refused
        self.failed = None if ok else {"packages": sorted(refused or {pypi_name(m) for m in modules}), "log": out[-6000:]}
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
            except Exception:  # noqa: BLE001
                pass
            if self.ws:
                await self.ws.close()

    def cell_state(self, cid):
        c = self.cells[cid]
        waiting = self.waits_for_install(cid)
        return {"cell_id": cid, "code": c["code"], "running": c["status"] == "running",
                "queued": c["status"] == "queued" or waiting, "errored": c["errored"] and not waiting}

    def waits_for_install(self, cid):
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
             "stale": bool(c["stale"]) or (c["ran_code"] is not None and c["ran_code"] != c["code"]),
             "not_run": never and not c["errored"], "off": False, "notes": [], "inputs": []}
        if c["code"].lstrip().startswith("mo.md(") and changes_in_file(c["code"]):
            d["notes"].append(FILE_FORM_NOTE)
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
                "packages": self.install_state(), "package_date": day_of(package_date(self.path)),
                "cells": [self.snapshot_cell(cid) for cid in self.order]}

    async def environment(self):
        header = read_header(self.path) or ""
        wanted = header_python(header)
        if self.python_found is None or self.python_found[0] != wanted:
            self.python_found = (wanted, await asyncio.to_thread(uv_python_version, wanted, os.path.dirname(self.path)))
        return {"python": wanted, "python_version": self.python_found[1], "packages": [direct_package(d) for d in header_deps(header)] if header else [],
                "package_date": day_of(package_date(self.path)), "install": self.install_state(), "failed": self.failed}

    def install_state(self):
        if self.asking and not self.installing:
            self.asking["cells"] = [c for c in self.asking["cells"] if c in self.cells]
            if not self.asking["cells"]:
                self.asking = None
        if self.asking and not self.installing:
            packages, near = self.asking["packages"], self.asking.get("near") or {}
            return {"step": "asking", "new": True, "packages": packages, "seconds": 0, "last_line": None,
                    "download": {"bytes": None, "packages": [{"package": p, **({"close_to": near[p]} if p in near else {})} for p in packages]}}
        if not self.installing and not any(self.waits_for_install(c) for c in self.order):
            return None
        started = (self.installing or {}).get("started", time.time())
        waiting = [c for c in self.order if self.waits_for_install(c)]
        raising = [e.get("raising_cell") for c in waiting for e in self.error_data(c)]
        named = self.missing_modules(waiting + [r for r in raising if r in self.cells], tried=True)
        return {"step": "installing", "packages": (self.installing or {}).get("packages") or sorted({pypi_name(m) for m in named}),
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


INTERACTIVE = re.compile(r"<(script|canvas|input|select|button|textarea|iframe)\b"
                         r"|<marimo-(?!(ui-element|table|tex|json-output|callout-output|accordion|tabs|carousel|sidebar"
                         r"|dict|stat|mermaid|progress|nav-menu)\b)[a-z]", re.I)


def interactive_bundle(bundle):
    if any(mime.startswith(("application/vnd.vega", "application/vnd.plotly")) for mime in bundle):
        return True
    return any(INTERACTIVE.search(str(bundle.get(mime) or "")) for mime in ("text/html", "text/markdown"))


IMG_SRC = re.compile(r"<img\b[^>]*?\bsrc\s*=\s*[\"']([^\"']+)[\"']", re.I)
DATA_PNG = re.compile(r"data:image/png;base64,([A-Za-z0-9+/=]+)$")
VIRTUAL_FILE = re.compile(r"^(?:\./)?@file/(\d+)-[^/]+\.png$", re.I)
EMBEDDED_PNG_MIN = 4096
PNG_MAGIC = b"\x89PNG\r\n\x1a\n"


def embedded_png(html):
    found = []
    for src in IMG_SRC.findall(html):
        data, virtual = DATA_PNG.match(src), VIRTUAL_FILE.match(src)
        if data and data.group(1).startswith("iVBORw0KGgo"):
            found.append((len(data.group(1)) * 3 // 4, data.group(1)))
        elif virtual:
            found.append((int(virtual.group(1)), src))
    size, src = max(found, default=(0, None))
    return src if size >= EMBEDDED_PNG_MIN else None


def html_text(html):
    text = re.sub(r"<(script|style)\b.*?</\1>", "", html, flags=re.S | re.I)
    text = re.sub(r"<br\s*/?>|</(p|div|tr|li|h\d|pre)>", "\n", text, flags=re.I)
    text = re.sub(r"<[^>]+>", "", text)
    for a, b in (("&lt;", "<"), ("&gt;", ">"), ("&quot;", '"'), ("&#x27;", "'"), ("&#39;", "'"), ("&amp;", "&")):
        text = text.replace(a, b)
    return re.sub(r"\n{3,}", "\n\n", text).strip()


def table_text(html):
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
        if embedded_png(html):
            parts.append("[the output holds an image; call view_cell_output to see it]")
    elif any(v not in (None, "") for v in bundle.values()):
        parts.append(f"[{', '.join(sorted(bundle))} output]")
    parts = [p.rstrip("\n") for p in parts if p]
    return cap("\n".join(parts))


def pin_marimo(path):
    from marimo._environments import script_metadata
    try:
        if not script_metadata.should_add_marimo(path):
            return
        script_metadata.ensure_metadata_block(path)
        with script_metadata.materialized_for_edit(path) as target:
            script_metadata.uv(["add", "--script", target.path, f"marimo=={marimo.__version__}"],
                               env=script_metadata.script_command_env(), timeout=30, cwd=target.directory)
    except Exception as e:  # noqa: BLE001
        print(f"endeavor marimo: couldn't pin marimo in {path}'s header: {e}", file=sys.stderr)


def date_stamp(date):
    now = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    if date is None:
        return now
    if not isinstance(date, str) or not re.fullmatch(r"\d{4}-\d{2}-\d{2}", date):
        raise argument_error("invalid_argument", "date must be a day as YYYY-MM-DD")
    try:
        time.strptime(date, "%Y-%m-%d")
    except ValueError:
        raise argument_error("invalid_argument", f"{date} isn't a day") from None
    return min(f"{date}T23:59:59Z", now)


def day_of(stamp):
    found = re.match(r"\d{4}-\d{2}-\d{2}", stamp or "")
    return found.group(0) if found else None


def read_header(path):
    from marimo._environments import script_metadata
    return script_metadata.read_manifest(path)


def header_deps(header):
    import tomlkit
    return [str(d) for d in (tomlkit.parse(header).get("dependencies") or [])]


def uv_python_version(requirement, folder):
    try:
        found = subprocess.run([os.environ.get("UV", "uv"), "python", "find", requirement or platform.python_version()], cwd=folder or None,
                               capture_output=True, text=True, timeout=30)
        if found.returncode != 0 or not found.stdout.strip():
            return None
        version = subprocess.run([found.stdout.strip(), "-c", "import platform; print(platform.python_version())"],
                                 capture_output=True, text=True, timeout=30)
        return version.stdout.strip() or None
    except (OSError, subprocess.SubprocessError):
        return None


def header_python(header):
    if not header:
        return None
    import tomlkit
    return str(tomlkit.parse(header).get("requires-python") or "") or None


def direct_package(dep):
    pinned = exact_pin(dep)
    if pinned:
        return {"name": pinned[0].name, "version": pinned[1]}
    from packaging.requirements import Requirement
    try:
        req = Requirement(dep)
    except Exception:  # noqa: BLE001
        return {"name": dep, "version": None}
    return {"name": req.name, "version": str(req.specifier) or None}


def is_marimo(dep):
    from packaging.requirements import Requirement
    try:
        return canonical(Requirement(dep).name) == "marimo"
    except Exception:  # noqa: BLE001
        return False


def exact_pin(dep):
    from packaging.requirements import Requirement
    try:
        req = Requirement(dep)
    except Exception:  # noqa: BLE001
        return None
    specs = list(req.specifier)
    if len(specs) == 1 and specs[0].operator == "==" and "*" not in specs[0].version:
        return req, specs[0].version
    return None


def with_version(req, version):
    extras = f"[{','.join(sorted(req.extras))}]" if req.extras else ""
    marker = f"; {req.marker}" if req.marker else ""
    return f"{req.name}{extras}{'==' + version if version else ''}{marker}"


def with_deps_and_date(header, deps, stamp):
    import tomlkit
    doc = tomlkit.parse(header)
    if "dependencies" in doc:
        array = doc["dependencies"]
        for i, d in enumerate(deps):
            if str(array[i]) != d:
                array[i] = d
    else:
        array = tomlkit.array()
        array.multiline(True)
        for d in deps:
            array.append(d)
        doc["dependencies"] = array
    tool = doc.setdefault("tool", tomlkit.table())
    uv = tool.setdefault("uv", tomlkit.table())
    uv["exclude-newer"] = stamp
    return tomlkit.dumps(doc)


def let_go(header, stamp):
    deps = [d if is_marimo(d) or not exact_pin(d) else with_version(exact_pin(d)[0], None) for d in header_deps(header)]
    return with_deps_and_date(header, deps, stamp)


def pin_at(header, stamp, versions):
    def pinned(d):
        found = None if is_marimo(d) else exact_pin(d)
        if not found:
            return d
        req, was = found
        return with_version(req, versions.get(canonical(req.name), was))
    return with_deps_and_date(header, [pinned(d) for d in header_deps(header)], stamp)


def write_header(path, header, previous):
    from marimo._environments import script_metadata
    with script_metadata.notebook_file_lock(path):
        if script_metadata.read_manifest(path) != previous:
            return False
        with open(path, encoding="utf-8", newline="") as f:
            text = f.read()
        newline = "\r\n" if "\r\n" in text else "\n"
        lines = ["# /// script", *("#" if not line.strip() else f"# {line}" for line in header.rstrip().split("\n")), "# ///"]
        block = newline.join(lines)
        if script_metadata.loads(text) is not None:
            text = script_metadata.replace_block(text, block)
        elif text.startswith("#!"):
            shebang, _, rest = text.partition("\n")
            text = f"{shebang}\n{block}{newline}{rest}"
        else:
            text = f"{block}{newline}{text}"
        with open(path, "w", encoding="utf-8", newline="") as f:
            f.write(text)
    return True


def resolve(path, header):
    from marimo._environments import script_metadata
    text = Path(path).read_text(encoding="utf-8")
    block = script_metadata.wrap_block(header) if header.strip() else script_metadata.wrap_block("dependencies = []")
    script = script_metadata.replace_block(text, block) if script_metadata.loads(text) is not None else block + "\n" + text
    with tempfile.TemporaryDirectory(prefix="endeavor-packages-") as folder:
        copy = Path(folder) / Path(path).name
        copy.write_text(script, encoding="utf-8")
        try:
            out = script_metadata.uv(["export", "--script", str(copy), "--no-hashes", "--no-header", "--no-annotate", "--no-build", "--quiet"],
                                     env=script_metadata.script_command_env(), timeout=120, cwd=folder).stdout
        except Exception as e:  # noqa: BLE001
            why = (getattr(e, "stderr", None) or str(e)).strip()
            return None, "\n".join(line for line in why.splitlines() if line.strip())[-1500:]
    versions = {}
    for line in out.splitlines():
        found = re.match(r"\s*([A-Za-z0-9][A-Za-z0-9._-]*)(?:\[[^\]]*\])?==([^\s;]+)", line)
        if found:
            versions[canonical(found.group(1))] = found.group(2)
    return versions, None


def version_changes(old, new):
    from packaging.version import Version
    rows = []
    for name in sorted(set(old) | set(new)):
        was, now = old.get(name), new.get(name)
        if was == now:
            continue
        if was is None:
            change = "added"
        elif now is None:
            change = "removed"
        else:
            try:
                change = "upgraded" if Version(now) > Version(was) else "downgraded"
            except Exception:  # noqa: BLE001
                change = "changed"
        rows.append({"name": name, "from": was, "to": now, "change": change})
    return rows


def package_date(path):
    try:
        text = Path(path).read_text()
    except OSError:
        return None
    block = re.search(r"^# /// script\s*$(.*?)^# ///\s*$", text, re.M | re.S)
    found = block and re.search(r'^#\s*exclude-newer\s*=\s*"([^"]+)"', block.group(1), re.M)
    return found.group(1) if found else None


def header_packages(path):
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
    return re.sub(r"[-_.]+", "-", name).lower()


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
    from marimo._runtime.packages.module_name_to_pypi_name import module_name_to_pypi_name
    return module_name_to_pypi_name().get(module, module)


def json_text(data):
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
    clash = {c for cells in defined.values() if len(cells) > 1 for c in cells}
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


SET_INPUT = CODE_MODE + """import json as _json
import marimo as _mo
from marimo._plugins.ui._core.ui_element import UIElement as _UIElement


def _endeavor_ui_value(el, value):
    ui = _mo.ui
    number = lambda v: isinstance(v, (int, float)) and not isinstance(v, bool)

    def within(v):
        if not number(v):
            raise ValueError(f"It takes a number, not {_json.dumps(v)}")
        low, high = getattr(el, "start", None), getattr(el, "stop", None)
        if (low is not None and v < low) or (high is not None and v > high):
            raise ValueError(f"It allows {low} to {high}")
        step = getattr(el, "step", None)
        if step and low is not None and abs((v - low) / step - round((v - low) / step)) > 1e-9:
            raise ValueError(f"It allows {low} to {high} in steps of {step}")
        return v

    def stepped(v):
        steps = getattr(el, "steps", None)
        if steps is None:
            return within(v)
        if v not in steps:
            raise ValueError("It allows one of " + ", ".join(map(str, steps)))
        return list(steps).index(v)

    def choice(v):
        if v not in el.options:
            raise ValueError("It allows one of " + ", ".join(_json.dumps(o) for o in list(el.options)[:30]))
        return v

    if isinstance(el, ui.slider):
        return stepped(value)
    if isinstance(el, ui.range_slider):
        if not isinstance(value, list) or len(value) != 2:
            raise ValueError("It takes two numbers, the low end and the high end, as [2, 6]")
        low, high = (stepped(v) for v in value)
        if low > high:
            raise ValueError("The low end comes first")
        return [low, high]
    if isinstance(el, ui.number):
        return within(value)
    if isinstance(el, (ui.checkbox, ui.switch)):
        if not isinstance(value, bool):
            raise ValueError("It takes true or false")
        return value
    if isinstance(el, (ui.text, ui.text_area)):
        if not isinstance(value, str):
            raise ValueError("It takes a string")
        return value
    if isinstance(el, ui.radio):
        return choice(value)
    if isinstance(el, ui.dropdown):
        return [choice(value)]
    if isinstance(el, ui.multiselect):
        return [choice(v) for v in (value if isinstance(value, list) else [value])]
    return None


async def _endeavor_set_input(name, value):
    el = ctx.globals.get(name)
    if not isinstance(el, _UIElement):
        return {"error": "input_refused", "message": f"`{name}` isn't a widget: it is a {type(el).__name__}, not a `mo.ui` control"}
    try:
        sent = _endeavor_ui_value(el, value)
    except ValueError as e:
        return {"error": "invalid_value", "message": f"`{name}` is a {type(el).__name__}. {e}. Nothing changed"}
    if sent is None:
        return {"error": "input_refused", "message": f"`{name}` is a {type(el).__name__}, which only the user sets, on the page. set_input sets a slider, range_slider, number, checkbox, switch, text, text_area, dropdown, radio or multiselect"}
    async with ctx:
        ctx.set_ui_value(el, sent)
    return {"set": True}
"""

SET_INPUT_MARK = "ENDEAVOR-SET-INPUT "


class Adapter:
    def __init__(self):
        self.servers = [Marimo(ROOT, 0)]
        self.starting = asyncio.Lock()
        self.pending = None
        self.server_ended = asyncio.Event()
        self.bridge_port = None
        self.notebooks = {}
        self.seq = 0
        self.notes = []
        self.changed = asyncio.Event()
        self.runs = 0
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

    def serving(self, path):
        return next((s for s in self.servers if other_drive(path, s.root) is None), None)

    async def server_for(self, path):
        async with self.starting:
            if (server := self.serving(path)) is not None:
                return server
            server = self.pending = Marimo(drive_root(path), len(self.servers))
            try:
                server.start()
                await server.wait_ready()
            except Exception as e:
                await stop_server(server)
                raise argument_error("python_failed", f"marimo couldn't start for {server.root}: {e}") from e
            finally:
                self.pending = None
            self.servers.append(server)
            asyncio.create_task(self.watch_server(server))
            write_state(self.bridge_port, self.servers)
            return server

    async def watch_server(self, server):
        await server.child.wait()
        self.server_ended.set()

    async def open(self, path, run):
        path = real(path)
        if not os.path.isfile(path):
            raise argument_error("file_not_found", f"No file at '{path}'")
        if (nb := self.by_path(path)) is not None:
            raise argument_error("notebook_already_open", f"'{path}' is already open as notebook_id {nb.id}; use that id")
        nb = Notebook(self, await self.server_for(path), path, allowed=bool(run))
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
        await self.server_for(real(path))
        os.makedirs(os.path.dirname(path), exist_ok=True)
        date = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        Path(path).write_text(NEW_HEADER.format(python=f"{sys.version_info[0]}.{sys.version_info[1]}", version=marimo.__version__, date=date) + NEW_NOTEBOOK)
        try:
            r = await self.open(path, True)
        except BaseException:
            if (made := self.by_path(real(path))) is not None:
                await made.close()
                self.forget(made)
            Path(path).unlink(missing_ok=True)
            raise
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
            nb.reload_if_changed()
        for op in ops:
            if op.get("op") in ("set_code", "delete") and op.get("expected") is not None:
                cid = nb.check_cell(op["cell_id"])
                if nb.cells[cid]["code"] != op["expected"]:
                    raise argument_error("stale_read", f"Cell {cid} changed since last read; call read_cell again")
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
            await nb.m.post("/api/document/transaction", sid, {"changes": changes})
            deleted = [ch["cellId"] for ch in changes if ch["type"] == "delete-cell"]
            if deleted:
                if any(nb.cells[c]["ran_code"] is not None for c in deleted):
                    down = {d["cell_id"]: d["downstream"] for d in graph(nb, edges=True)["cells"]}
                    todo, seen = list(deleted), set(deleted)
                    while todo:
                        for r in down.get(todo.pop(), []):
                            if r not in seen:
                                seen.add(r)
                                todo.append(r)
                                if nb.cells[r]["ran_code"] is not None:
                                    nb.cells[r]["stale"] = True
                asyncio.create_task(self.unregister(nb, deleted))
            for ch in changes:
                nb.document_change(ch)
            await self.save(nb, sid)
        self.seq += 1
        return {"inserted": inserted, "seq": self.seq, "codes": kept}

    async def unregister(self, nb, cells):
        code = CODE_MODE + "from marimo._runtime.commands import DeleteCellCommand\n" + \
            "".join(f"await ctx._kernel.delete_cell(DeleteCellCommand(cell_id={c!r}))\n" for c in cells)
        try:
            ok, out = await nb.m.execute(await nb.session_id(), code)
        except Exception as e:  # noqa: BLE001
            ok, out = False, str(e)
        if not ok:
            print(f"endeavor marimo: couldn't take {cells} out of the kernel: {out[-500:]}", file=sys.stderr)

    async def save(self, nb, sid):
        cells = [nb.cells[c] for c in nb.order]
        await nb.m.post("/api/kernel/save", sid, {
            "cellIds": list(nb.order), "codes": [c["code"] for c in cells], "names": [c["name"] or "_" for c in cells],
            "configs": [{"column": (c.get("config") or {}).get("column"), "disabled": bool((c.get("config") or {}).get("disabled")),
                         "hide_code": c["hide_code"], "expand_output": bool((c.get("config") or {}).get("expand_output"))} for c in cells],
            "filename": nb.path, "persist": True})

    async def start_run(self, nb, cells, watch=True, ask_new=False):
        sid = await nb.session_id()
        for c in cells:
            if nb.cells[c]["status"] != "running":
                nb.cells[c]["status"] = "queued"
                nb.cells[c]["queued_by_us"] = True
        code = CODE_MODE + "async with ctx:\n" + "".join(f"    ctx.run_cell({c!r})\n" for c in cells)
        asyncio.create_task(self.background(nb, code, cells))
        return self.watch(nb, cells, ask_new) if watch else None

    def watch(self, nb, cells, ask_new, first=None):
        self.runs += 1
        run = f"py-run-{self.runs}"
        nb.watched.update(cells)
        self.done[run] = asyncio.Event()
        asyncio.create_task(self.watch_run(nb, cells, run, ask_new, first))
        return run

    async def background(self, nb, code, cells):
        try:
            ok, out = await nb.m.execute(await nb.session_id(), code)
            if not ok and "Traceback" in out and not any(nb.cells[c]["errored"] for c in cells if c in nb.cells):
                print(f"endeavor marimo: a run failed in code mode: {out[-500:]}", file=sys.stderr)
        except Exception as e:  # noqa: BLE001
            print(f"endeavor marimo: couldn't run {cells}: {e}", file=sys.stderr)
        finally:
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

    async def set_input(self, nb, name, value, wait, timeout):
        if not isinstance(name, str) or not name.isidentifier():
            raise argument_error("invalid_argument", "name must be the name the widget is assigned to")
        if not nb.allowed:
            raise argument_error("execution_blocked", "The notebook is in safe preview, so its widgets can't be set until it may run (allow_execution)")
        g = graph(nb)
        cells = {c["cell_id"]: c for c in g["cells"]}
        defining = [c for c in nb.order if name in cells[c]["definitions"]]
        if len(defining) != 1:
            raise argument_error("input_refused", f"No cell assigns `{name}`" if not defining else f"`{name}` is assigned in more than one cell: {', '.join(defining)}")
        cell = defining[0]
        c = nb.cells[cell]
        if c["status"] in ("queued", "running"):
            raise argument_error("input_refused", f"Cell {cell}, which makes `{name}`, is {c['status']}; set it once read_cell says the cell has finished")
        if not nb.live:
            raise argument_error("input_refused", "The notebook is still starting; set the widget once its cells have run")
        unready = "errored; fix it and run it first" if c["errored"] else "hasn't run; run it first (execute_cell)" if c["ran_code"] is None \
            else "hasn't run since it was edited; run it first" if c["ran_code"] != c["code"] else None
        if unready:
            raise argument_error("input_refused", f"Cell {cell}, which makes `{name}`, {unready}")
        code = SET_INPUT + f"print({SET_INPUT_MARK!r} + _json.dumps(await _endeavor_set_input({name!r}, _json.loads({json.dumps(value)!r}))))\n"
        try:
            ok, out = await nb.m.execute(await nb.session_id(), code)
        except Exception as e:  # noqa: BLE001
            ok, out = False, str(e)
        said = next((json.loads(line[len(SET_INPUT_MARK):]) for line in out.splitlines() if line.startswith(SET_INPUT_MARK)), None)
        if not ok or said is None:
            raise argument_error("input_refused", f"marimo didn't set `{name}`; its value is unchanged: {out[-300:].strip()}")
        if said.get("error"):
            raise argument_error(said["error"], said["message"])
        readers = [r for r in nb.order if r != cell and name in cells[r]["references"]]
        result = await self.run(nb, readers, wait, timeout) if readers else {"accepted": True, "process_status": nb.process_status()}
        result.update(cell=cell, value=value, cells=readers, lazy=False)
        return result

    async def run(self, nb, cells, wait, timeout, ask_new=False):
        cells = [nb.check_cell(c) for c in cells]
        result = {"accepted": nb.allowed and nb.live, "process_status": nb.process_status()}
        if result["accepted"] and nb.asking:
            cells = [c for c in nb.asking["cells"] if c in nb.cells and c not in cells] + cells
            nb.asking = None
        if not result["accepted"] or not cells:
            if result["accepted"] and wait:
                result.update(completed=[], timed_out=[])
            return result
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
        done = self.done.get(run)
        if done is not None:
            try:
                await asyncio.wait_for(done.wait(), float(timeout or 60))
            except asyncio.TimeoutError:
                pass
        left = self.unfinished(nb, ran) if done is not None and not done.is_set() else []
        if nb.asking:
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
        nb.asking = None
        if nb.live:
            await nb.m.post("/api/kernel/interrupt", await nb.session_id(), {})
        return {"interrupted": busy}

    async def restart(self, nb):
        if not nb.allowed:
            raise argument_error("execution_blocked", "The notebook is in safe preview; Run notebook starts it")
        nb.closing = True
        await nb.m.post("/api/kernel/restart_session", await nb.session_id(), {}, timeout=300)
        if nb.ws:
            await nb.ws.close()
        if nb.watch:
            await nb.watch
        nb.closing, nb.live, nb.asking = False, False, None
        for c in nb.cells.values():
            c.update(status="idle", ran_code=None, stale=False, output=None, console=[], errored=False, last_run=0.0, runtime=0.0, awaiting_install=False)
        await self.start(nb)
        return {"restarted": True}

    async def preview_packages(self, nb, date, package=None, version=None):
        if package is not None or version is not None:
            raise argument_error("not_supported", "A Python notebook's packages move together with its date: give `date`, or leave it out for today's newest. Exact versions are in the header")
        stamp = date_stamp(date)
        before = read_header(nb.path)
        after = let_go(before, stamp)
        old, problem = await asyncio.to_thread(resolve, nb.path, before)
        new = None
        if old is None:
            problem = f"The notebook's header doesn't resolve as it is, so there is nothing to compare with: fix the header first (a version that doesn't exist, or one only buildable from source). uv said: {problem}"
        else:
            new, problem = await asyncio.to_thread(resolve, nb.path, after)
        if problem and f"no version of marimo=={marimo.__version__}" in " ".join(problem.split()):
            problem = f"marimo {marimo.__version__}, which runs the notebook, was published after this date, so it can't be the package date: pick a later day. uv said: {problem}"
        changes = [] if new is None else version_changes(old, new)
        nb.previewed = None if new is None else {"day": day_of(stamp), "stamp": stamp, "header": before, "changes": changes}
        return {"date": day_of(stamp), "asked": day_of(stamp), "current_date": day_of(package_date(nb.path)),
                "status": "failed" if new is None else "ready", "changes": changes,
                "problems": [] if problem is None else [{"message": problem}], "message": None,
                "packages": len([d for d in header_deps(before) if not is_marimo(d)])}

    async def apply_packages(self, nb, date):
        shown = nb.previewed
        day = day_of(date_stamp(date))
        if not shown or shown["day"] != day:
            raise argument_error("no_preview", "there is no ready preview for that date. Call preview_package_update first and tell the user what it shows")
        again = "the notebook's packages changed since the preview, so the update would change more or less than it showed. Call preview_package_update again and tell the user what it shows"
        before = read_header(nb.path)
        old = new = None
        if before == shown["header"]:
            old, _ = await asyncio.to_thread(resolve, nb.path, before)
        if old is not None:
            new, _ = await asyncio.to_thread(resolve, nb.path, let_go(before, shown["stamp"]))
        nb.previewed = None
        if new is None or version_changes(old, new) != shown["changes"]:
            raise argument_error("no_preview", again)
        if not write_header(nb.path, pin_at(before, shown["stamp"], new), before):
            raise argument_error("no_preview", again)
        nb.mtime = os.stat(nb.path).st_mtime
        if nb.live:
            await self.restart(nb)
        return {"date": day, "changes": shown["changes"], "process_status": nb.process_status()}

    async def approve(self, nb, packages=None):
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

    async def export(self, nb, form):
        if form == "file":
            with open(nb.path, encoding="utf-8") as f:
                return {"text": f.read()}
        if form != "html":
            raise argument_error("invalid_format", f"Python notebooks export as 'file' or 'html', not '{form}'")
        if not nb.live:
            raise argument_error("execution_blocked", "The notebook is in safe preview; it can export as a web page once it may run")
        r = await nb.m.post("/api/export/html", await nb.session_id(), {"download": False, "files": [], "includeCode": True}, timeout=120)
        return {"text": r.text}

    async def move(self, nb, path):
        path = os.path.abspath(os.path.expanduser(path))
        if os.path.exists(path):
            raise argument_error("invalid_path", f"'{path}' already exists")
        if (drive := other_drive(real(path), nb.m.root)) is not None:
            raise argument_error("unsupported", f"'{path}' is on {drive}, and an open Python notebook can't move to another drive. Move it on its own drive, or tell the user to close it and move the file themselves.")
        if nb.live:
            await nb.m.post("/api/kernel/rename", await nb.session_id(), {"filename": path})
        else:
            raise argument_error("execution_blocked", "The notebook is in safe preview; it can move once it may run")
        nb.path = real(path)
        return {"path": nb.path}

    async def virtual_png(self, server, src):
        if not src or not VIRTUAL_FILE.match(src):
            return src
        try:
            async with httpx.AsyncClient(timeout=10) as c:
                r = await c.get(server.base + "/" + src.removeprefix("./"), headers=server.headers())
        except httpx.HTTPError:
            return None
        return base64.b64encode(r.content).decode() if r.status_code == 200 and r.content.startswith(PNG_MAGIC) else None

    async def render_png(self, nb, cid):
        out = nb.cells[cid]["output"] or {}
        bundle = bundle_of(out)
        png = bundle.get("image/png")
        embedded = None if png else await self.virtual_png(nb.m, embedded_png(str(bundle.get("text/html") or "")))
        if png:
            rendered = {"png": str(png).split(",", 1)[-1].replace("\n", ""), "mime": "image/png"}
        elif embedded:
            rendered = {"png": embedded, "mime": "text/html", "source": "embedded image"}
        else:
            rendered = {"png": None, "mime": next(iter(bundle), out.get("mimetype") or "")}
        if interactive_bundle(bundle):
            rendered["interactive"] = True
        return rendered

    def validate(self, nb, code):
        errors = []
        try:
            ast.parse(code)
        except SyntaxError as e:
            errors.append({"type": "syntax_error", "message": f"{e.msg} (line {e.lineno}, column {e.offset})"})
        return {"errors": errors}

    def status(self):
        return {"marimo": marimo.__version__, "marimo_port": self.servers[0].port, "seq": self.seq,
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
        if method == "environment":
            return await nb.environment()
        if method == "export":
            return await self.export(nb, p.get("format"))
        if method == "render_text":
            return {"text": None}
        if method == "render_png":
            return await self.render_png(nb, nb.check_cell(p.get("cell_id")))
        if method == "validate":
            nb.check_cell(p.get("cell_id"))
            return self.validate(nb, str(p.get("code") or ""))
        if method == "approve_install":
            packages = p.get("packages")
            return await self.approve(nb, packages if isinstance(packages, list) else None)
        if method == "preview_packages":
            return await self.preview_packages(nb, p.get("date"), p.get("package"), p.get("version"))
        if method == "set_package_date":
            return await self.apply_packages(nb, p.get("date"))
        if method == "set_input":
            return await self.set_input(nb, p.get("name"), p.get("value"), bool(p.get("wait")), p.get("timeout"))
        if method == "answer":
            raise argument_error("not_supported", "Python notebooks can't do this yet: it works in Julia and R notebooks")
        raise argument_error("unknown_method", f"Unknown adapter method: '{method}'")


def new_id(nb, taken):
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
        except Exception as e:  # noqa: BLE001
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


def write_state(bridge_port, servers):
    if not STATE_FILE:
        return
    tmp = f"{STATE_FILE}.tmp{os.getpid()}"
    Path(tmp).write_text(json.dumps({"pid": os.getpid(), "bridge_port": bridge_port, "marimo_port": servers[0].port, "marimo": marimo.__version__,
                                     "servers": [{"root": s.root, "base": s.base_url, "port": s.port} for s in servers]}))
    os.replace(tmp, STATE_FILE)


def process_of(server):
    return server.child if isinstance(getattr(server, "child", None), asyncio.subprocess.Process) else None


async def stop_server(server):
    child = process_of(server)
    if child is not None and child.returncode is None:
        child.terminate()
        try:
            await asyncio.wait_for(child.wait(), 10)
        except asyncio.TimeoutError:
            child.kill()
    shutil.rmtree(server.config, ignore_errors=True)


async def main():
    if not TOKEN:
        sys.exit("ENDEAVOR_TOKEN is not set")
    adapter = Adapter()
    first = adapter.servers[0]
    first.start()
    await first.wait_ready()
    asyncio.create_task(adapter.watch_server(first))
    adapter.bridge_port = free_port()
    server = uvicorn.Server(uvicorn.Config(make_app(adapter), host="127.0.0.1", port=adapter.bridge_port, log_level="warning", lifespan="off"))
    serving = asyncio.create_task(server.serve())
    while not server.started:
        await asyncio.sleep(0.05)
    write_state(adapter.bridge_port, adapter.servers)
    loop = asyncio.get_running_loop()
    stop = asyncio.Event()
    if sys.platform != "win32":
        for sig in (signal.SIGTERM, signal.SIGINT):
            loop.add_signal_handler(sig, stop.set)
    ended = asyncio.create_task(adapter.server_ended.wait())
    await asyncio.wait([serving, ended, asyncio.create_task(stop.wait())], return_when=asyncio.FIRST_COMPLETED)
    server.should_exit = True
    gone = [s for s in adapter.servers if process_of(s).returncode is not None]
    for s in [*adapter.servers, adapter.pending]:
        if s is not None:
            await stop_server(s)
    if ended.done():
        sys.exit("; ".join(f"marimo's server for {s.root} ended ({process_of(s).returncode})" for s in gone))


if __name__ == "__main__":
    asyncio.run(main(), loop_factory=asyncio.ProactorEventLoop if sys.platform == "win32" else None)
