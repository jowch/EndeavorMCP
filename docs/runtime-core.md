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
| Pluto | Julia | `runtime/` (`EndeavorRuntime`, [pluto-adapter.md](pluto-adapter.md)), built |
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
  for a page of Pluto's other than the runtime's own link (`/`), which offers a
  button instead. A user who only uses R never finds, downloads or starts
  Julia. To spare a Julia user the wait, the core starts Julia ahead, in
  the background, for a session whose folder has Pluto notebooks (three levels
  deep, as the app's folder scan), or which its caller says is a Julia session
  (`kind` in `endeavor/set_session_folder`: `julia`, `r` or `unknown`; `r`
  never starts it). The kind is kept for the session until a later call says
  another (`unknown` forgets it), and a `new_notebook` without a path is made
  in it: an R session's is an R notebook, named
  `notebook.R` (then `notebook 2.R`). Starting ahead never downloads Julia: if Julia would need
  downloading, it waits for the first Julia notebook. In a fresh depot, Pluto's
  packages still install on that first start, as on any start. The app starts the core without the flag for now, so Julia starts
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
its own bridge port, with the engines' key the core gives it (made at each
start, kept in no file, and neither the MCP bearer token nor the pages'
token, since `/adapter` runs cells without the core's run gate): the core
`POST`s each call to `/adapter` as `{"method", "params"}` and reads the
reply's `result` or `error`, and reads notifications from one long-lived
`GET /notifications` stream, a `data: {"method", "params"}` line each. Not
stdio: an engine's stdout and stderr are the runtime log, which Pluto,
packages and notebook code all print to, so a stdio protocol would need a
pipe of its own, and every adapter already serves HTTP (the engine's UI). The core opens the
stream once the engine answers and reopens it if it drops, rereading every
notebook when it does. An engine other than Pluto's that stops answering
(R's or Python's) is dropped, and the next R notebook opened starts it again. Its
notebooks are kept as closed, with why it ended, until R reports one open
again (an open's reply, or a snapshot that lists it) or the app's
`stop_notebook` dismisses it: how
its process ended and the last lines it wrote to stderr, which the core
passes on to the runtime log and keeps. A call that names one of them answers
`notebook_closed` with that reason, and `/endeavor/events` lists them under
`engine_ended` (`{path, language, why, ended_at}`, with `ended_at` in ms telling one ending of a path from the next). The core keeps one `AdapterProcess` per engine kind;
what differs per kind (launch command, detection, page adapter, skills) is
the small `Backend` enum in `crates/wire/src/backend.rs`.

The core calls `interrupt` for the user's Stop and for the agent's `interrupt_notebook` (see the Stop bullets below).

Core → engine:

| Call | Returns |
| --- | --- |
| `open(path, run)` / `new(path? \| folder?)` | notebook id, path, process status; `new` also its cells |
| `shutdown(nid)` | whether it was in safe preview |
| `allow_execution(nid, run, timeout)` | whether it was already allowed, whether it ran, process status, and the `run` id of the run it started, as `run` names one, if it goes on |
| `snapshot(nid)` | path, order, process status, whether execution is allowed, whether it's in safe preview, per cell: code, folded, running, queued, errored, last run time, runtime, output summary or structured error, whether the tools hide it (boilerplate such as Pluto's package cells) and whether it's markdown. Without `nid`, every open notebook, in the engine's order |
| `graph(nid, fresh?, refresh?, edges?, packages?)` | per cell, in the engine's order: definitions, function names, references, as of the engine's last analysis (`fresh`: of the notebook as it is now, not kept); with `edges`, each cell's direct upstream and downstream cells; with `packages`, the packages each cell loads; the runnable cells in run order, and the rest |
| `apply(nid, ops)` | ops: set code (refused, before any op applies, if a cell's code isn't the `expected` code), insert at an index, delete, move to an index, fold. The engine saves the file and updates its own UI. The inserted cells' ids |
| `run(nid, cells, wait, timeout)` | accepted, or not because gated (with the process status). The run always starts in the background and is watched until its task ends, with no time limit (only the notebook's process ending or leaving the session cuts it short): a failed task releases the cells marked queued by hand, and `run_finished` follows. With `wait`, the reply is read when the run is over or after `timeout` seconds, whichever is first: the process status, which cells finished, `timed_out` (the notebook's cells still running or queued, empty when the run is over) and, if the process ended by itself, `exited`. A run that goes on (unwaited, or outlasting `timeout`) is named by the reply's `run` id, which its `run_finished` carries. In R, a run in autorun covers the cells downstream of its cells, which Ember queues as each result comes in, so it is over when those have run too. In R, the wait also ends when a cell of the run waits on a prompt (`readline()`, `menu()`), named by `prompted`; the snapshot's cell then has `prompt` (`text`, `choices`). `timeout` is ignored without `wait` |
| `interrupt(nid)` | — |
| `answer(nid, cell, text)` | R only: Ember's `answer_input()`, the answer to the prompt `cell` waits on, which lets the run go on; refused (`no_prompt`) when it waits on none |
| `restart(nid, timeout)` | the engine's own restart: a new process, then every cell runs (refused in safe preview) |
| `preview_packages(nid, date?, timeout)` | R only: Ember's `preview_date()` for `date` (today, in local time, if none, as the Packages tab's Update): the date, the notebook's own, `status` (`ready`, `failed`, or `fetching` if the index didn't come within `timeout`), `changes` and `problems`. Nothing changes |
| `set_input(nid, name, value, wait, timeout)` | R and Python. Python: marimo's `set_ui_value` on the `mo.ui` widget assigned to `name`, after the adapter checks the value against its range or options (`invalid_value`); refused (`input_refused`) when the name isn't such a widget or its cell isn't fresh; then a `run` of the cells that read it, with `lazy` false. R: Ember's `set_input()` for the input bound to `name`, refused (`input_refused`) when none is bound or its cell isn't fresh, and `invalid_value` when R's check refuses the value. A JSON number for a dropdown or radio is matched to a numeric choice, and a JSON array is a vector. The input's cell, the value as R holds it, `cells` (the readers that rerun, or in lazy mode, `lazy` true, turn stale), and in autorun the reply of a `run` of those cells, which Ember queued itself. R reads the value only after the cell it is running; not read by the deadline, the reply is `pending` true with no `cells`, and the value lands later |
| `set_package_date(nid, date)` | R only: Ember's `set_date()`, refused (`no_preview`) unless that date's preview is ready and current and matches the `changes` `preview_packages` last returned (Ember redoes its proposal when the packages change): the new date and the applied changes |
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
  the dependency graph changed; `run_finished(nid, run, cells)`, a run the
  core didn't wait for or stopped waiting for ended, with the cells it
  finished (a cell can end it unfinished, queued again by a restart's run).

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
- When the agent cancels a call (the user pressed Stop in the chat), the
  notebook's run goes on (#227): stopping the agent and stopping the notebook
  are separate. A waited call that is cancelled still answers when its wait
  ends, and the agent ignores the answer. The run is still the session's, so its
  next read or waited run waits for it. A session's run stops counting as its
  own when it ends (`run_finished`), when the process ends, or on a Restart,
  even if a later run (a restart's) is running its cells.
- `interrupt_notebook`, like the page's Stop, interrupts the notebook's run,
  whoever started it. The interrupt lands 2 s or more after it is asked for:
  Pluto first waits 2 s for the run to end by itself. A cell that starts in
  that time (one queued behind the running one) is the one interrupted. A cell
  that doesn't stop for Ctrl+C (one long `sleep`) ends with its notebook's
  process, as with Pluto's Stop: the notebook then waits for a restart, and
  this isn't reported as a crash (`exited`). A process that lives through the
  interrupt and ends later is reported as a crash, as before.
- The agent can stop a run itself with `interrupt_notebook` (#161), which is the
  notebook's Stop button: it interrupts the run whoever started it. It asks nothing in
  any mode, plan mode refuses it, and it stays in the session's own notebook. Its
  result lists the cells that were running and `stopped`, the queued ones `dropped`,
  and any `still_running`. Pluto's interrupt returns once the run has ended; Ember's
  sends SIGINT and returns, so the core watches the cells for up to 10 s, within the
  call's 45 s. A cell still running then is in code that ignores the interrupt, and
  the result says only a restart ends it.
- The agent can restart a notebook with `restart_notebook` (#202), the page's Restart:
  the same `restart` call as `endeavor/restart_notebook`. It runs code, so Ask to run
  asks first, and plan mode refuses it. Pluto's restart then runs every cell, and that
  run is the session's, as `allow_execution`'s is, so a read waits for it. That run includes a cell that was running, stuck or
  not: the result names those (`running_again`) and says to edit them first. Ember's leaves every cell not run, and the result says
  how to run them.
- The agent can close its notebook with `close_notebook`, the app's Stop notebook:
  the same `shutdown` call as `endeavor/stop_notebook`, for every session and the
  user's browser too. It drops the session's binding, so the session may open
  another. Every value goes, so Ask to run asks first, and plan mode refuses it.
  A notebook that closed meanwhile fails with `notebook_not_found`.
- It reports in `/endeavor/events` the app build it was started by (`build`), so the
  app can tell a runtime from an older build and hold back what that runtime
  can't do.

## How the core runs (`core.rs`)

### Its process and its record

- The helper starts `endeavor core …` with `ENDEAVOR_TOKEN`,
  `ENDEAVOR_LAUNCHER` and `ENDEAVOR_BUILD` (the app build it came from, which
  it reports to the app) in its environment. Its stdout and stderr are the
  runtime's log, which Julia shares. Secrets go in the environment, not argv,
  which `ps` shows to every user.
- Started without the app (`standalone`), the environment also has
  `ENDEAVOR_FOLDER` (the notebooks' folder) or `ENDEAVOR_NO_FOLDER` (work in
  the home folder), `ENDEAVOR_PORT`, `ENDEAVOR_HOST_TOOLS` (the host name
  under which every session gets the host tools) and `ENDEAVOR_IDLE_HOURS`
  (48 when not set). `ENDEAVOR_EXIT_IDLE` ends the runtime once no notebook
  has been open that long; a client asks for it on a runtime it starts in the
  background, which has no one else to stop it.
- The core holds the start lock until `runtime.json` is written, so a client
  can tell a start under way from one that died: whoever sees the lock free
  and no record knows the start died. Everything a reader of the record needs
  is set before it is written. The record is written whole and readable only
  by this user, so a reader never sees half of it and nobody else sees the
  token.
- The record has the core's pid and start time (which tell it from a later
  process given its pid), its port, launcher, node, job (so a reconnect can
  find the job with `squeue`), tokens, `exits_when_idle` and the network
  settings it started with.
- Relative paths in the tools, and Julia's, start in the notebooks' folder,
  never in the client's working folder, which may be one a plugin update
  removes.
- The engines' key is made at each start and kept in no file. Julia's and R's
  bridges run cells without the core's run gate, so they take neither the MCP
  bearer token, which an agent's config carries, nor the pages' token.

### The interface number

`INTERFACE`, recorded as `interface`, numbers what a core offers the
processes that use it: the notebook tools' names and arguments at `/mcp`,
the app's `/endeavor/call`s and `/endeavor/events`, and the record's own
fields. A caller of another build uses a runtime with the same number as it
is, and treats one with another number, or none, as another build's.

Two builds with one number must take each other's calls both ways: a newer
front meets an older core as often as the reverse. So raise it with any
change a caller of the build before would get wrong, additions included (a
tool, an argument, a call), and with a change in what one returns or does
while its arguments stay the same. A record field older callers ignore and
newer ones do without (`network`) doesn't count, nor do descriptions or
changes inside the core. Tests in `mcp.rs` fail when the tools' names or
arguments change, or a call or record field is added, removed or renamed.
Nothing catches a change in what a call or the events stream returns.

### Julia

Julia (`julia boot.jl`, with Pluto) is the core's child, on private ports
the core picks (Pluto's and Julia's bridge, both held at once so the OS can't
hand out the same port twice).

- **When it starts.** Without `--julia-when-needed`, at once, and
  `runtime.json` is written once Julia is ready. With it, the first time
  something needs it (see "Processes" above). A warm start, which nothing
  asked for yet, never downloads Julia. A cluster job submitted before any
  notebook needed Julia is handed the one the login node found or downloaded
  (`endeavor/use_julia`, #80).
- **Its flags** must keep the compile cache's key the same as
  `standalone::precompile_command`'s, or that background precompile is
  wasted.
- **One thread runs every start**, for the core's life: on Linux a process
  ends with the thread that started it (`PR_SET_PDEATHSIG`), and a request's
  thread ends with its connection.
- **No console.** Julia is a console program, and the core may have none to
  give it (in the app, the core is the app's own GUI program), so on Windows
  it starts with no window. Pluto's workers share Julia's.
- **The wait.** A call that needs Julia waits up to 30 s (`JULIA_WAIT`), well
  within the 45 s an agent's call waits, and less if the call already waited
  (for the user's answer, say), so the notebook's open has some of the 45 s.
  Then it is told the step, how long so far, and to call again now: each call
  is the wait. An agent told to try again "in a minute", with no way to
  pause, gave up after a dozen tries (#153).
- **A stall.** A start with no sign of progress (a new step, or new lines in
  the log) for 30 minutes fails, and Julia is ended so the next call starts
  it afresh. A first start installs and compiles Pluto's packages for
  minutes, logging each one. The longest quiet stretch is compiling Pluto
  itself: 75 s on a Linux cloud machine with Julia 1.12.6 (2026-10-10). 30
  minutes leaves room for a machine twenty times slower, such as Windows with
  antivirus scanning each file. It may be stuck on anything (a lock, a network
  share, a hung precompile).
- **Failures** are said once to the next call that needs Julia, and that call
  tries again after. `endeavor/julia_status` reads where Julia is without
  starting it, and doesn't count as saying a failure. The app's Retry is
  `endeavor/start_julia`.
- **Once it ran**, Julia ending ends the core, so there is never a second
  `Ready`. Pluto's page is told the notebooks' folder before Julia counts as
  ready, since whoever opens a notebook next may rely on it.
- **Windows pids.** A pid alone may name another process once Julia has gone,
  so Julia is killed only through its pid and start time
  (`winproc::start_time`, #464).

### Stopping

- Julia shares the core's process group, which the helper made, so the
  helper's signals to the group reach both. The core blocks the stop signals
  before any thread starts, for one thread to take, and keeps the mask it
  had for Julia and R.
- A stop signal to the core alone goes on to Julia. A Julia that is ready is
  first asked to shut down through its bridge, and signalled only if that
  fails or it still runs `stop_grace` (10 s) later: a SIGTERM while Julia
  compiles can hang its exit for good (#509). A Julia still starting has no
  bridge and gets the signal. A second signal (a second Ctrl-C) is passed
  straight on.
- If a signal passed on doesn't end Julia (#316: once a core went on after a
  SIGTERM; a real Julia with two notebooks ends in about 4 s), the core asks
  it to shut down as `endeavor stop` does, then kills it. A runtime that
  ends on its own (idle, or left and no longer wanted) does the same (#458).
- The core exits the way Julia did, so the helper reports the same status.
- **The lifeline.** With `--lifeline FD` (from `connect --own-with-client`),
  until the helper says "k" the core watches a pipe from it. Its end without
  "k" means the start was abandoned: the core ends itself, Julia and its
  process group as the helper's own cancel would, and SIGKILLs the group if
  the core still runs ten seconds later. A helper that is killed outright
  can't stop the start it began, and an ssh server may kill it when its
  client goes instead of ending its input (#95). On Windows the lifeline is
  an inherited handle and the core's Job Object takes Julia along (#144).
  After "k", a killed helper leaves the runtime running, as a detach does.
- **When the core dies abruptly** (an OOM kill, `kill -9`), nothing is left to
  stop Julia. Linux sends it SIGTERM (`PR_SET_PDEATHSIG`), which can hang it
  mid-compile (#509), and macOS has no such signal. So a shell waits on a
  pipe whose other end only the core holds: a line on it means Julia ended,
  its end without one that the core died. Then it signals Julia (macOS) and
  kills it `stop_grace` later if the same pid with the same start time still
  runs. On Windows the core's job ends Julia with it.

### R

R's adapter (`runtime/r/adapter.R`, with Ember) starts the first time an R
notebook is opened. How it and Ember's install work is in
[ember-adapter.md](ember-adapter.md).

- **Ember** comes from its r-universe repository, which builds Ember's latest
  main, into a folder of Endeavor's own (one for Endeavor's own R, kept apart
  from the user's R, and one for any other), unless the core has
  `--r-library`. Each start updates it if r-universe has a newer build. If the
  adapter doesn't start with a new build, it starts with the one before, and
  that build is not tried again; the agent is told with its next notebook.
  `ember.dcf` records the build in use, the one before and the ones that
  failed. A build replaced is dated for `install.R`'s cleanup a week later,
  since another runtime may have it loaded.
- **Waiting.** A first install takes minutes (packages build from source),
  longer than an agent's call may wait. A call waits as long as Julia's
  start, then answers `r_installing`, and the install goes on. A call made
  while it runs waits for it too (#175). A first install that ends after its
  call stopped waiting starts R at once, so the next call finds it ready and
  `r_status` never reads `not_started` in between (#148). An update that
  takes long goes on too, and R starts with the Ember installed.
- **A failed install** is said to every call for `retry_after` (60 s), then
  the next call tries again: long enough that agents retrying at once don't
  each start one (six in a minute, #183), short enough that what the user
  fixed (the network, a compiler) is tried without a restart. The words say
  nothing was tried for this call, or an agent whose user just installed a
  compiler says it is still missing.
- **Endeavor's own R** is offered only with `--own-r`, on the user's own
  computer and only on a Mac. A `serve` or `mcp` runtime is always standalone,
  and a server's never gets it. Through `endeavor mcp` the agent installs it
  with `use_machine` once the user agrees; the app's agents have none, so
  there the user installs it from the notebook pane or Settings. R starts
  once for every caller of a runtime the app and the plugin share, so its
  offer is worded for whoever started the runtime.
- **A compiler.** On a Mac the core asks whether the developer tools are
  installed without running `cc`, which opens an installer on a Mac without
  them. Elsewhere it looks for `cc` and `make` on the PATH.
- **The user's settings.** R isn't run `--vanilla`: notebooks run in it, so
  they get the user's ~/.Renviron and ~/.Rprofile (a library in R_LIBS_USER,
  a repos option, GITHUB_PAT) and the site's. R reads the home files only when
  the folder it starts in has none, so the core names the home .Renviron, or
  a project's (renv's, say) would be read and could take Ember's libraries
  away. On Windows R looks in R_USER (Documents), so it keeps its own rules.
  An error in a profile stops Rscript before the adapter runs, so R reads an
  empty profile and the adapter runs the user's, where an error is logged and
  R goes on. Endeavor's own R keeps its own settings.
- **The signal mask.** R starts with the mask the core was started with, not
  the core's, which blocks Ctrl-C, SIGTERM and SIGHUP. R would pass a blocked
  mask to Ember's workers, where a cell's interrupt and the supervisor's
  SIGTERM then wait forever (#161).
- **Output.** What R prints goes to the log, never to the core's stdout,
  which is the MCP channel for `endeavor mcp`. R's last stderr lines say why
  it ended. A killed process can close its stderr a moment before it can be
  reaped, so its exit is waited for, within the same second.
- **Ember's page secret** is the core's to make and is handed to R: hex,
  since it goes into URLs and a cookie, and 32 random bytes, more than
  Ember's own `random_secret(32)`. Ember takes it in the query, the only way
  its WebSocket does, and the Host stays the browser's, since Ember refuses a
  WebSocket whose Origin isn't its Host.
- Ember, its install and R's adapter haven't been tried on Windows.

### Python

Python notebooks' adapter (`runtime/marimo/adapter.py`, which runs marimo)
starts the first time a Python notebook is opened or made. How it works is in
[marimo-adapter.md](marimo-adapter.md). uv runs it, with
the packages in `requirements.txt`, and each notebook gets its own uv
environment from its header. uv is found on the PATH, where its installer
puts it, or in Homebrew's folder.

- The first start may download Python and marimo. A call waits as long as
  Julia's start, then answers `python_installing`, and the start goes on for
  up to 300 s (`MARIMO_START`) for the next call to wait on.
- Only a runtime on this computer starts it: the plugin's, or the app's
  (`--own-r`). A server's runtime has no uv set up (#354). The app's own
  sessions are refused per caller (#358, `mcp::PYTHON_NOT_IN_APP`).
- It stops with SIGTERM first: the adapter stops marimo, which stops each
  notebook's kernel.
- marimo serves under `/marimo` itself (`--base-url`) and takes its token as a
  bearer, so the browser's cookie and authorization go no further. Its
  terminal and its file and home listings are refused: they reach the user's
  computer beyond the notebook. A page or WebSocket for a file that isn't an
  open notebook allowed to run is refused, since marimo would build its
  environment, which runs code (safe preview). marimo decodes the path before
  it routes it, and takes the last `file` of a query, so the core decodes
  both first, refuses a path that decodes to something odd, and allows only
  one `file`.

### Left to the plugin

- **The app quits** (`endeavor/leave_to_plugin`, app #232): if the plugin is
  in the runtime (`Notebooks::shared`), it is left running, marked as ending
  when idle so the plugin's fronts treat it as theirs, and ends once the
  plugin has been out of it for `LEFT_GRACE` (10 minutes): long enough for
  Claude Code to restart and open its notebook again. An idle limit of Never
  counts as 48 hours there (`LEFT_IDLE_HOURS`), since the user asked for it to
  stop with the app.
- **The app's helper leaves it running** (`endeavor/end_when_idle`, app
  #274): its notebooks go on until each is idle for the idle limit, and once
  none is open the runtime ends `LEFT_GRACE` later.
- **The app is back** (`endeavor/keep_for_app`): the runtime goes back to the
  lifetime it started with, and the watch ends (a generation number moves on),
  so no front of another build stops it as the plugin's own.

### Serving the port

- A thread per client; there are only a few, the helper's relayed streams. A
  failed accept doesn't close the port, and a pause keeps a lasting failure
  from spinning.
- `/mcp` has no server-initiated stream, and a session lasts as long as the
  runtime: DELETE doesn't end one.
- The runtime's own link (`/`) before anything asked for Julia doesn't start
  it, since that can mean a download: it waits for a click.
  `/?start-julia` is any other Pluto page as far as Julia goes, and a browser
  opening one starts Julia and sees how that goes until it's ready.
- A browser without the cookie gets a page naming `endeavor open`, since the
  link in a tool result has no token. On Windows the command is in
  PowerShell's form (`& "C:\…\endeavor.exe" …`), since a quoted path alone is
  a string there.
- Who may reach which route is in [one-port.md](one-port.md). Loopback isn't
  private on a shared machine, so the tokens keep other local users out, and
  a DNS-rebinding page sends a foreign Host.
- While the app can't reach the runtime, its listener answers one request
  and closes (`serve_unreachable`). The agent's MCP client gets an answer
  instead of a reset, which Claude Code counts toward giving up on the
  server for good.

## Next

1. **Ember and its adapter** (the build order in Ember's repository).
2. **marimo adapter** ([marimo.md](https://github.com/jowch/Endeavor/blob/main/docs/marimo.md) steps 2 onward, minus what the
   core provides).

## Open questions

- **Transport to adapters:** loopback HTTP (see "The engine interface").
  A marimo or Ember adapter whose engine keeps stdout clean could use stdio
  instead; the core would need a second `Upstream` for it.
