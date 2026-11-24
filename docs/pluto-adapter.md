# The Julia runtime and its Pluto adapter

`runtime/`: the Julia side, embedded in the binary and unpacked on first use
([front.md](front.md)). `boot.jl` starts it; `EndeavorRuntime` is the Pluto
adapter's side of the engine interface ([runtime-core.md](runtime-core.md),
"The engine interface"); `EndeavorRuntimeWarm` is a precompile workload;
`cache_guard.jl` cleans the compiled-file cache. `EndeavorRuntime` was seeded
from [PlutoMCP.jl](https://github.com/mthelm85/PlutoMCP.jl) (MIT) via the
jowch fork at 918e75d.

## Starting (`boot.jl`)

`endeavor core` runs `julia --project=runtime runtime/boot.jl <pluto_port>
<mcp_port>`, with `ENDEAVOR_TOKEN` (the bridge's bearer token, which the core
makes), `ENDEAVOR_STATE` (where to write its state) and `ENDEAVOR_LAUNCHER`
in the environment. Both ports are private to the core, which serves the
runtime's one port in front of them and adds Pluto's secret
([one-port.md](one-port.md)). Once Pluto and the bridge are up it writes its
state, which is how the core learns Julia is ready; it serves until the
bridge's `endeavor/shutdown` or a signal.

- stdout and stderr are `runtime.log`, which Julia buffers; the helper reads
  it for boot progress and a crash's last lines, so it flushes often.
- `cache_guard.jl` runs before anything opens the runtime's compiled files.
- The token comes from the environment, not argv (which `ps` shows every
  user), and is dropped so notebook workers don't inherit it. It goes in no
  file. A Slurm job's id goes in the state, so a reconnect finds the job.
- The state file is made private before Pluto's secret goes in, then moved
  into place whole, so a reader never sees half of it and nobody else can
  read the secret.
- `EndeavorRuntimeWarm` is left out of setup's precompile, which it would
  slow by about 30 s on Windows, and loaded only once precompiled. It isn't
  warmed up in a Slurm job, where the warm-up's Julia and worker would share
  the job's memory limit with the user's notebooks.

## The compiled-file cache (`cache_guard.jl`)

Included first by `boot.jl` and by the background precompile, in plain Julia,
before Pkg or EndeavorRuntime loads.

- A compiled file of the runtime's own packages that this user can't read
  makes every later start fail: `Base.isprecompiled` and loading throw
  "Permission denied", and Julia never compiles it again by itself (#372).
  Julia gives a new cache file its source's mode plus write, so one compiled
  while its source was gone is `--w-------`. Removing it (with its `.ji`,
  native image and, on macOS, `.dSYM`) lets Julia compile it again.
- A native image with no `.ji` beside it is never used, and Julia never
  removes it, since its eviction works from the `.ji` files (#382). It is
  removed once older than `ENDEAVOR_ORPHAN_AGE`, since a precompile renames
  its files into place one at a time.
- A compiled set whose source is gone can't be used again: each unpacked
  runtime folder compiles its own set, and Julia keeps up to 10 a package
  (#382). A runtime folder may be on one machine's disk while the depot is
  shared, as on a cluster with a per-node cache folder (#528), so a source
  missing here may be there elsewhere. Each start marks the sets whose source
  it sees (`<stem>.used`), and a set goes only when no start has marked it for
  `ENDEAVOR_UNUSED_AGE`. A mark whose set Julia evicted goes by itself. A set
  whose header names a depot-relative path is kept and not marked.
- A file that can't be removed (on Windows a loaded image can't be) is tried
  again at the next start without a word.

## The adapter (`Adapter.jl`)

The core calls `POST /adapter` with `{"method", "params"}` and reads
`result` or `error`. The methods: status, open, new, and for a notebook
snapshot, graph, shutdown, apply, run, interrupt, restart, allow_execution,
move, render_png and validate. Pluto's events reach the core on
`GET /notifications`, one `data:` line each: notebook_opened,
notebook_shut_down, file_saved, execution_done, cell_state (every cell's
code and run state: Pluto's hook doesn't say which changed),
topology_changed, run_finished (an unwaited run ended, with its `run` id and
the cells it finished) and process_exited (the notebook's process ended by
itself, with the cells that were running). Each carries `seq`, which `apply`
also advances.

### Staged edits

Cells the tools changed and haven't run are staged, with a topology that
analyses their new code (`current_topology`). Pluto's own topology stays as
of the last run, because a run compares it with the new one to know what the
edits changed, as a run from the page does. Analysing a staged edit into
`nb.topology` left that comparison empty, so a later run didn't run the other
cell of a conflict it fixed, nor drop a name it removed (L1 on Haiku).

- A run takes the staged cells downstream of its cells with it: Pluto
  analyses only a run's own cells, so a staged cell left out would be missing
  (a new one) or run with its old names still defined (an edited one).
- Runs Pluto starts itself see the staged cells too (#438): the page's run
  handler and the bond runs (`set_bond_values_reactive`, which runs readers
  with the topology as it is) are wrapped, once per Julia, so a staged cell
  they reach is analysed into the run, and a staged cell that reads a bound
  name only since its edit is among the readers. Bond runs are saved, unlike
  Pluto's.
- `apply`: Pluto's page diffs `cell_order` by reference, so each change
  assigns a new vector. A `set_code` with `expected` is refused before any
  op applies when the code isn't that.
- Pluto marks cells queued only inside its run task, after the package sync,
  so `run` marks them first, as the page's run button does. A run task that
  failed leaves those marks, so they are released.

### Runs

- A run that goes on is watched; its reply's `run` names it, and
  `run_finished` says when it ends. A run is over when its task ended, or
  its notebook's process is gone or the notebook left the session, which no
  task ending would say.
- A notebook's process is this Julia started again (Malt runs
  `Base.julia_cmd()`). juliaup removes a version it updates past even while
  it runs, and then Malt's spawn fails with ENOENT. What would start a
  process asks first and says why (`julia_removed`), and nothing running is
  stopped for a start that can't happen; the page's Restart does nothing
  then.
- A process that ended while it started leaves its failed start in Pluto's
  workspaces, and every later fetch rethrows it (#194), so it is dropped;
  the page's Restart (`restart_process`) is wrapped to do the same.
- Printing Pluto's start exception walks the whole notebook and never
  finishes, so it isn't printed.

### Asking before a new package (#397)

A run whose cells would make Pluto add a package the notebook doesn't have
(not in its Project.toml, not a standard library) waits for the user's OK
when the caller says new packages ask. Its cells stay queued and the
snapshot's `packages` says `asking`. `approve_install` is the OK, for the
packages the user was shown only (#418); one added since asks again. A
notebook that runs `Pkg.activate` adds nothing, so asks nothing.

- An open where new packages ask opens as a safe preview and leaves it
  through the gate, since Pluto's run on open adds every package the file
  uses.
- Cells that waited for an OK no longer needed (an edit dropped the package,
  or it came another way) are released and run with the next run, unless
  they ran since (#523). A snapshot releases them, so an edit unblocks
  without another call; it costs a topology pass per snapshot while one
  asks. A Stop drops them.

### Packages and registries

- Pluto's package step is reported as it goes: the part under way, the
  packages, how long, and the last line of its log with any `user:token@`
  taken out of a URL. It reads Pluto's internals, so a Pluto release that
  changes them reports nothing.
- Registries in the user's own depot, behind ours (a LocalRegistry, say),
  are updated where they are before a notebook opens, at most once per
  cooldown (#367): Pkg updates only the first depot's, ours, so a version
  registered from another computer was never seen. General, which we have
  too, is ours to update. Offline, the copy there is used as is.

## Stop (`Lifecycle.jl`)

On Julia 1.12 a Ctrl+C sent to a notebook's process lands on Malt's root
task, which waits on its socket and ignores it, so a waiting cell (`sleep`,
a download, a lock) never hears it. Pluto then presses Ctrl+C five times,
which ends Julia, so Stop on `sleep(30)` lost every value (#139). Julia
fixed this on master (JuliaLang/julia#62069).

- So the notebook's process notes each cell's task, and Stop sends that task
  an InterruptException, as Malt does in-process. A cell that never yields
  (a loop in compiled code) gets Pluto's Ctrl+C as before.
- This is a method on PlutoRunner's `run_inside_trycatch`, only on the Pluto
  releases `_stop_hook_ok` knows. It is a shortcut to remove (#567) once the pinned
  Julia's interrupts reach a waiting task.
- Only the cell Stop meant, and only right away: a late message mustn't stop
  the next run. A process with no hook answers false and falls back at once.
- The page's Stop (`interrupt_all`) is wrapped to do the same, once per
  Julia: it used to end a waiting cell's process, which read as a crash.
- A Stop while the notebook's process is still starting finds no cell
  running: the run is told to skip all its cells once the process is up, as
  it skips the cells queued after a Stop (#179; before, nothing was stopped
  and every cell ran).
- A process Stop's Ctrl+C ended isn't one that ended by itself; the next run
  starts a new one. Pluto marks a process it stops (shutdown, restart)
  discarded first.
- A notebook's process ending by itself is watched for: Pluto only notices
  when a call into it fails, so a process killed while idle, or by the OS
  while it runs, would leave the notebook "ready" and its cells running.

## Shutting down

- Closing the server waits for open connections, and event streams never end
  on their own, so they are ended first. Pluto's server and the bridge are
  closed before `exit`, forced, since under `exit` a connection being
  written to finds its socket torn down and HTTP.jl logs an error.
- `endeavor/shutdown` gives each notebook's process a few seconds to take its
  exit, then its SIGTERM, before Malt kills it. A process busy in a cell
  can't take the exit until it yields, and Malt's own stop waits 15 s each,
  one at a time: longer than the core waits before it signals Julia (#509).
  Pluto has saved every notebook, so nothing is lost.

## Warming up (`EndeavorRuntimeWarm`)

A cold first open spends about 25 s compiling Pluto's open-and-run path and
the adapter's (#16). The workload runs that path once (a new notebook's
first run in its worker, an edit and a run, an agent's reads, through
`adapter_call`), so the compiled code is cached; it saves a first open about
10 s.

- It is its own package so setup doesn't wait for it. Once the runtime has
  been quiet a while (`warm_up_later`: no notebook starting and no cell
  queued or running) it precompiles in a low-priority process (`nice`, or a
  lower priority class on Windows), and the next start loads it. The quiet
  starts when the runtime is ready, so where the app starts Julia early the
  warm-up runs while the user is still choosing.
- A failed workload is a warning, not a failed precompile. It must leave
  nothing running or the precompile waits forever: the notebook is shut
  down, a worker that never connected is ended, the pipes Malt leaves open
  are closed, the session's timer closed, and Pluto's throttle timers left
  to run out.
- It adds the notebook as `open` does without calling `open`, which renders
  text cells in this process, which can't happen while precompiling. Pluto's
  package manager can't run then either, so a cell that names `Pkg.activate`
  turns it off.
- A precompile that was ended leaves its temporary `jl_…` files, tens of MB
  each; ones an hour old are removed.

## Output (`Output.jl`)

- One output the snapshot can't summarize mustn't fail it, and with it every
  read (app #193).
- `view_cell_output` can usually turn PNGs and SVGs into a PNG; HTML and
  other images rarely have one, so `read_cell` doesn't point there for them.
  Pluto shows the richest format it can (SVG before PNG), so the worker is
  asked to re-render the value as `image/png`. An HTML output's embedded PNG
  (a widget's static render, a Masque.jl figure) counts if it is at least
  `_EMBEDDED_PNG_MIN` bytes; smaller ones are icons. `@bind` makes a cell's
  value a `Bond`, whose widget may have a picture of its own.
- HTML is interactive when it has a script, a canvas or a control (`<bond>`
  is `@bind`). `view_cell_output` then tells the agent it hasn't seen it work
  (#393).
- Big outputs and PCRE: a lazy pattern that has to cross megabytes (a large
  data URL, a plot's script) fails with "match limit exceeded" (app #193), so
  tags are found with short patterns and plain searches. `[^<>]` means a
  stray `<` costs a scan to the next tag; a tag whose attribute holds a `<`
  leaves its start in the text, which is rare. Entities are decoded in one
  pass, so `&amp;#123;` stays `&#123;`. Pluto's Markdown writes `{`, `+`
  and `=` in math as `&#123;`, `&#43;` and `&#61;`.
- A worker running cells answers only when they yield, which may be minutes
  (#62), so a render waits a while, not for the run. A render that timed out
  stays queued there, so a retry joins it.
- A text form is the value's `text/plain` display from the worker, not asked
  while cells run. A value that shows as HTML but has only the generic
  `show` reads better as its stripped HTML.
- What a cell printed and logged comes back as notes (#257): a `@warn` the
  agent never sees lets it report a result as clean. Debug and progress logs
  are left out, colour codes removed, and a line rewritten with `\r` keeps
  what it shows last. It reads Pluto's internals, so a Pluto release that
  changes them gives none.
- Cells held for an OK are queued but don't run (#506), so they don't count
  as running.

## Controls (`Controls.jl`)

A notebook's `@bind` controls, as `read_cell` shows them under `inputs`, and
a note on the cells whose results used a control moved from its default. A
control's value lives only in the notebook's process and isn't saved, so an
agent that reports a result made at a moved value reports numbers a rerun
won't give unless it is told.

- The describing code runs in the notebook's process, where the widgets are.
  A widget's own text form is used where its package gives one (Masque's).
  What a control allows is read after `transform_value`, since the page's
  own values are indices. A widget with no default never counts as moved.
- The process is asked only when something that could change the answer
  changed (its process, its cells' last runs, the page's values), and only
  while no cell runs or waits: a busy process may not answer for a long
  time. Only the call that starts a query waits, briefly; until it answers,
  the last answer stands. A failed query counts as asked for its key.
- The key covers every cell's last run: a value the page sends is in
  `nb.bonds` before the run that sets it starts, so a query in that moment
  reads the old value, and the readers' runs then change the key.

## Saving (`Saving.jl`)

Pluto saves a notebook in place, so a process killed during the write left
the file cut short (#102). A more specific method of Pluto's save writes the
content beside the file and renames it over, so the file is either the old
notebook or the new one. Permissions are kept, a symlink keeps its link, and
a file with hard links is written in place so its names stay one file. Where
that can't be done (a folder it can't write beside, a rename Windows refuses
while another program has the file open), it writes in place as Pluto does.
Staged copies a killed process left are removed once older than one write
could take (#469), once per folder per process.

## The bridge (`Server.jl`)

Only the core calls the bridge, and it checks Origin and Host first, but
loopback isn't private on a shared machine, so every request needs the bearer
token, compared in constant time. A request's body is read before the reply,
as HTTP.jl's stream contract asks. The bridge also serves Pluto's editor as one
page with its code inside (`offline_bundle`), for a view that can load nothing
from outside, made from the Pluto installed so it always matches the server,
with no Binder button. The view sets `window.endeavor_notebook_id` before the
page's scripts run. New notebooks' "Save notebook" box suggests the session's
folder instead of Pluto's scratch folder. Pluto's "Ask AI" and "Fix with AI"
are turned off; the app adds its own agent's buttons.
