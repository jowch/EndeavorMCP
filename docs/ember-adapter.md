# R's adapter and Ember's install

`runtime/r/`: the R side of the engine interface
([runtime-core.md](runtime-core.md), "The engine interface"), as
[pluto-adapter.md](pluto-adapter.md) is Julia's. `install.R` installs Ember;
`adapter.R` runs it. How the core starts them is in runtime-core.md, "R".

## Starting (`adapter.R`)

The core runs `Rscript --no-save --no-restore runtime/r/adapter.R` with
`ENDEAVOR_TOKEN` (the bearer token every request carries), `ENDEAVOR_R_STATE`
(where to write `{pid, bridge_port, ember_port, ember_secret}` once it
listens) and `R_LIBS` (where Ember is). It runs one Ember server holding every
notebook it opens, and beside it a bridge on 127.0.0.1:

- `POST /adapter`: `{"method", "params"}`, answered with `{"result"}` or
  `{"error"}`.
- `GET /notifications?after=n`: every notification numbered above `n`, as a
  JSON array, held up to `POLL_HOLD` (25 s) while there are none.

The state file is written whole or not at all, since the core polls for it.
Requests need `Authorization: Bearer <token>`, compared without stopping at
the first difference.

- The token is read once and taken out of the environment, so code a
  notebook runs can't pick it up, as boot.jl does for Pluto's processes.
  Ember's page secret is made by the core, which already knows it, so it
  isn't written back, and it isn't left in the environment either, which
  the notebooks' R processes would inherit. `test.R` checks a notebook can
  read neither, nor the state file's path.
- Notebooks run in this R, with the user's settings. The core has R read an
  empty profile and names the user's in `ENDEAVOR_R_PROFILE` (empty for
  ~/.Rprofile; unset for Endeavor's own R, which reads none). The adapter runs
  it into the global environment, as R would have, so an error in it is
  logged and R carries on instead of stopping before the notebook opens.
  If the profile set `.libPaths()`, the libraries `R_LIBS` names, where Ember
  is, go back in front.

## Methods, errors and notifications

The methods and their replies are Adapter.jl's (`adapter_call`): status,
open, new, and for a notebook snapshot, graph, shutdown, apply, run,
interrupt, answer, restart, allow_execution, move, render_text, render_png
and validate. One is R's own: `page_bundle`, the notebook view's page.

Errors read as Julia shows them, which the core parses:
`ArgumentError: kind::message`, or `KeyError: key "kind::message" not found`
for a notebook or cell that isn't there (`unwrap_key_error` in mcp.rs). Any
other R error is `r_error::message`. A call's reply is a value or a promise of
one, never an R error.

The notifications are Adapter.jl's too, each with `seq`, a counter that
`apply` also advances; `apply`'s reply, `snapshot`'s and `status`'s carry it.
Ember's own per-notebook `seq` is not used. Everything shares Ember's one R
thread, so notifications stay small: `cell_state` names only the cells whose
code changed. Ember's `packages_changed` and `worker_usage` events are
nothing the core follows. A notebook's R ending by itself (or failing to
start) is `process_exited`: Ember stops it and clears its results, and the
core hears which cell was running.

## The snapshot

- Process states are Pluto's, as near as Ember's come: safe preview is
  `waiting_for_permission`; a busy R is `ready`, as a Pluto process running a
  cell is; an R that ended or never started is `no_process`, though Ember
  starts a new one on the next run.
- An error is in Pluto's shape (`{kind, msg}`); R's own `error` is Pluto's
  `runtime`. It names what failed, as R's "Error in <call>:" does: a code
  cell's call, or a text cell's `` `r expr` `` span, so the agent can tell
  which part of the cell to change (#288).
- A package the code doesn't name: Ember's fix is to add it to the header's
  `[extra_packages]`, which no tool does and an agent mustn't do by editing
  the file. Naming it in the cell does the same through the tools, so that is
  the fix the agent hears; Ember's page keeps its own wording. Ember gives
  that fix for a missing package, and for a run error whose message names a
  package the code can't show (Ember #184, such as tidytext asking for
  textdata).
- The output as text is what the cell printed, then its value's print form.
  Plots aren't text: like Pluto's, their text points at `view_cell_output`.
  A warning reads as R prints one, so it isn't taken for the cell's output.
- A text cell's output is its prose without the `#'` prefixes, each `r expr`
  span replaced by its value, as the page shows it. Spans are found with
  knitr's inline-code pattern, as Ember matches it (`text-cells.R`), one line
  at a time; a span inside double backticks counts too. Spans stay as written
  while there are no values for this code (not run yet, or edited since), so
  the prose still reads (#270).
- Pluto stamps a run when it ends; Ember records when it started. Ember has
  no boilerplate cells to hide.
- Ember says itself when a result is stale: made before an ancestor last ran,
  or from code the cell no longer has (an edit on Ember's page that hasn't
  run, #291), or not run since the notebook started (a restart leaves every
  cell so). A disabled cell, or one downstream of it, won't run (#232).
- A running cell waiting on `readline()`, `menu()` and the like (Ember #186)
  shows what the prompt printed and, for `menu()`, its choices; `answer`
  replies. Its output is what this run printed so far, where a question's
  own text (a license, a menu's title) usually is: Ember keeps the last run's
  result, error and notes on a running cell, and they say nothing about the
  question (trial 20).
- A cell's inputs (emberui, Ember's `docs/inputs.md`) show as read_cell shows
  them: "n: slider 10 to 500, step 10, value 250 (default 100)". An input
  that isn't bound says why, as the page's greyed widget does. Only the input
  on the cell's last line is its output, the widget the user can move. An
  Ember from before #446 doesn't report `output`, and then nothing is said.
  A number reads as JSON writes it (5, not 5L), and an `input_list()` part as
  `list(n = 3)`; anything else as R prints it.
- A note goes on each cell whose result used an input the user moved off its
  default (on the page or with `set_input`): a rerun of the file uses the
  default, so the agent shouldn't report the result as the notebook's. The
  input's own cell has the default in its `inputs` line instead. A reader
  that shows `stale` didn't run with the value yet, so it gets none. When
  every input is at its default, the usual case, the graph isn't built.

### Notes

The notes Ember's page shows under a cell, from its last run or its code,
worded as the page words them (Ember's `lang/english.json`), each
`kind::text` like a tool warning (#207):

- `changed_in_place`: the run changed, by reference, a value another cell
  defines, so the cells that read it didn't rerun.
- `settings_found`: the run found a global setting, so the cell became a
  settings cell. `load_notes`: a package it loaded reset a setting a settings
  cell sets.
- `from_lock`: the cell's install calls didn't run, since the lock answers
  them (Ember #79); `""` stands for a call that names no package.
- `untracked_reads`: the cell reads globals through `get()` on a computed
  name, `eval()` or a function that does, so it doesn't rerun when they
  change (Ember #357, #365). It comes from the code, so it shows before the
  cell runs too.
- `untracked_files`: a reader on the last run got something that names no
  file Ember can watch, such as a connection of a kind it doesn't know, so
  the cell doesn't rerun when that file changes (Ember #380, #513).

### Packages

Ember's package work is given in the shape Adapter.jl's `package_step` gives
the core, or none when there is none: while queued cells wait for packages
(`waiting_for`) or the library installs. The core then counts queued cells as
running, and `read_cell` waits and tells the agent what is going on. A cell
that isn't queued may name a package that isn't installed; only a run makes
it wait.

- `packages` are the ones the cells wait for, `seconds` how long since this
  adapter first saw the work, and `last_line` the installer's last line, or
  the package it is on.
- Step `asking`: the install stopped before a large download and waits for
  the user's OK (Ember #37, `approve_install()`). `download` then gives the
  total `bytes` and each `package`, `version` and `bytes` (null when its size
  isn't known), largest first.
- While installing, `source_builds` names the packages ("name version") that
  build from source for want of a binary for this R (Ember #19, the target's
  `no_binary`).

## The graph

Ember's graph always reflects the current code, staged edits included, so
`fresh` and `refresh` change nothing. `functions` are the names a cell
assigns a function to; `definitions` the rest, learned ones included.

## Edits

Pluto's ops count from 0 and Ember's from 1. A `set_code` with `expected` is
refused before any op applies if the cell's code isn't that. Ember saves the
file and updates its page itself, and trims trailing blank lines from code
(and turns CRLF into LF). Ember folds a new text cell and unfolds a new code
cell, and can't name a cell inserted in the same batch, so a fold asked for
otherwise is a second edit. Two deletes in one edit fail in Ember once the
cells have run (Ember #637), so `test.R` deletes one per edit.

## Runs

- A run that goes on gets a name, as Pluto's adapter gives one: the core
  counts the run, its dependents included, as going on until `run_finished`
  names it (#151) and says which of its cells finished. A waited run of an
  upstream cell returns once its dependent has run too, as Pluto's does.
- A run covers, in autorun, the cells downstream of its own, which Ember
  queues as each result comes in (in the same step, so a run never looks
  over between a cell and its dependents), and its readers. In lazy mode
  they don't run.
- The readers are the cells that read a name the last run of a run's cells
  defined, and those downstream of them. When a run drops a name (a rename),
  Ember reruns its readers too, and they fail as in plain R, but the graph no
  longer links them, so the run would look over before they ran (#228). They
  are read before the run's results come in, while `results` still holds the
  old names. A cell that has never run, or a disabled one, isn't rerun. The
  run's own cells are left out; a reader Ember already queued with them, as
  in a definition clash, is kept, so the core reports it. A cell Ember skips
  (one in a definition clash) loses its names in `run_cells()` itself, which
  queues their readers there, so the readers can't find those.
- The core adds the readers to the graph's dependents, for `will_run` and the
  cells a waited run reports. Ember's queue also holds the ancestors it runs
  first, which the core leaves out.
- A run is over when none of its cells or their dependents is left, its R
  ended, or the notebook shut down. A run waiting on a prompt runs nothing
  until it is answered or interrupted. An install that stopped before a
  large download, with nothing running, moves only on `approve_install()`,
  so a waited run returns rather than wait it out.
- `run` is refused in safe preview and for a read-only notebook: Ember would
  allow execution on a run, and Pluto (and the user's approval) wants
  `allow_execution` for that. With `wait`, the reply comes once the run is
  over, a cell of it waits on a prompt (`prompted` names it), or `timeout`
  passes (the cells still to run are then `timed_out`). A run of no cells is
  Pluto's cleanup after a delete, which Ember doesn't need.
- `allow_execution` is Pluto's "Run notebook code" for safe preview: it runs
  every cell (`run`) or leaves them to start on the next run, and names the
  run it started (#170).
- Restart is Ember's: a new R, every cell left not run (Pluto's runs them
  all). It is refused in safe preview, where `allow_execution` starts the
  notebook.

## Moving the package date

- The date a call names, or today in local time, as the Packages tab's
  Update button does (`ember_update_packages`).
- `preview_packages` is Ember's `preview_date()`: what would change, with
  nothing changed yet. It may fetch that date's index first, so it is waited
  for until it is ready or failed or `timeout` passes (then `status` is still
  `fetching`). A day Package Manager hasn't published yet (today, early in
  the day in UTC) is previewed at the newest day it has: the proposal's
  `date` is that day and `asked` the day asked for (Ember #162, #221), so
  either one finds it (#247), and applying the day asked applies the day it
  moved to, which Ember's `set_date()` takes.
- With `package`, the date is Ember's for putting that package at `version`
  (CRAN's newest when null): `preview_update()`, the Packages tab's row
  Update (#266). The other packages move with the date, as far as they must.
  It waits for CRAN's release dates first (`dates`). A call again for the
  same package and version, while its preview is being made or is ready,
  waits on that one rather than starting over; a different ask (a pin, then
  the newest) starts a new one. Ember replaces a newest's null with the
  version it resolves before the preview is ready, so only an explicit
  version is checked against the proposal's.
- A package the code loads that the lock doesn't have yet (the notebook has
  just opened, or a cell added it) isn't refused: Ember's preview waits for
  the lock, and fails saying why if the lock never gets it.
- The reply counts the notebook's packages: with none, nothing changes but
  the date (#269). They are counted from what the code and header name and
  what the lock holds, not `package_status`, whose table is empty until they
  resolve. An Ember that doesn't say gives no count.
- `set_package_date` is Ember's `set_date()`: it applies the preview for
  `date`, refused unless it is ready and current and matches what the agent
  was shown. Ember redoes its proposal when the notebook's packages change,
  so it can differ from what `preview_packages` showed; then this refuses
  too, and the agent previews again (#201). The library installs, and a
  worker with a changed package loaded restarts once it is ready, with its
  cells left not run.

## Inputs, installs and prompts

- `set_input` is Ember's: a new value for the input bound to a name. The
  worker checks it against the input's spec; a value it refuses is an error
  that says what the input allows. In autorun the cells that read the name
  rerun, as a run of them, waited for as `run` waits; in lazy mode they are
  only marked stale. Either way the input's own cell doesn't rerun.
- R reads the value only once a running cell finishes; if it hasn't by the
  deadline, the value is still on its way, so the reply says it is pending,
  not refused. The call's value and outcome are found by the token Ember gave
  the value, as its own `set_input()` waits: the value with the worker is
  this one, since Ember refuses an R value while another is with it. Past
  this call, the worker may hold a page value, and the outcome may be
  another caller's.
- A value from JSON, which has no integers and no vectors, is made the R
  value the input means: a dropdown's or radio's choice compared as a number
  when both are numbers (JSON's 2 is the choice 2L or 2), and a JSON array as
  a vector (a multiple dropdown's choices, or one choice that is itself a
  vector). emberui compares numbers by value too; this is a backstop for an
  older emberui, which compared with `identical()`.
- `approve_install` is Ember's `approve_install()`: the user's OK for an
  install that stopped before a large download (Ember #37). It is refused
  when no install waits for one.
- `answer` is Ember's `answer_input()` for the prompt a cell waits on, which
  lets it go on. With `id`, only if it is still that prompt, not one asked
  since.

## Opening and outputs

- A new notebook goes at `path`, else is named `notebook.R` (then
  `notebook 2.R`, ...) in `folder`, else in the working directory. Like
  Pluto's, it opens allowed to run, and its cells come back so the caller can
  edit them.
- The snapshot's `output` already holds every output's text form, so
  `render_text` has nothing more to render.
- jsonlite's base64 breaks the line every 72 characters. The core counts
  every character as data (`base64_len()`), and a strict client may not
  decode it, so the breaks go (#208).
- HTML is interactive as the Julia runtime decides it (`_INTERACTIVE` in
  Output.jl): a script, a canvas or a control. An htmlwidget (plotly,
  leaflet, DT) brings its data in a script. A picture of such an output
  can't show that it works (#398).
- `page_bundle` is Ember's live editor as one page with its code inside, for
  the notebook view in Claude's desktop app, which can load nothing from
  Ember's server (`views.rs`). The view sets `window.endeavor_notebook_id`
  before the page's scripts run, and relays its WebSocket to Ember itself.
  The same page serves every notebook: the notebook id only routes the call
  to R. An Ember from before `view_page()` can't make one: it updates when R
  next starts.

## Installing Ember (`install.R`)

The core runs `Rscript --vanilla runtime/r/install.R <folder> <repository>`
each time it starts R for an R notebook, before the adapter. CRAN is
`ENDEAVOR_CRAN` when set.

`<folder>` holds one library per Ember build, named by the first 12
characters of the SHA256 the repository lists for the file (r-universe's
`PACKAGES` has no commit; the installed package's DESCRIPTION has it, as
RemoteSha); `deps`, a library with the packages Ember needs that R lacks or
can't load; and `ember.dcf`, which names the build in use (Current), the one
before it (Previous) and the builds that failed to install or load here
(Failed). The adapter runs with `R_LIBS=<folder>/<Current>:<folder>/deps`.

It exits with 0 when Current is ready: updated, already the newest, or an
Ember is installed and the newest couldn't be reached or downloaded; with 3
when the newest build failed to install or load and Current is the one
before it; with 1 when there's no Ember to use.

- The newest build comes from the first place that lists one: a binary where
  r-universe has one for this platform and R, else the source package. Its
  row of the index is what the download uses too, so it gets the build
  chosen here even if the index changes meanwhile. A repository that didn't
  answer is one R only warns about.
- Without the developer tools, r-universe's source can't be built here. When
  its binary can't be reached and an Ember is installed, trying the source
  would only fail and say the newest Ember didn't work, so the installed one
  is kept.
- Where outbound connections hang rather than being refused, as on some
  compute nodes, each place waits out the timeout, and R would start that
  much later each time. So when none answered within the last hour,
  `unreachable` says so, and with an Ember installed R starts with it without
  asking again (#90).
- A failure says whether it is this build's fault (`blame`): then it isn't
  tried again until there's a newer one, and the core tells the agent.
  Otherwise (no network, or a package Ember needs didn't install) the next
  start tries again.
- Ember's dependencies, all the way down, that this R can't load go into
  `deps`, which comes first, so a broken or old copy elsewhere is shadowed:
  missing ones, ones in a library built for another R, and ones that load but
  are older than Ember or a package in the tree needs (#90), since
  `install.packages` brings a newer one along only for the packages it
  installs. A package's minimum is what Ember and the tree, as CRAN lists
  it, ask for with `>=` or `>`. A version is read from the
  DESCRIPTION R would load, since a namespace loaded here may be an older
  copy than the one just installed.
- Whether they load is checked in a new R with the library path the adapter
  will have, not in this process: the checks above loaded whatever copies
  they found, and an old one stays loaded even once a newer copy is in
  `deps`, so a package that needs the newer one would fail here but load in
  the adapter (#216). R's reasons go to stderr. An R that ended before it was
  through counts every package as failed.
- The build is downloaded first, so a download that fails isn't counted
  against it, and checked against the SHA256 the index lists, so it isn't
  one cut short or changed on the way (`tools::sha256sum` came with R 4.5;
  before it, coreutils' `sha256sum`). Another start may have installed it
  first.
- The build that stops being Previous is dated from then. Builds that are
  neither Current nor Previous go a week after they stopped being either,
  since a runtime started before then may still have one loaded, and
  unfinished installs go once a day old.

## Tests (`runtime/r/test/`)

- `test.R` runs the adapter end to end: it starts `adapter.R` as the core
  does, then drives it over HTTP (`Rscript runtime/r/test/test.R`, from
  anywhere). It needs ember, processx and curl, and uses a temp folder, cache
  and data folder, so it never touches the user's own. Every notebook sticks
  to base R, so nothing is installed.
- `unit.R` checks the adapter's pure functions with no Ember and no server
  (`Rscript --vanilla runtime/r/test/unit.R`, from anywhere): only the
  definitions it names are read out of `adapter.R` and run on hand-made
  values, so the rest of the file (libraries, the server) never runs. e2e_r
  runs it. The user's profile check runs it into the notebook's environment:
  Rscript stops on an error when R reads the profile itself.
