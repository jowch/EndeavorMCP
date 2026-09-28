# Runtime core

Design for moving the language-neutral half of the runtime from Julia into a
Rust process, so Pluto, marimo and turtleR (our R notebook engine) sit
behind one boundary. Step 1 below is built; the rest is not yet. Endeavor's side of R support
is in [r-notebooks.md](r-notebooks.md), turtleR's own design in its
repository (https://github.com/jowch/turtleR); marimo in
[marimo.md](marimo.md).

_Drafted 2026-09-26_

## Summary

Today one Julia process (`runtime/boot.jl` + `EndeavorRuntime`, about 3,260
lines) does two jobs: it runs Pluto, and it serves everything the app and the
agent talk to (MCP tools, `/call`, `/events`, policy, staging, host tools).
Only the first job needs Julia.

The core is a long-lived Rust process per host, started by `endeavor-remote`
in place of `boot.jl`. It owns the bridge port and all tool semantics. Each
notebook kind is an **engine** that owns its own dependency graph, file and
UI, driven by the core through a small **adapter** written in the engine's
language and running in the engine's process:

| Engine | Language | Adapter |
| --- | --- | --- |
| Pluto | Julia | `runtime/` (what is left of `EndeavorRuntime`) |
| marimo | Python | `runtime-py/` ([marimo.md](marimo.md)) |
| turtleR ([r-notebooks.md](r-notebooks.md)) | R | `runtime-r/` |

turtleR is a standalone R package in its own repository, usable without
Endeavor. Endeavor treats it exactly like Pluto and marimo.

## How much moves to Rust

Classified from the current source (approximate line counts):

| Class | Lines | What | Where it goes |
| --- | --- | --- | --- |
| A. Neutral | ~1,650 | HTTP/SSE server, bearer token, Origin/Host checks, MCP protocol and tool schemas, `/call` methods, run policy and plan mode, one notebook per session, idle timers, host tools (`list_folder`, `read_file`, `run_shell`), event subscribers and dedup, author/before/version tracking, read receipts, `search_code`, `runtime.json` | Core, unchanged behaviour |
| B. Needs cells and graph, not Julia | ~730 | Staging and "ran since edit", `run_conflict` (upstream check), `run_preview`, dependency and symbol tools, cell-order arithmetic, `submit_changes` checks, projection order, cell names in events | Core, fed by the engine's `snapshot` and `graph` |
| C. Pluto internals | ~850 | ServerSession and lifecycle, `on_event` hooks, safe-preview gate, mutating Pluto cells and saving, topology, output serialization and PNG rendering, `validate_cell` parsing, projection exclusions (package cells, `@bind` shim) | Pluto adapter, stays Julia |

So roughly three quarters of the runtime moves. What stays in Julia is the
part that is Pluto-specific anyway, and it shrinks to a thin adapter.

Two side effects:

- **One `NotebookState` struct per notebook** replaces today's module-level
  dictionaries keyed by notebook ID. This fixes a current leak:
  `clear_notebook_staging!` and `clear_all_pending!` are never called, and
  `_AUTHORS`/`_BEFORES` are never cleared on shutdown.
- **Tests split.** The protocol, auth, events, policy, host-tool, sharing and
  idle testsets (about a quarter of `test/runtests.jl`) become Rust tests
  against a fake engine. Tool-semantics tests run once in Rust against the
  fake engine and again end to end against each real adapter.

## Processes

```
app ── ssh/stdio frames ── endeavor-remote ── core (Rust, bridge port)
                                               ├─ julia: Pluto + adapter (Pluto UI port) ── Pluto workers
                                               ├─ python: marimo + adapter (marimo UI port)
                                               └─ R: turtleR + adapter (turtleR UI port) ── R workers
```

- The core may be a subcommand of the helper (`endeavor-remote core`), so
  there is still one binary to ship and pin per host.
- Adapters start lazily, when a notebook of their kind is opened, as
  [marimo.md](marimo.md) already proposes. A user who only uses R never
  downloads Julia.
- `crates/wire`'s `Target::{Pluto, Bridge}` becomes `Target::{Bridge,
  NotebookUi(backend)}`. The core writes `runtime.json`, listing each
  engine's UI port as it starts.

## The engine interface

Every adapter speaks the same calls and notifications over loopback HTTP on
its own bridge port, with the bearer token the core gives it: the core
`POST`s each call to `/adapter` as `{"method", "params"}` and reads the
reply's `result` or `error`, and reads notifications from one long-lived
`GET /notifications` stream, a `data: {"method", "params"}` line each. Not
stdio: an engine's stdout and stderr are the runtime log, which Pluto,
packages and notebook code all print to, so a stdio protocol would need a
pipe of its own, and every adapter already serves HTTP (Pluto's UI, and
until step 5 is done, the tools the core passes on). The core opens the
stream once the engine answers and reopens it if it drops, rereading every
notebook when it does. The core keeps one `AdapterProcess` per engine kind;
what differs per kind (launch command, detection, page adapter, skills) is
the small `Backend` enum [marimo.md](marimo.md) proposes for the app.

All of these are built (steps 5a and 5b, and `restart` and `move` for the
app's notebook menu); nothing calls `interrupt` yet.

Core → engine:

| Call | Returns |
| --- | --- |
| `open(path, run)` / `new(path? \| folder?)` | notebook id, path, process status; `new` also its cells |
| `shutdown(nid)` | whether it was in safe preview |
| `allow_execution(nid, run, timeout)` | whether it was already allowed, whether it ran, process status |
| `snapshot(nid)` | path, order, process status, whether execution is allowed, whether it's in safe preview, per cell: code, folded, running, queued, errored, last run time, runtime, output summary or structured error, whether the tools hide it (boilerplate such as Pluto's package cells) and whether it's markdown. Without `nid`, every open notebook, in the engine's order |
| `graph(nid, fresh?, refresh?, edges?, packages?)` | per cell, in the engine's order: definitions, function names, references, as of the engine's last analysis (`fresh`: of the notebook as it is now, not kept); with `edges`, each cell's direct upstream and downstream cells; with `packages`, the packages each cell loads; the runnable cells in run order, and the rest |
| `apply(nid, ops)` | ops: set code (refused, before any op applies, if a cell's code isn't the `expected` code), insert at an index, delete, move to an index, fold. The engine saves the file and updates its own UI. The inserted cells' ids |
| `run(nid, cells, wait, timeout)` | accepted, or not because gated (with the process status); waited for, which cells finished and which timed out |
| `interrupt(nid)` | — |
| `restart(nid, timeout)` | the engine's own restart: a new process, then every cell runs (refused in safe preview) |
| `move(nid, path)` | the file moved to `path` (checked by the core), and its new path |
| `render_png(nid, cell)` | base64 PNG or none, and the output's MIME type |
| `validate(nid, cell, code)` | parse errors |
| `status()` | the engine's own status (`pluto_session_status`) |

Engine → core, as notifications, each naming its notebook:

- `cell_state(nid, cells)`: cells whose state changed, each with its code
  (users edit in the engine's UI), running, queued and errored. Pluto's hook
  doesn't say which cell changed, so its adapter sends every cell;
- `notebook_opened(nid, path)`; `notebook_shut_down(nid)`, once it has left
  the engine (a restart in place isn't one); `file_saved(nid)`;
  `execution_done(nid)`, when a run finishes; `topology_changed(nid)`, when
  the dependency graph changed; `run_finished(nid, cells)`, the cells a run
  the core didn't wait for finished.

Notifications say when to look; the core reads a fresh `snapshot` and `graph`
each time it tells the app anything, coalescing a burst of notifications
into one read. The code in `cell_state` also goes straight into `author`
tracking, so an edit undone before the next read still counts as the user's.

Everything else (staging, receipts, `run_preview`, `before`/`author`,
conflict warnings, event diffing) is computed in the core from these.

### Rules that become core rules

Some behaviour [marimo.md](marimo.md) put in the Python runtime is really a
rule of our tools, so it moves to the core and applies to every engine:

- **Running a cell runs its unrun ancestors first.** The core reads the
  graph and passes the full list to `run`.
- **Cell identity across reloads.** Pluto and turtleR store cell IDs in
  the file. marimo doesn't; for it the core matches old IDs to new cells by
  code and reports unmatched ones as "cell no longer found".
- **Staged edits from outside** (marimo's `--watch`, a user editing the file)
  show as `unrun`, the same state Pluto's staged edits produce.
- **Opening a notebook** follows each engine's own default, so existing
  notebooks behave the way their users know: Pluto runs every cell; marimo
  (`auto_instantiate = false` by default) and turtleR open without
  running, and cells run on request, ancestors first.

What stays in each adapter: calls into the engine's API or internals, output and
error conversion, hiding boilerplate in `read_notebook_code` (Pluto's
package cells and `@bind` shim, marimo's decorators and `return` lines), and
the engine's own package handling.

Under this split `runtime-py/` in [marimo.md](marimo.md) shrinks to the
adapter: steps 1 and 2 of "The Python runtime" and `marimo_api.py` stay;
`/events`, tool serving and host tools come from the core.

## Build order

1. **Core with the Pluto adapter, no behaviour change.** Includes
   [marimo.md](marimo.md) build step 1 (one MCP server name, `Backend` in
   the app). The existing Julia test suite is the reference: the same
   scenarios must pass through the core. Each step below ends in a working
   app.
2. **turtleR and its adapter** ([r-notebooks.md](r-notebooks.md) build
   order).
3. **marimo adapter** ([marimo.md](marimo.md) steps 2 onward, minus what the
   core now provides).

Step 1 is the largest risk: about 2,400 lines of tested behaviour are
rewritten. Doing it against Pluto first means a known-good backend checks the
port before a new engine adds its own bugs. It goes in small steps, each
ending in an app that behaves as before:

1. The app's side: MCP server `notebook`, `notebook://pluto/…` annotation
   links, `Backend` in the app. Done.
2. Clear a notebook's runtime state when it shuts down (a leak found while
   planning this). Done.
3. `endeavor-remote core`: the core owns the bridge port, starts Julia as its
   child and writes `runtime.json`, and forwards every request to Julia's
   bridge unchanged. Done.
4. Move the handlers that need no notebook state into the core, one at a
   time: run policy and plan mode, host tools, idle stop, sharing checks,
   auth and Host/Origin checks. Their tests move to Rust; the Julia code goes.
   Done for the agent's MCP connection (Julia answers what the core passes
   to its internal `/dispatch`), auth and Host/Origin checks, run policy and
   plan mode, and host tools. Idle stop, sharing checks and the
   one-notebook-per-session binding stay for step 5: each needs a
   notebook's cells, running state or path.
5. The adapter interface, then the handlers that need the graph. In two
   parts:
   - 5a, done: the interface (`snapshot`, `graph`, `shutdown` and the
     notifications) and the core's own `NotebookState` per notebook, dropped
     when it shuts down. The core serves `/events` (subscribers, the state on
     connect, sending only what changed) and keeps author, `before` and
     version tracking, cell names, idle stop (`keep_notebook_alive`,
     `endeavor/set_idle_limit`, `idle_stopped`), `endeavor/stop_notebook`,
     and the one-notebook-per-session binding (`endeavor/set_notebook`). It
     attributes an edit to the agent by watching `edit_cell`, `edit_cells`
     and `add_cell` pass through, reading the notebook just before.
   - 5b, done: every tool rule. Staging, read receipts and each cell's last
     change join `NotebookState`; the core answers every notebook tool,
     `tools/list`, `initialize` and `endeavor/run_preview`, carrying changes
     out through `apply`, `run` and the other calls, and `pending_run` left
     `snapshot`. Julia is now the Pluto adapter: Pluto's session, the
     adapter's calls and notifications, output and error conversion, what
     the tools hide, and its own `/call` for `endeavor/set_folder` and
     `endeavor/shutdown`. Lists Julia kept in hash tables (`pending_run`,
     `stale_cell_ids`, `search_code`, `upstream`, `downstream`) now come in
     notebook order.
   - Rebased onto `main`, the core also answers what `main` added to Julia
     meanwhile: `list_notebooks`' `this_session`, the packages in
     `run_preview`, and the app's `endeavor/restart_notebook`,
     `endeavor/move_notebook`, `endeavor/file_info` and
     `endeavor/new_notebook`.

## Open questions

- **Transport to adapters:** settled in step 5a as loopback HTTP (see "The
  engine interface"). A marimo or turtleR adapter whose engine keeps stdout
  clean could use stdio instead; the core would need a second
  `Upstream` for it.
- **Is step 1 worth doing before R?** The alternative is building the core
  for R only and leaving Pluto on the Julia runtime. That keeps two
  implementations of the same tool rules, which would drift. Not
  recommended.
