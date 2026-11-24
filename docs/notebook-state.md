# What the core keeps about each notebook

`notebooks.rs`: the state the core holds beside each open notebook, which
the notebook tools ([notebook-tools.md](notebook-tools.md)) and the app's
`/endeavor/events` stream read. The engine's adapter answers calls and sends
notifications ([runtime-core.md](runtime-core.md), "The engine interface");
the core reads a fresh snapshot each time it tells the app anything. A
notebook's state goes when it shuts down, except its entry in `idle_stopped`.

## Per cell

- **Who last changed it.** Each cell's code, hashed, as last seen, and who
  made it so: `agent`, `user`, or nobody if it hasn't changed since the core
  first saw it. Code that changed without the agent's tools writing it was the
  user's. Each state is seen as it comes, so an edit made and undone between
  two events still counts as the user's.
- **The agent's last edit's `seq`.** A snapshot or notification numbered below
  the `apply` of the agent's last edit of a cell may still show the code from
  before, so it doesn't change who last changed it.
- **Before.** Each unrun cell's code before the agent's first edit since it
  last ran (`""` for a cell the agent added), for the app's in-editor diff.
  Forgotten once it runs, or once it no longer differs.
- **Pending.** Cells edited through the tools and not run since. A cell stops
  being pending once it runs, however it runs (the tools, Pluto's run button,
  a reactive re-run). A cell Pluto's page or a reload removed stays pending
  until a run prunes it. "Ran since the edit" is a run that started after
  it: the engine stamps a run when it ends, so a run already under way at the
  edit ends after it, and its duration gives its start.
- **Edited, and reached by another run.** When the tools last changed each
  cell, kept until the tools run it: a cell that ran since ran the agent's
  code some other way (the user's run reached it), so an approved run of it
  needn't run it again. The same goes for cells the user's run reached after
  the page asked first (Run anyway).
- **Reads.** What each session last read of each cell: its code, the
  engine's `seq`, and the session's call count then. Kept per session, so one
  session's reads and edits aren't another's.
- **Queued.** Pluto marks every cell `queued` when it loads a notebook, ahead
  of the planned run, and in safe preview that run never happens. So a queued
  cell counts only while the notebook runs something or installs the packages
  its run needs. Cells held for the user's OK to a package aren't running:
  they wait, queued (#506).

## Runs and Stop

- The runs sessions started that haven't ended (`run_finished`), with their
  cells that hadn't finished when the tool answered: what that session's Stop
  interrupts. A run the engine names goes on, with the cells that depend on
  it, until its `run_finished`; one it doesn't name goes on while one of its
  own cells hasn't finished.
- The dependents a run will rerun count too: the engine queues one only once
  its parent's result is in, so until then it looks idle (#192).
- A `run_finished` can come before the tool that started the run recorded it
  (a quick run), so the last 64 ended runs are kept, and such a run isn't
  recorded after.
- A run the tools didn't wait for that ends leaves its cells as staged as
  they are, so an edit made after a cell finished but before the notification
  came stays staged (#231). When a notebook's process ends, no run started
  before it goes on; a restart's run is no session's.

## Sessions and their notebook

- **One notebook per session.** The app binds a session started from an
  existing notebook; otherwise the first notebook it opens or creates binds
  it. Other notebooks stay readable. A notebook that is no longer open is not
  one it works on, unless the session closed it itself with `close_notebook`:
  then it stays bound to it, so closing isn't a way round the rule (#520),
  until it opens it again or the app binds it anew.
- **Several notebooks** (`allow_several`): a client that serves several
  conversations on one connection (Claude's desktop app), which the server
  can't tell apart. Each notebook it opens or creates is one it works on; it
  still can't change one it didn't open, and `list_notebooks` marks none as
  the session's own.
- **Records.** When each session last made a tool call, whether from the app
  or the plugin, and through which front. A session's record and binding go a
  week after its last call. A plugin session whose front didn't say which
  process it is counts as still in the runtime for 30 minutes after its last
  call (`Notebooks::shared`, app #232).
- **Kind.** A session's notebook kind, when its client said it, is the engine
  of a `new_notebook` without a path.
- **Auto mode.** A run in the app's Auto mode, or Claude Code's own, installs
  a Julia notebook's new packages without asking (#397). Another plugin
  client's auto mode isn't known here, so it asks.

## Opens that outlast their call

The first notebook in a new Julia process compiles for a while, so an open
or creation could outlast what an agent's call waits. Then it goes on, and
the call answers `notebook_opening`. The same call again (same key) waits for
it and gets its result, kept for 10 minutes. An open a session stopped
waiting for makes that session's notebook even if no call collects it, since
an agent may take the id from `list_notebooks` instead (#333).

## The events stream

`/endeavor/events` sends the state now, then after each change:
`notebooks` (the `list_notebooks` summary), `cells` per notebook (`running`,
`errored`, `unrun`, `author`, `before`, `version`, `name`), `idle_stopped`,
`engine_ended`, `asks` and `build`.

- `version` is an FNV-1a hash of the code as the engine keeps it (Ember drops
  trailing blank lines and turns CRLF into LF); the app only compares them.
- The snapshots are taken before the state is locked, so a notebook made
  meanwhile is missing from them though a tool may already have recorded
  reads of it. Each use of a notebook's state counts as touching it, so what a
  tool recorded meanwhile stays (#376). A notebook closed after the snapshots
  has no graph and is left out; before, the whole event failed with a 502.
- Events go out in order, under one lock, once per burst of notifications.

## Idle stop

Notebooks idle for the limit (48 hours unless the caller sets another; no
tool call, no save or run, no cell running) are stopped, except kept-alive
ones, and listed in `idle_stopped` until they open again, so the app can say
why. The stop is recorded first, so the event after it carries it. The check
runs every five minutes.

## The app's calls

- **`restore_cells`**: the app's Restore on a turn's changed cells. All or
  nothing: if any cell's code isn't what the turn left (`expected`), or the
  cell is gone, nothing changes. Only an explicit null deletes a cell the
  turn added. Edits go first, since the engine checks every `expected` before
  it applies any op. Nothing runs, except Pluto's cleanup of what a deleted
  cell defined. The restored code is the user's; a restored cell whose code
  isn't what last ran is pending, so it shows unrun, and an agent reads it
  before running it.
- **`move_notebook`**: rename or move a notebook's file, never over another
  file. A notebook keeps its engine (a Julia notebook ends in .jl, an R one in
  .R). Bound sessions and an `idle_stopped` entry follow it.
- **`export_notebook`**: a Python notebook's Share and export. `file` is the
  notebook's file as it is on disk. `html` is marimo's own web page export,
  made in the notebook's marimo session, so it needs the notebook out of safe
  preview. Julia and R notebooks export through their own page instead.
- **`environment`**: what a Python notebook runs on, for the app's Packages
  view, since marimo's page shows none of it. It gives the header's Python
  requirement and the Python uv picks for it, the direct packages with their pinned versions, the package
  date, the install that waits for the user or is running, and the last
  install that failed, with marimo's log. A failure stays until the next
  install works. Julia and R notebooks read this from their own page.
- **`approve_install`** covers only the packages the user's yes named that
  the call asked for, so a yes never widens what was asked (#418). Names
  compare as PyPI's do (case, `-`, `_`, `.`), and the names that go on are
  the ones the user saw.

## Ids and paths

- A cell id is a UUID in the one form Julia parses (36 characters), or a
  short Ember id from a hand-written file (`# %% id=B`: 1 to 64 of
  `[A-Za-z0-9._-]`, Ember #82). A JSON number is taken as Julia's `UUID`
  takes it.
- Notebook paths compare as the Julia runtime compared them: absolute, `~`
  expanded, and resolved through symlinks as far as they exist. A relative
  path is taken from the session's folder, in one place. A session that was
  told it has no folder can't use a relative path, or a `new_notebook`
  without one. On Windows `\x` and `/x` are rooted on the working folder's
  drive, which such a session doesn't know, so only a drive or `\\server`
  path counts as absolute there.

## The engines (`notebooks/engines.rs`, `notebooks/r.rs`)

Each engine joins when it starts, Pluto's too. How calls are routed is in
runtime-core.md ("Processes"); what the code adds:

- A notebook's engine is learned from every `open`, `new` and `snapshot`
  reply, which a notification stream that skipped `notebook_opened` can't
  lose. An id no running engine reported is "not open", not "Julia isn't
  running". A path's engine is known by its name alone, since the file may
  not exist yet.
- `snapshot` or `status` of every notebook: Pluto failing fails the call.
  Another engine failing is left out, so a broken R never hides the Julia
  notebooks; one that doesn't answer at all is dropped with its notebooks.
- When an engine other than Pluto's is dropped, each closed notebook's record
  is written before it leaves the engine's list, so a call that no longer
  finds it open finds why it closed (#357). The record's `ended_at`, with the
  path, tells one ending from the next, so the app sees an ending after a
  reopen as new even when both fall between two of its events. Two callers
  that both saw the engine fail drop it once.
- httpuv can't stream a response, so R's adapter's notifications come by
  long-polling `GET /notifications?after=<seq>`, which the core turns back
  into a stream of `data:` lines, and reads every notebook again when a
  stream begins.
