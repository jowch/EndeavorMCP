# Runtime core

How the runtime is split: a language-neutral core in Rust, and a small adapter
per notebook engine, so Pluto, marimo and Ember (our R notebook engine) sit
behind one boundary. The core and the Pluto adapter are built; the marimo and
Ember adapters are not. R support, Endeavor's side included, is designed in
Ember's repository (https://github.com/jowch/Ember); marimo in
[marimo.md](https://github.com/jowch/Endeavor/blob/main/docs/marimo.md).

## Summary

The core is a long-lived Rust process per host (`endeavor core`),
started by the `endeavor` helper. It owns the runtime's one port
([one-port.md](one-port.md)) and all tool semantics. Each notebook kind is an **engine** that owns its own dependency
graph, file and UI, driven by the core through a small **adapter** written in
the engine's language and running in the engine's process:

| Engine | Language | Adapter |
| --- | --- | --- |
| Pluto | Julia | `runtime/` (`EndeavorRuntime`), built |
| marimo | Python | `runtime-py/` ([marimo.md](https://github.com/jowch/Endeavor/blob/main/docs/marimo.md)), not built |
| Ember (https://github.com/jowch/Ember) | R | `runtime-r/`, not built |

Ember is a standalone R package in its own repository, usable without
Endeavor. Endeavor treats it exactly like Pluto and marimo.

## What lives where

| Part | What | Where |
| --- | --- | --- |
| Neutral | HTTP server on the runtime's one port, bearer token and Pluto's cookie, Origin/Host checks, passing Pluto's page and WebSocket through, MCP protocol and tool schemas, `/endeavor/call` methods, run policy and plan mode, one notebook per session, idle timers, host tools (`list_folder`, `read_file`, `run_shell`), event subscribers and dedup, author/before/version tracking, read receipts, `search_code`, `runtime.json` | Core |
| Needs cells and graph, not the engine's language | Staging and "ran since edit", `run_conflict` (upstream check), `run_preview`, dependency and symbol tools, cell-order arithmetic, `submit_changes` checks, projection order, cell names in events | Core, fed by the engine's `snapshot` and `graph` |
| Engine internals | For Pluto: ServerSession and lifecycle, `on_event` hooks, safe-preview gate, mutating cells and saving, topology, output serialization and PNG rendering, `validate_cell` parsing, projection exclusions (package cells, `@bind` shim) | Adapter |

The core keeps one `NotebookState` per notebook, dropped when the notebook
shuts down. Tool semantics are tested in Rust against a fake engine
(`crates/endeavor-mcp/src/notebooks/tests.rs`), and end to end against the
real Pluto adapter (`crates/endeavor-mcp/tests/e2e_julia.rs`).

## Processes

```
app ── ssh/stdio frames ── endeavor ── core (Rust, the runtime's one port)
                                               ├─ julia: Pluto + adapter (private UI and bridge ports) ── Pluto workers
                                               ├─ python: marimo + adapter (private ports)
                                               └─ R: Ember + adapter (private ports) ── R workers
```

- The core may be a subcommand of the helper (`endeavor core`), so
  there is still one binary to ship and pin per host.
- Adapters start lazily, when a notebook of their kind is opened, as
  [marimo.md](https://github.com/jowch/Endeavor/blob/main/docs/marimo.md) proposes. Julia does too when the core is
  started with `--julia-when-needed` (the plugin, its machines and `serve`):
  it starts on the first Julia notebook opened or made, or when a browser asks
  for Pluto's page. A user who only uses R never finds, downloads or starts
  Julia. The app starts the core without the flag for now, so Julia starts
  with the core, as before.
- Each relayed connection goes to the core's one port. The core passes
  `/mcp` and `/endeavor/…` to itself and every other path to Pluto's private
  port, adding Pluto's secret. With more than one engine, each later engine
  gets a path prefix of its own (`/marimo/…`, `/ember/…`) and Pluto stays at
  `/`.
- The core's notebook state talks to the engines through one router
  (`notebooks/engines.rs`). A call goes to the engine of the notebook it
  names (learned from what each engine reports), of the file `open` or `new`
  names (by its extension), or else to Pluto's; `snapshot` and `status` of
  every notebook ask each running engine and put the notebooks together, each
  with its own engine's `seq`. Each engine's notifications are followed on
  their own. Pluto's adapter starts with the core, or on first need (above); R's (`runtime/r/adapter.R`,
  Ember in its own R process) starts the first time an R notebook is opened or
  made. httpuv can't stream a response, so R's notifications come by
  long-polling `GET /notifications?after=<seq>`, which the core turns back
  into the stream it reads from Julia.

## The engine interface

Every adapter speaks the same calls and notifications over loopback HTTP on
its own bridge port, with the bearer token the core gives it: the core
`POST`s each call to `/adapter` as `{"method", "params"}` and reads the
reply's `result` or `error`, and reads notifications from one long-lived
`GET /notifications` stream, a `data: {"method", "params"}` line each. Not
stdio: an engine's stdout and stderr are the runtime log, which Pluto,
packages and notebook code all print to, so a stdio protocol would need a
pipe of its own, and every adapter already serves HTTP (the engine's UI). The core opens the
stream once the engine answers and reopens it if it drops, rereading every
notebook when it does. The core keeps one `AdapterProcess` per engine kind;
what differs per kind (launch command, detection, page adapter, skills) is
the small `Backend` enum in `crates/wire/src/backend.rs`.

Nothing calls `interrupt` yet.

Core → engine:

| Call | Returns |
| --- | --- |
| `open(path, run)` / `new(path? \| folder?)` | notebook id, path, process status; `new` also its cells |
| `shutdown(nid)` | whether it was in safe preview |
| `allow_execution(nid, run, timeout)` | whether it was already allowed, whether it ran, process status |
| `snapshot(nid)` | path, order, process status, whether execution is allowed, whether it's in safe preview, per cell: code, folded, running, queued, errored, last run time, runtime, output summary or structured error, whether the tools hide it (boilerplate such as Pluto's package cells) and whether it's markdown. Without `nid`, every open notebook, in the engine's order |
| `graph(nid, fresh?, refresh?, edges?, packages?)` | per cell, in the engine's order: definitions, function names, references, as of the engine's last analysis (`fresh`: of the notebook as it is now, not kept); with `edges`, each cell's direct upstream and downstream cells; with `packages`, the packages each cell loads; the runnable cells in run order, and the rest |
| `apply(nid, ops)` | ops: set code (refused, before any op applies, if a cell's code isn't the `expected` code), insert at an index, delete, move to an index, fold. The engine saves the file and updates its own UI. The inserted cells' ids |
| `run(nid, cells, wait, timeout)` | accepted, or not because gated (with the process status). The run always starts in the background and is watched until its task ends, with no time limit (only the notebook's process ending or leaving the session cuts it short): a failed task releases the cells marked queued by hand, and `run_finished` follows. With `wait`, the reply is read when the run is over or after `timeout` seconds, whichever is first: the process status, which cells finished, `timed_out` (the notebook's cells still running or queued, empty when the run is over) and, if the process ended by itself, `exited`. A run that outlasts `timeout` goes on and sends `run_finished` as an unwaited one does. `timeout` is ignored without `wait` |
| `interrupt(nid)` | — |
| `restart(nid, timeout)` | the engine's own restart: a new process, then every cell runs (refused in safe preview) |
| `move(nid, path)` | the file moved to `path` (checked by the core), and its new path |
| `render_png(nid, cell)` | base64 PNG or none, and the output's MIME type |
| `validate(nid, cell, code)` | parse errors |
| `status()` | the engine's own status (`session_status`) |

Engine → core, as notifications, each naming its notebook:

- `cell_state(nid, cells)`: cells whose state changed, each with its code
  (users edit in the engine's UI), running, queued and errored. Pluto's hook
  doesn't say which cell changed, so its adapter sends every cell;
- `notebook_opened(nid, path)`; `notebook_shut_down(nid)`, once it has left
  the engine (a restart in place isn't one); `file_saved(nid)`;
  `execution_done(nid)`, when a run finishes; `topology_changed(nid)`, when
  the dependency graph changed; `run_finished(nid, cells)`, the cells a run
  finished that the core didn't wait for or stopped waiting for.

Notifications say when to look; the core reads a fresh `snapshot` and `graph`
each time it tells the app anything, coalescing a burst of notifications
into one read. The code in `cell_state` also goes straight into `author`
tracking, so an edit undone before the next read still counts as the user's.

Each notification carries a `seq` that `apply` also advances and returns, and
`snapshot` says the `seq` it read at. A snapshot or `cell_state` numbered below
the `apply` of the agent's last edit to a cell may show the code from before
it, so it doesn't change who last changed that cell (both come on their own
connections, so either can reach the core after the edit's reply).

Everything else (staging, receipts, `run_preview`, `before`/`author`,
conflict warnings, event diffing) is computed in the core from these.

### Core rules

Some behaviour is a rule of our tools rather than of an engine, so the core
holds it and it applies to every engine:

- **Running a cell runs its unrun ancestors first.** The core reads the
  graph and passes the full list to `run`.
- **Cell identity across reloads.** Pluto and Ember store cell IDs in
  the file. marimo doesn't; for it the core matches old IDs to new cells by
  code and reports unmatched ones as "cell no longer found".
- **Staged edits from outside** (marimo's `--watch`, a user editing the file)
  show as `unrun`, the same state Pluto's staged edits produce.
- **Opening a notebook** from disk runs nothing. Pluto would run every cell,
  so Endeavor opens it in safe preview (execution not allowed) until the user
  runs it; notebooks Endeavor or Claude create skip safe preview (see
  [ui-spec.md](https://github.com/jowch/Endeavor/blob/main/docs/ui-spec.md), "Safe preview"). marimo (`auto_instantiate =
  false` by default) and Ember open without running. Cells run on request,
  ancestors first.

What stays in each adapter: calls into the engine's API or internals, output and
error conversion, hiding boilerplate in `read_notebook_code` (Pluto's
package cells and `@bind` shim, marimo's decorators and `return` lines), and
the engine's own package handling.

Under this split `runtime-py/` in [marimo.md](https://github.com/jowch/Endeavor/blob/main/docs/marimo.md) is only the
adapter: steps 1 and 2 of "The Python runtime" and `marimo_api.py`;
`/endeavor/events`, tool serving and host tools come from the core.

## What else the core does

For agents other than Claude Code ([other-agents.md](https://github.com/jowch/Endeavor/blob/main/docs/other-agents.md) items 2
and 3), and for the app:

- It keeps each session's last 64 tool results and answers
  `endeavor/tool_result`, for agents whose own result says only "success".
- It holds a call that runs code while the session's policy is "ask" and the
  app turned this on (`endeavor/set_policy` with `asks: true`). The held call
  is listed under `asks` in `/endeavor/events` until the app answers with
  `endeavor/answer_run`, the agent cancels, or its connection closes. While
  it waits, its reply is an event stream that has already begun (see
  [endeavor-mcp.md](endeavor-mcp.md), "Transport"), so the agent's client
  doesn't time out waiting for a response to start. In Manual the app adds
  `edits: true`, and the core holds a call that changes the notebook the same
  way, whatever the run policy.
- It reports in `/endeavor/events` the app build it was started by (`build`), so the
  app can tell a runtime from an older build and hold back what that runtime
  can't do.

## Next

1. **Ember and its adapter** (the build order in Ember's repository).
2. **marimo adapter** ([marimo.md](https://github.com/jowch/Endeavor/blob/main/docs/marimo.md) steps 2 onward, minus what the
   core provides).

## Open questions

- **Transport to adapters:** loopback HTTP (see "The engine interface").
  A marimo or Ember adapter whose engine keeps stdout clean could use stdio
  instead; the core would need a second `Upstream` for it.
