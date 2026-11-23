# The notebook tools' rules

`notebooks/tools.rs`: every rule the agent's notebook tools follow. The core
applies them and the engine's adapter carries them out
([runtime-core.md](runtime-core.md), "The engine interface"). What the core
keeps between calls is in [notebook-state.md](notebook-state.md).

Results and errors are what the Julia runtime gave before, to the byte, with
two exceptions: lists Julia kept in hash tables now come in notebook order,
and a wrong argument type no longer comes with Julia's stack of candidate
methods. An error is the text of the exception Julia raised;
`ArgumentError: kind::message` names its kind (`mcp::tool_error` reads it
back). A few of Julia's own messages are kept for this reason: `KeyError`,
`UUID` parse errors, the Bool errors, and `StringIndexError` from the search
tools, which cut 40 bytes either side of a match by byte index as Julia did.

## How long a call waits

- `WAIT_SECONDS` (45 s) is the most a call waits for the user's answer and
  then a run, counted from when the call arrived. It sits under the 60 s that
  Claude Code (by default, whatever progress it hears) and Codex give a tool
  call. A wait for the user's own run of the same cells (`ran_after_user`)
  comes out of it.
- A run always gets at least `WAIT_FLOOR_SECONDS` (5 s), however much of the
  call went before.
- A caller that hangs up ends a wait early.
- A wait that runs out with the cell still going says so (#203). An agent
  can't tell how long a call took, and otherwise reports that the read didn't
  wait.
- A prompt the run waits on (`readline()`, `menu()`, #261) ends the wait at
  once, and so does an install waiting for the user's OK: neither goes on by
  itself, so waiting longer only spends the call. A run waiting for an OK
  didn't run out of time, and its message says what it waits for.
- `read_cell` waits, within the call, for a cell waiting on a package step
  (an agent that reads it at once sees no change and gives up long before a
  first install ends) and for a cell this session's run is running or will
  run (an agent that reads it at once reads it again, each read a whole
  request of its context). It doesn't wait for the user's run or another
  session's, for a cell behind a prompt, or for a queued cell whose install
  is held for the user's OK. A cell busy once the session's runs ended is
  another run's. A read nobody will see doesn't count as one.
- A package step past `INSTALL_USUAL_SECONDS` (10 min) tells the agent it is
  taking longer than usual, and to keep waiting only while the package log
  moves.
- A submit with nothing staged still waits for a run this session started
  (its dependents, say) rather than say it completed while it runs (#151).
  Its receipt says "errored" if a cell that ran meanwhile did (#171).
- An edit's `run_after` and the open's run are not waited for when the call
  says so: a long run would hold up the agent's other calls.

## Reads and writes

- An edit needs the session to have read the cell's current code. A read
  whose reply left the code out still counts before an edit, but not as the
  agent having the code: otherwise a read every few calls would keep code it
  lost to a compaction "unchanged" for good.
- `read_cell` leaves out code this session read or wrote within its last
  `RECENT_CALLS` (10) calls, unchanged since (`code_unchanged`). An agent that
  reads each cell it just ran otherwise gets its own code back each time, a
  tenth of a long session. An older read may have left the agent's context
  (a compaction, cleared tool results), and an agent told it has code it lost
  edits from memory. The reply names the code it means, so the app keeps its
  copy only if it is that one.
- A first read of a cell another session changed within
  `OTHER_SESSION_RECENT` (10 min) says so (#108). Otherwise the reader takes
  the other session's fresh edit for the notebook as it is, and replaces it
  with neither told. A session that had read the cell hears it through
  `changed_since_read` instead.
- A read of a cell someone else changed since this session last read it (the
  user, another session, or this conversation before it was resumed,
  `CHANGED_BY`) gives what the session last read (#504). An agent that read
  the new code without being told it was new replaced it without a word to
  the user. A snapshot the engine took before the cell's last edit through
  the tools still shows the old code, so it doesn't count; an engine that
  numbers its snapshots (`seq`) says which those are, and with one that
  doesn't, a cell whose last change was this session's counts as unchanged.
  The app's calls (no owner) are never told either of these.
- A change the user made through the app (`endeavor/restore_cells`) is owned
  by `USER`, no session.
- A run that targets cells another session changed since this one last read
  them, or cells they depend on, is refused as `run_conflict::`. An edit's
  `run_after` that conflicts leaves the edit staged.
- The engine may keep code in another form (marimo's file form). The edit's
  reply then gives the code as kept (`codes`); Pluto and R keep it as sent,
  and the reply leaves it out, since the agent and the app already have it.
- `code_sha` is the first 12 hex digits of the code's SHA-256, the same as
  the app's `celldiff::code_sha`.
- `read_notebook_code` starts each cell with Pluto's `# ╔═╡ ` or Ember's
  `# %% id=`. An Ember file's text cells are already `#'` comments, so they
  get no `# md:`.

## Runs

- A run runs the cells its targets depend on that never ran first (as after
  leaving safe preview without a run), so they don't fail on names those
  cells define. The app's approval card (`endeavor/run_preview`) shows these
  (`needed_ids`), the dependents that rerun with them, and the packages a
  whole-notebook run loads. For `allow_execution`, `count` is the notebook's
  size whether or not it runs.
- The receipt shows the cells that ran: the ones asked for, the stale or
  staged ancestors the engine ran first (#187), and the dependents autorun
  reran (#192). These are known from what ran once the run is over, so they
  miss a cell still running when the wait ends and count one the user ran
  meanwhile. A run not waited for lists the dependents it will rerun.
- Dependents are the cells the graph links to the run's cells, plus those the
  engine says it reruns: Ember reruns the readers of a name a cell's last run
  defined, which the graph no longer links once the cell drops the name
  (#228). Ember skips a dependent that has never run; in lazy mode they go
  stale instead, and the receipt lists every stale cell (#196). Ember never
  runs a disabled cell or one downstream of it (`graph$off`), though the
  graph still links them, so a run doesn't name them and a read of one
  doesn't wait for it (#232).
- A run whose targets the user's run already reached while the approval card
  waited isn't run again; its receipt says so. Cells waiting for the user's
  OK to a package don't run by themselves (#355).
- Cells that only ran along (dependents and stale ancestors: `Ran`'s `along`)
  get shorter output text and leave out the quiet notes (`QUIET_NOTES`): a
  run of a cell many others read sent back each one's table, most of a long
  session's run replies. An error isn't cut.
- A blocked run touched no cell. A cell still running or queued shows its
  last run's output, so it isn't listed (#190).
- The notebook's order isn't in a change's reply: two lists of every cell's
  id grow with the notebook. An agent that needs it calls `get_cell_order` or
  `get_execution_order`.

## Output

- `OUTPUT_MAX` (16,000 bytes) of a text output come back, `OUTPUT_END` of
  them from its end, where a long print's result is; a `Text` of 200,000
  lines once came back whole, 1.5 MB. A cell that only ran along gets
  `DEPENDENT_TEXT_MAX`, and a change's receipt `RECEIPT_TEXT_MAX`. The rest is
  out of every tool's reach (`OUTPUT_REST`).
- While an edited cell is running, queued, or staged, its output and error
  are from before the edit, so they come as `previous_output`,
  `previous_errored` and `previous_error` (#190, #435). A note on the code is
  about the code just sent, so it stays in `notes` (#449). An output from
  before a change is called that, or an agent takes an old plot for its new
  code's.
- An image over `MAX_IMAGE_BYTES` (4 MB) isn't sent: Claude takes about 5 MB.
- `view_cell_output` says what the agent may claim about a picture. An
  interactive output (scripts or controls, `_interactive_html` in
  `Output.jl`) gets `INTERACTIVE_UNSEEN` or `INTERACTIVE_PICTURE`; any picture
  without controls gets `STATIC_PICTURE` (#407). The skill's text alone didn't
  stop Haiku claiming hover and zoom worked, or switching packages to get a
  picture (#393).
- When the runtime has no PNG of an output, no worker, or a busy one (#62), a
  picture of the notebook (`guard::Snapshot`) can stand in.

## Notes and warnings

- The notes Ember shows under a cell (a value changed in place whose readers
  don't rerun, #207) and what a Pluto cell printed or logged (a `@warn` that
  rows were dropped, #257) come back as `warnings`, "kind::Cell <id>: text",
  one line for the same note on several cells. Logged errors come first, then
  warnings and Ember's notes, then the quiet ones, at most `RUN_NOTES_MAX`
  lines and `RUN_NOTES_BYTES`.
- The first logged warning or error is also quoted in the run's `message`:
  Haiku 4.5 read one in `warnings` and reported the result as clean (N11). So
  is a result made at a control the user moved on the page, which a rerun of
  the file wouldn't give.
- Notes that come from a cell's code rather than its run (`CODE_NOTES`) stay
  `notes` while an edit waits to run; the rest become `previous_notes`.
- `open_notebook`'s note on safe preview is said again by
  `read_notebook_code`: Haiku 4.5 read the code after the note and described
  it as if it had run (#163).
- A Pluto cell that fails with "Multiple definitions for x" also names the
  other cells that define x (#242). Ember's error has the same words, but the
  advice doesn't fit R.
- An R cell whose package Ember couldn't install gets `INSTALL_FAILED` (#390,
  #394): Ember's error is `missing_package`, whose usual fix is a rerun, and
  Haiku restarted R, installed from a cell, and answered from memory.
- A notebook process that ends during a waited run is reported in the words
  of the app's crash page.

## Packages

- A run that needs a package the notebook doesn't have waits for the user's
  OK: an R install before a large download (Ember #37), a Julia package
  (#397), a Python import (#355). The agent is told to ask and end its reply
  (`ASK_STEP`); the plugin's front puts its own words there where it asks the
  user itself (`standalone::consent`).
- `approve_install` is the user's yes, and the tool says to call it only after
  they said yes. It is refused when nothing waits. Where the yes named
  packages, only those are approved and the rest ask again (#418). A yes that
  approved nothing started nothing.
- A Python package name a letter or two from a common one is flagged as a
  likely typo (#416), only as a hint: real packages sit close to common ones
  (pysam and pyyaml).
- Ember's notice that packages build from source (Ember #19) is passed on as
  its Packages tab says it.
- R's snapshot date (#201): `preview_package_update` shows what moving it would
  change, waited for within the call. A day not published yet is previewed at
  the newest one that is (#247). With `package`, the date is Ember's
  `preview_update` for that package (#266), so the others move as little as they can. Applying is
  refused unless that preview is ready and current, and moves the date even
  with no packages yet (#269). A Python notebook's date is its
  `exclude-newer` (#352). In safe preview neither Ember nor marimo installs.

## Opening, new notebooks, safe preview

- A session that may work on several notebooks can't say which is the
  conversation's (`allow_several`). Closing its own notebook doesn't free the
  session for another, and opening it again works as any notebook (#520).
- A plain script isn't opened: Pluto would leave a backup copy beside it
  before refusing it.
- A call made again while an open is under way gets that open. Opening an
  open notebook runs nothing and leaves its safe preview as it was.
- An open's run of the whole notebook, `allow_execution`'s, and a restart's
  are the session's (#170): a read or a waited submit waits for them (#331).
  Pluto marks every cell queued as it opens and names no run, so the run goes
  on while one of its cells hasn't finished. Each waits for the user's OK to
  a new package (#397). `allow_execution` with `run_notebook=false` only lifts
  safe preview; it starts a new process that can outlast the call, and a call
  made again gets its result.
- A new notebook goes in the session's folder, named as the engine names one,
  in the session's language when its client gave it. A new notebook without a
  path is the session's. A read-only folder (a computer's files shared
  read-only into a virtual machine) is refused before the engine fails to
  save, saying where to make it instead; this is checked only on Unix. R and
  Python notebooks aren't offered on Windows yet.
- An agent that names a notebook that isn't open is told the open ones, by id
  and path, a few at most (#274).

## Stop, restart, close, prompts and inputs

- `interrupt_notebook` is the page's Stop (#161), whoever started the run.
  Pluto's returns once the run has stopped; Ember's sends SIGINT and returns,
  so the cells are watched for up to `INTERRUPT_WAIT_SECONDS` (10 s). A cell
  still running then is usually in compiled code that ignores interrupts;
  Ember offers its restart 3 s after a Stop. If the process is still
  starting, its queued cells are dropped (#179). An R notebook marks stale
  the cells the run didn't get to, which keep the earlier output, so the
  reply lists every stale cell (#260).
- `restart_notebook` is the page's Restart (#202). Pluto then runs every cell,
  so a cell that was running runs again (`running_again`): a restart meant to
  end a stuck cell doesn't, unless the cell was edited first. Ember's leaves
  every cell not run. Engines refuse it in safe preview.
- `close_notebook` is the app's "Stop notebook": the process ends for every
  session and the user's browser. Pluto saved every change, so only values
  are lost.
- `answer_prompt` answers the prompt a running cell waits on (#261) and is
  waited for as a run is. An answer to a question that has gone isn't given
  to the next one.
- `set_input` moves an R notebook's input as its widget does (Ember's
  `set_input()`): readers rerun in autorun, go stale in lazy mode, and the
  value isn't saved. R reads it only once its running cell finishes, so one
  not read by the wait's end is reported as on its way, not refused.
