# Runtime core

Design for moving the language-neutral half of the runtime from Julia into a
Rust process, so Pluto, marimo and turtleR (our R notebook engine) sit
behind one boundary. Nothing here is built yet. Endeavor's side of R support
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

Every adapter speaks the same JSON-RPC calls on stdio. The core keeps one
`AdapterProcess` per engine kind; what differs per kind (launch command,
detection, page adapter, skills) is the small `Backend` enum
[marimo.md](marimo.md) proposes for the app.

Core → engine:

| Call | Returns |
| --- | --- |
| `open(path, allow_run)` / `new(path?)` | notebook id, cells, whether execution is gated |
| `shutdown(nid)` | whether it was in safe preview |
| `allow_execution(nid, run)` | — |
| `snapshot(nid)` | path, order, per cell: code, folded, running, queued, errored, last run time, runtime, output summary or structured error, process status |
| `graph(nid)` | per cell: definitions, references; topological order |
| `apply(nid, ops)` | ops: set code, insert, delete, move, fold. The engine saves the file and updates its own UI |
| `run(nid, cells)` | accepted, or refused because gated |
| `interrupt(nid)`, `restart(nid)` | — |
| `render_png(nid, cell)` | image bytes or none |
| `validate(nid, code)` | parse errors |

Engine → core, as notifications:

- `cell_state(nid, cell, {...})` including code, because users edit in the
  engine's UI;
- `notebook_opened`, `notebook_shut_down`, `file_saved`,
  `topology_changed`.

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
   bridge unchanged.
4. Move the handlers that need no notebook state into the core, one at a
   time: run policy and plan mode, host tools, idle stop, sharing checks,
   auth and Host/Origin checks. Their tests move to Rust; the Julia code goes.
5. The adapter interface over stdio, then the handlers that need the graph:
   staging and read receipts, `run_preview`, author and `before` tracking,
   event diffing. What is left in Julia is the Pluto adapter.

## Open questions

- **Transport to adapters:** stdio JSON-RPC is simplest and needs no port.
  Pluto's adapter also serves Pluto's HTTP UI, so it listens on a port
  regardless. Stdio for control, a port only for the UI, is the proposal.
- **Is step 1 worth doing before R?** The alternative is building the core
  for R only and leaving Pluto on the Julia runtime. That keeps two
  implementations of the same tool rules, which would drift. Not
  recommended.
